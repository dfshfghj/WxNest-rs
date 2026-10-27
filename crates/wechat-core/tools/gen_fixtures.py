from __future__ import annotations

import ctypes
import ctypes.util
import hashlib
import hmac
import json
import os
import shutil
import sqlite3
import struct
import sys
from pathlib import Path

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures"
PY_SRC = Path(
    os.environ.get(
        "WECHAT_PY_SRC",
        ROOT.parents[2] / "WeChatDataAnalysis" / "src",
    )
)
sys.path.insert(0, str(PY_SRC))

from wechat_decrypt_tool import image_key_resolver, isaac64, sqlite_wal
from wechat_decrypt_tool.wechat_decrypt import (
    PAGE_SIZE,
    SQLITE_HEADER,
    WeChatDatabaseDecryptor,
)

PAGE = PAGE_SIZE
SALT = bytes(range(16))
IV_SIZE = 16
HMAC_SIZE = 64
RESERVE = 80

def ensure_dir(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    return path

def write(path: Path, data: bytes) -> None:
    ensure_dir(path.parent)
    path.write_bytes(data)

def aes_cbc_encrypt(key: bytes, iv: bytes, data: bytes) -> bytes:
    encryptor = Cipher(algorithms.AES(key), modes.CBC(iv)).encryptor()
    return encryptor.update(data) + encryptor.finalize()

def aes_ecb_encrypt(key: bytes, data: bytes) -> bytes:
    encryptor = Cipher(algorithms.AES(key), modes.ECB()).encryptor()
    return encryptor.update(data) + encryptor.finalize()

def pkcs7_pad(data: bytes, block: int = 16) -> bytes:
    padding = block - len(data) % block
    return data + bytes([padding]) * padding

def pbkdf2(password: bytes, salt: bytes, rounds: int, length: int = 32) -> bytes:
    return hashlib.pbkdf2_hmac("sha512", password, salt, rounds, dklen=length)

def page_hmac(mac_key: bytes, page: bytes, page_num: int) -> bytes:
    offset = 16 if page_num == 1 else 0
    data_end = PAGE - RESERVE + IV_SIZE
    mac = hmac.new(mac_key, digestmod=hashlib.sha512)
    mac.update(page[offset:data_end])
    mac.update(page_num.to_bytes(4, "little"))
    return mac.digest()

def make_plain_db(path: Path) -> bytes:

    if path.exists():
        path.unlink()

    library = ctypes.CDLL(ctypes.util.find_library("sqlite3"))
    library.sqlite3_open.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)]
    library.sqlite3_open.restype = ctypes.c_int
    library.sqlite3_file_control.argtypes = [
        ctypes.c_void_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_void_p,
    ]
    library.sqlite3_file_control.restype = ctypes.c_int
    library.sqlite3_exec.argtypes = [
        ctypes.c_void_p,
        ctypes.c_char_p,
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_char_p),
    ]
    library.sqlite3_exec.restype = ctypes.c_int
    library.sqlite3_close.argtypes = [ctypes.c_void_p]
    library.sqlite3_close.restype = ctypes.c_int

    database = ctypes.c_void_p()
    if library.sqlite3_open(os.fspath(path).encode(), ctypes.byref(database)) != 0:
        raise RuntimeError("sqlite3_open failed")
    try:
        reserve = ctypes.c_int(80)

        if library.sqlite3_file_control(database, b"main", 38, ctypes.byref(reserve)) != 0:
            raise RuntimeError("sqlite3_file_control(reserve) failed")

        def execute(sql: str) -> None:
            error = ctypes.c_char_p()
            code = library.sqlite3_exec(
                database, sql.encode(), None, None, ctypes.byref(error)
            )
            if code != 0:
                message = error.value.decode(errors="replace") if error.value else str(code)
                raise RuntimeError(message)

        execute("PRAGMA page_size=4096")
        execute("PRAGMA journal_mode=DELETE")
        execute("CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT NOT NULL)")
        for index in range(1, 80):
            body = ("hello-%04d-" % index) * 40
            execute(f"INSERT INTO messages (id, body) VALUES ({index}, '{body}')")
    finally:
        library.sqlite3_close(database)

    data = path.read_bytes()
    if len(data) % PAGE:
        data += b"\0" * (PAGE - len(data) % PAGE)
        path.write_bytes(data)
    return data

def wal_checksum(data: bytes, state: tuple[int, int] = (0, 0)) -> tuple[int, int]:
    a, b = state
    for index in range(0, len(data), 8):
        x, y = struct.unpack_from("<II", data, index)
        a = (a + x + b) & 0xFFFFFFFF
        b = (b + y + a) & 0xFFFFFFFF
    return a, b

