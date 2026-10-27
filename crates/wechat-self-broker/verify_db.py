from __future__ import annotations

import argparse
import hashlib
import hmac
import struct
import sys

PAGE_SIZE = 4096
SALT_SIZE = 16
ROUNDS = 256_000
IV_SIZE = 16
HMAC_SIZE = 64
KEY_SIZE = 32

def _force_utf8() -> None:
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass

_force_utf8()

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("db")
    ap.add_argument("--key-hex")
    ap.add_argument("--key-file")
    ap.add_argument("--internal-key-hex")
    args = ap.parse_args()

    if args.key_hex:
        try:
            key = bytes.fromhex(args.key_hex.strip())
        except ValueError:
            print("FAIL: key-hex 不是合法十六进制")
            return 1
    elif args.key_file:
        try:
            key = bytes.fromhex(open(args.key_file, encoding="ascii").read().strip())
        except (OSError, ValueError) as exc:
            print(f"FAIL: 读取 key 失败: {exc}")
            return 1
    else:
        print("FAIL: 需要 --key-hex 或 --key-file")
        return 1

    if len(key) != KEY_SIZE:
        print(f"FAIL: key 必须是 {KEY_SIZE} 字节")
        return 1
    if args.internal_key_hex:
        try:
            internal = bytes.fromhex(args.internal_key_hex.strip())
        except ValueError:
            print("FAIL: internal-key-hex 非法")
            return 1
        if len(internal) != KEY_SIZE:
            print("FAIL: internal key 必须是 32 字节")
            return 1
        key = bytes(a ^ b for a, b in zip(key, internal))

    try:
        with open(args.db, "rb") as fh:
            data = fh.read(PAGE_SIZE)
    except OSError as exc:
        print(f"FAIL: 无法读取数据库: {exc}")
        return 1
    if len(data) < PAGE_SIZE:
        print("FAIL: 文件不足一页")
        return 1

    salt = data[:SALT_SIZE]
    mac_salt = bytes(b ^ 0x3A for b in salt)
    new_key = hashlib.pbkdf2_hmac("sha512", key, salt, ROUNDS, KEY_SIZE)
    mac_key = hashlib.pbkdf2_hmac("sha512", new_key, mac_salt, 2, KEY_SIZE)

    reserve = ((IV_SIZE + HMAC_SIZE + 15) // 16) * 16
    start = SALT_SIZE
    end = PAGE_SIZE
    mac = hmac.new(
        mac_key,
        data[start : end - reserve + IV_SIZE] + struct.pack("<I", 1),
        hashlib.sha512,
    ).digest()
    offset = end - reserve + IV_SIZE
    actual = data[offset : offset + HMAC_SIZE]
    ok = hmac.compare_digest(mac, actual)
    print("OK: key 与数据库匹配" if ok else "FAIL: key 不匹配")
    return 0 if ok else 1

if __name__ == "__main__":
    raise SystemExit(main())