def build_wal(entries: list[tuple[int, bytes]], page_size: int, salt: bytes, db_size: int) -> bytes:
    wal = bytearray()
    wal.extend(struct.pack(">III", 0x377F0682, 3007000, page_size))
    wal.extend(struct.pack(">I", 2))
    wal.extend(salt)
    wal.extend(b"\0" * 8)
    state = wal_checksum(bytes(wal[:24]))
    wal[24:32] = struct.pack(">II", *state)

    for index, (pgno, page) in enumerate(entries):
        commit_size = db_size if index == len(entries) - 1 else 0
        header = bytearray()
        header.extend(struct.pack(">II", pgno, commit_size))
        header.extend(salt)
        header.extend(b"\0" * 8)
        state = wal_checksum(bytes(header[:8]), state)
        state = wal_checksum(page, state)
        header[16:24] = struct.pack(">II", *state)
        wal.extend(header)
        wal.extend(page)
    return bytes(wal)

def encrypt_database(plain: bytes, key_material: bytes, mode: str) -> bytes:
    if mode == "raw":
        enc_key = key_material
    elif mode == "passphrase":
        enc_key = pbkdf2(key_material, SALT, 256000)
    else:
        raise ValueError(mode)
    mac_key = pbkdf2(enc_key, bytes(value ^ 0x3A for value in SALT), 2)
    output = bytearray()
    for page_index in range(len(plain) // PAGE):
        page_num = page_index + 1
        page_plain = plain[page_index * PAGE : (page_index + 1) * PAGE]
        offset = 16 if page_num == 1 else 0
        ciphertext = aes_cbc_encrypt(
            enc_key, bytes([page_num % 256]) * IV_SIZE, page_plain[offset : PAGE - RESERVE]
        )
        page = bytearray()
        if page_num == 1:
            page.extend(SALT)
        page.extend(ciphertext)
        page.extend(bytes([page_num % 256]) * IV_SIZE)
        page.extend(b"\0" * HMAC_SIZE)
        page[-HMAC_SIZE:] = page_hmac(mac_key, bytes(page), page_num)
        assert len(page) == PAGE
        output.extend(page)
    return bytes(output)

def build_sqlcipher_fixtures() -> None:
    directory = ensure_dir(FIXTURES / "sqlcipher")
    plain = make_plain_db(directory / "plain.db")
    key_material = bytes(range(1, 33))
    fixtures = {}
    for mode in ("raw", "passphrase"):
        encrypted = encrypt_database(plain, key_material, mode)
        encrypted_path = directory / f"encrypted_{mode}.db"
        expected_path = directory / f"expected_{mode}.db"
        write(encrypted_path, encrypted)
        decryptor = WeChatDatabaseDecryptor(key_material.hex())
        ok = decryptor.decrypt_database(str(encrypted_path), str(expected_path))
        result = dict(decryptor.last_result)
        fixtures[mode] = {
            "success": bool(ok and result.get("success")),
            "key_mode": result.get("key_mode"),
            "total_pages": result.get("total_pages"),
            "successful_pages": result.get("successful_pages"),
            "failed_pages": result.get("failed_pages"),
            "hmac_warning_pages": result.get("hmac_warning_pages"),
            "copied_as_sqlite": result.get("copied_as_sqlite"),
            "diagnostic_status": result.get("diagnostic_status"),
            "expected_sha256": hashlib.sha256(expected_path.read_bytes()).hexdigest(),
        }

    wrong = WeChatDatabaseDecryptor(("ff" * 32))
    wrong_path = directory / "wrong_output.db"
    wrong_ok = wrong.decrypt_database(str(directory / "encrypted_raw.db"), str(wrong_path))
    wrong_result = dict(wrong.last_result)
    fixtures["wrong_key"] = {
        "success": bool(wrong_ok or wrong_result.get("success")),
        "error": wrong_result.get("error"),
        "failed_pages": wrong_result.get("failed_pages"),
        "key_mode": wrong_result.get("key_mode"),
    }

    plain_updated_path = directory / "plain_updated.db"
    shutil.copyfile(directory / "plain.db", plain_updated_path)
    connection = sqlite3.connect(plain_updated_path)
    connection.execute("UPDATE messages SET body = body || '-updated' WHERE id = 2")
    connection.commit()
    connection.close()
    plain_updated = plain_updated_path.read_bytes()
    encrypted_base = encrypt_database(plain, key_material, "raw")
    encrypted_updated = encrypt_database(plain_updated, key_material, "raw")
    changed_pages = [
        index + 1
        for index in range(len(plain) // PAGE)
        if encrypted_updated[index * PAGE : (index + 1) * PAGE]
        != encrypted_base[index * PAGE : (index + 1) * PAGE]
    ]
    modified_page = next((page for page in changed_pages if page > 1), changed_pages[-1])
    wal = build_wal(
        [(modified_page, encrypted_updated[(modified_page - 1) * PAGE : modified_page * PAGE])],
        PAGE,
        bytes([9, 8, 7, 6, 5, 4, 3, 2]),
        len(plain) // PAGE,
    )
    encrypted_wal_path = directory / "encrypted_wal.db"
    write(encrypted_wal_path, encrypted_base)
    write(Path(str(encrypted_wal_path) + "-wal"), wal)
    expected_wal_path = directory / "expected_wal.db"
    wal_decryptor = WeChatDatabaseDecryptor(key_material.hex())
    wal_ok = wal_decryptor.decrypt_database(str(encrypted_wal_path), str(expected_wal_path))
    wal_result = dict(wal_decryptor.last_result)
    fixtures["wal"] = {
        "success": bool(wal_ok and wal_result.get("success")),
        "key_mode": wal_result.get("key_mode"),
        "total_pages": wal_result.get("total_pages"),
        "successful_pages": wal_result.get("successful_pages"),
        "failed_pages": wal_result.get("failed_pages"),
        "hmac_warning_pages": wal_result.get("hmac_warning_pages"),
        "modified_page": modified_page,
        "wal": wal_result.get("wal"),
        "expected_sha256": hashlib.sha256(expected_wal_path.read_bytes()).hexdigest(),
    }
    plain_updated_path.unlink(missing_ok=True)

    (directory / "expected.json").write_text(
        json.dumps(fixtures, ensure_ascii=False, indent=2, sort_keys=True), encoding="utf-8"
    )

def build_wal_fixtures() -> None:
    directory = ensure_dir(FIXTURES / "wal")
    base_path = directory / "base.db"
    plain = make_plain_db(base_path)
    page_count = len(plain) // PAGE
    page_number = max(2, page_count)
    page = bytearray(plain[(page_number - 1) * PAGE : page_number * PAGE])
    page[0] ^= 0x5A

    wal = bytearray()
    wal.extend(struct.pack(">III", 0x377F0682, 3007000, PAGE))
    wal.extend(struct.pack(">I", 1))
    salt = bytes([11, 22, 33, 44, 55, 66, 77, 88])
    wal.extend(salt)
    wal.extend(b"\0" * 8)

    def checksum(data: bytes, state: tuple[int, int] = (0, 0)) -> tuple[int, int]:
        a, b = state
        for index in range(0, len(data), 8):
            x, y = struct.unpack_from("<II", data, index)
            a = (a + x + b) & 0xFFFFFFFF
            b = (b + y + a) & 0xFFFFFFFF
        return a, b

    state = checksum(bytes(wal[:24]))
    wal[24:32] = struct.pack(">II", *state)

    header = bytearray()
    header.extend(struct.pack(">II", page_number, page_count))
    header.extend(salt)
    header.extend(b"\0" * 8)
    frame_state = checksum(bytes(header[:8]), state)
    frame_state = checksum(bytes(page), frame_state)
    header[16:24] = struct.pack(">II", *frame_state)
    wal.extend(header)
    wal.extend(page)

    expected, info = sqlite_wal.merge_wal_snapshot(plain, bytes(wal), PAGE)
    write(directory / "wal.bin", bytes(wal))
    write(directory / "expected.db", expected)
    (directory / "expected.json").write_text(
        json.dumps(
            {
                "page_size": PAGE,
                "database_pages": info["database_pages"],
                "committed_frames": info["committed_frames"],
                "applied_pages": info["applied_pages"],
                "ignored_tail_bytes": info["ignored_tail_bytes"],
                "expected_sha256": hashlib.sha256(expected).hexdigest(),
                "modified_page": page_number,
            },
            indent=2,
            sort_keys=True,
        ),
        encoding="utf-8",
    )

def build_dat_v4(aes_plain: bytes, raw: bytes, tail: bytes, xor_key: int, aes_key: bytes, magic: bytes) -> bytes:
    padded = pkcs7_pad(aes_plain)
    header = magic + struct.pack("<LL", len(padded) - 16, len(tail)) + b"\0"
    return header + aes_ecb_encrypt(aes_key, padded) + raw + bytes(value ^ xor_key for value in tail)

def build_media_fixtures() -> None:
    directory = ensure_dir(FIXTURES / "media")
    xor_key = 0x5A
    plain = b"hello wechat dat v4 payload" * 4
    fixtures = {}

    v3 = bytes(value ^ xor_key for value in plain)
    write(directory / "dat_v3.bin", v3)
    write(directory / "dat_v3_expected.bin", plain)
    fixtures["dat_v3"] = {"xor_key": xor_key, "expected_sha256": hashlib.sha256(plain).hexdigest()}

    for name, magic, aes_key in (
        ("dat_v4_v1", b"\x07\x08V1\x08\x07", b"cfcd208495d565ef"),
        ("dat_v4_v2", b"\x07\x08V2\x08\x07", b"0123456789abcdef"),
    ):
        aes_plain = plain + b"-aes"
        raw = b"-raw-"
        tail = b"-tail"
        data = build_dat_v4(aes_plain, raw, tail, xor_key, aes_key, magic)
        expected = aes_plain + raw + tail
        write(directory / f"{name}.bin", data)
        write(directory / f"{name}_expected.bin", expected)
        fixtures[name] = {
            "xor_key": xor_key,
            "aes_key": aes_key.decode("ascii"),
            "expected_sha256": hashlib.sha256(expected).hexdigest(),
        }

    png = (
        b"\x89PNG\r\n\x1a\n"
        + b"\x00\x00\x00\rIHDR"
        + b"\x00" * 13
        + b"\x00\x00\x00\x00IEND\xaeB`\x82"
    )
    xor_png = bytes(value ^ xor_key for value in png)
    write(directory / "xor_png.bin", xor_png)
    write(directory / "xor_png_expected.bin", png)
    fixtures["xor_png"] = {
        "xor_key": xor_key,
        "media_type": "image/png",
        "expected_sha256": hashlib.sha256(png).hexdigest(),
    }

    emoticon_key = b"0123456789abcdef"
    emoticon_plain = b"emoticon payload"
    emoticon_padded = pkcs7_pad(emoticon_plain)
    emoticon_encrypted = aes_cbc_encrypt(emoticon_key, emoticon_key, emoticon_padded)
    write(directory / "emoticon.bin", emoticon_encrypted)
    write(directory / "emoticon_expected.bin", emoticon_plain)
    fixtures["emoticon"] = {
        "aes_key_hex": emoticon_key.hex(),
        "expected_sha256": hashlib.sha256(emoticon_plain).hexdigest(),
    }

    (directory / "expected.json").write_text(
        json.dumps(fixtures, indent=2, sort_keys=True), encoding="utf-8"
    )

def build_image_key_fixtures() -> None:
    directory = ensure_dir(FIXTURES / "image_key")
    account_dir = ensure_dir(directory / "account")
    kvcomm_dir = ensure_dir(directory / "kvcomm")
    img_dir = ensure_dir(account_dir / "msg" / "attach" / "abc" / "2024-01" / "img")

    code = 0x12345678
    wxid = "wxid_test_1234"
    keys = image_key_resolver.derive_image_keys(code, wxid)
    jpeg_block = b"\xff\xd8\xff\xe0" + b"\0" * 12
    ciphertext = aes_ecb_encrypt(keys.aes_key.encode("ascii"), jpeg_block)
    tail = bytes([0xFF ^ keys.xor_key, 0xD9 ^ keys.xor_key])
    template = image_key_resolver.V2_MAGIC + b"\0" * 9 + ciphertext + tail
    write(img_dir / "template_t.dat", template)
    (kvcomm_dir / f"key_{code}_foo.statistic").write_bytes(b"")

    resolution = image_key_resolver.resolve_local_image_key(
        kvcomm_dir=kvcomm_dir,
        account_dir=account_dir,
        target_wxid=wxid,
    )
    if resolution is None:
        raise RuntimeError("Python image-key resolver did not resolve the fixture")
    payload = resolution.as_dict()
    payload["template_path"] = Path(payload["template_path"]).name
    (directory / "expected.json").write_text(
        json.dumps(payload, indent=2, sort_keys=True), encoding="utf-8"
    )

def build_isaac_fixtures() -> None:
    directory = ensure_dir(FIXTURES / "isaac64")
    fixtures = []
    for seed, size, word_format in (
        ("12345", 64, "be_swap32"),
        ("0x1234", 24, "raw_le"),
        ("", 8, "raw_be"),
    ):
        stream = isaac64.Isaac64(seed).generate_keystream(size, word_format=word_format)
        fixtures.append(
            {
                "seed": seed,
                "size": size,
                "format": word_format,
                "expected_hex": stream.hex(),
            }
        )
    (directory / "expected.json").write_text(
        json.dumps(fixtures, indent=2, sort_keys=True), encoding="utf-8"
    )

def main() -> None:
    if FIXTURES.exists():
        shutil.rmtree(FIXTURES)
    ensure_dir(FIXTURES)
    build_sqlcipher_fixtures()
    build_wal_fixtures()
    build_media_fixtures()
    build_image_key_fixtures()
    build_isaac_fixtures()
    print(f"fixtures written to {FIXTURES}")

if __name__ == "__main__":
    main()
