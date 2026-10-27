from __future__ import annotations

import hashlib
import json
import os
import shutil
import sqlite3
import sys
import tempfile
from pathlib import Path
from typing import Any

import zstandard as zstd

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures"
ACCOUNTS_ROOT = FIXTURES / "accounts"
PY_SRC = Path(
    os.environ.get(
        "WECHAT_PY_SRC",
        ROOT.parents[2] / "WeChatDataAnalysis" / "src",
    )
)
sys.path.insert(0, str(PY_SRC))

from wechat_decrypt_tool.chat_helpers import (
    _decode_message_content,
    _extract_md5_from_blob,
    _extract_md5_from_packed_info,
    _extract_voice_transcript_from_packed_info,
    _parse_app_message,
    _load_group_nickname_map_from_contact_db,
    _parse_location_message,
    _parse_pat_message,
    _parse_system_message_content,
    _pick_display_name,
    _should_keep_session,
)
from wechat_decrypt_tool import chat_search_index
from wechat_decrypt_tool.chat_helpers import _build_fts_query
from wechat_decrypt_tool.media_helpers import MediaPathIndex

SELF = "wxid_self0001"
FRIEND1 = "wxid_friend01"
FRIEND2 = "wxid_friend02"
FRIEND3 = "wxid_friend03"
STRANGER = "wxid_stranger01"
GROUP = "123456@chatroom"
OFFICIAL = "gh_official001"
FILEHELPER = "filehelper"
HIDDEN = "wxid_hidden01"
SERVICE = "service_abc"

SHARD0 = "message_0.db"
SHARD1 = "message_1.db"

def md5_hex(value: str) -> str:
    return hashlib.md5(value.encode("utf-8")).hexdigest()

def msg_table(username: str, prefix: str = "msg") -> str:
    return f"{prefix}_{md5_hex(username)}"

def zstd_bytes(value: str) -> bytes:
    return zstd.ZstdCompressor().compress(value.encode("utf-8"))

def connect(path: Path) -> sqlite3.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.unlink()
    connection = sqlite3.connect(path)
    connection.execute("PRAGMA journal_mode=DELETE")
    return connection

def create_message_table(connection: sqlite3.Connection, table: str, *, minimal: bool = False) -> None:
    if minimal:
        connection.execute(
            f'CREATE TABLE "{table}" ('
            "local_id INTEGER PRIMARY KEY, local_type INTEGER, real_sender_id INTEGER, "
            "create_time INTEGER, message_content BLOB)"
        )
        return
    connection.execute(
        f'CREATE TABLE "{table}" ('
        "local_id INTEGER PRIMARY KEY, server_id INTEGER, local_type INTEGER, "
        "sort_seq INTEGER, real_sender_id INTEGER, create_time INTEGER, "
        "message_content BLOB, compress_content BLOB)"
    )

def build_account_files(account_dir: Path) -> None:
    account_dir.mkdir(parents=True, exist_ok=True)
    (account_dir / "account.json").write_text(
        json.dumps({"username": SELF}, ensure_ascii=False), encoding="utf-8"
    )

def build_session_db(account_dir: Path) -> list[dict[str, Any]]:
    path = account_dir / "session.db"
    connection = connect(path)
    connection.execute(
        "CREATE TABLE SessionTable ("
        "username TEXT, is_hidden INTEGER, summary BLOB, draft TEXT, "
        "last_msg_type INTEGER, last_msg_sub_type INTEGER, "
        "sort_timestamp INTEGER, last_timestamp INTEGER)"
    )
    group_xml = "<msg><appmsg>群消息</appmsg></msg>"
    rows = [
        (FRIEND1, 0, "你好 session", "", 1, 0, 1_700_000_300, 1_700_000_300),
        (GROUP, 0, zstd_bytes(group_xml).hex(), "draft text", 49, 6, 1_700_000_200, 1_700_000_200),
        (FRIEND2, 0, "a &amp; b", None, 3, 0, 1_700_000_100, 1_700_000_100),
        (OFFICIAL, 0, "官方消息", "", 1, 0, 1_700_000_050, 1_700_000_050),
        (FILEHELPER, 0, "文件传输助手", "", 1, 0, 1_700_000_400, 1_700_000_400),
        ("notifymessage", 0, "notify", "", 1, 0, 1_700_000_900, 1_700_000_900),
        (HIDDEN, 1, "悄悄话", "", 1, 0, 1_700_000_500, 1_700_000_500),
        (SERVICE, 0, "客服", "", 1, 0, 1_700_000_600, 1_700_000_600),
        (FRIEND3, 0, b"bad\xffsummary", "", 1, 0, 1_700_000_000, 1_700_000_000),
    ]
    connection.executemany(
        "INSERT INTO SessionTable VALUES (?,?,?,?,?,?,?,?)", rows
    )
    connection.commit()
    connection.close()
    return [
        {
            "username": row[0],
            "is_hidden": bool(row[1]),
            "summary": row[2],
            "draft": row[3],
            "last_msg_type": row[4],
            "last_msg_sub_type": row[5],
            "sort_timestamp": row[6],
            "last_timestamp": row[7],
        }
        for row in rows
    ]

def pb_varint(value: int) -> bytes:
    output = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            output.append(byte | 0x80)
        else:
            output.append(byte)
            return bytes(output)

def pb_string(field_no: int, value: str) -> bytes:
    payload = value.encode("utf-8")
    return pb_varint((field_no << 3) | 2) + pb_varint(len(payload)) + payload

def pb_member(username: str, nickname: str) -> bytes:
    payload = pb_string(1, username) + pb_string(2, nickname)
    return pb_varint((1 << 3) | 2) + pb_varint(len(payload)) + payload

GROUP_MEMBERS = [
    ("wxid_groupmember", "群昵称甲"),
    (SELF, "我自己"),
    (STRANGER, "陌生人昵称"),
    ("wxid_other", "其他人"),
]

def build_contact_db(account_dir: Path) -> dict[str, dict[str, Any]]:
    path = account_dir / "contact.db"
    connection = connect(path)
    schema = (
        "username TEXT, remark TEXT, nick_name TEXT, alias TEXT, "
        "big_head_url TEXT, small_head_url TEXT"
    )
    connection.execute(f"CREATE TABLE contact ({schema})")
    connection.execute(f"CREATE TABLE stranger ({schema})")
    contact_rows = [
        (FRIEND1, "张三", "ZS", "zhangsan", "http://example.com/zhang.jpg", ""),
        (FRIEND2, "", "李四", "", "", "https://example.com/li.jpg"),
        (GROUP, "开发群", "群聊", "", "http://example.com/group.jpg", ""),
        (FILEHELPER, "文件传输助手", "", "", "", ""),
        (OFFICIAL, "", "官方号", "", "http://example.com/official.jpg", ""),
        ("", "空用户名", "跳过", "", "", ""),
        (HIDDEN, "", "隐藏联系人", "", "", ""),
    ]
    connection.executemany(
        "INSERT INTO contact VALUES (?,?,?,?,?,?)", contact_rows
    )
    stranger_rows = [
        (STRANGER, "陌生人备注", "陌生", "", "", ""),
        (FRIEND1, "不应覆盖", "覆盖", "", "", ""),
    ]
    connection.executemany(
        "INSERT INTO stranger VALUES (?,?,?,?,?,?)", stranger_rows
    )
    connection.execute("CREATE TABLE chat_room (username TEXT, ext_buffer BLOB)")
    ext_buffer = b"".join(pb_member(username, nickname) for username, nickname in GROUP_MEMBERS)
    connection.execute(
        "INSERT INTO chat_room VALUES (?, ?)", (GROUP, ext_buffer)
    )
    connection.commit()
    connection.close()

    expected: dict[str, dict[str, Any]] = {}
    for table, rows in (("contact", contact_rows), ("stranger", stranger_rows)):
        for row in rows:
            item = {
                "username": (row[0] or "").strip(),
                "remark": (row[1] or "").strip(),
                "nick_name": (row[2] or "").strip(),
                "alias": (row[3] or "").strip(),
                "big_head_url": row[4] or None,
                "small_head_url": row[5] or None,
            }
            if not item["username"] or item["username"] in expected:
                continue
            item["display_name"] = _pick_display_name(
                {
                    "remark": item["remark"],
                    "nick_name": item["nick_name"],
                    "alias": item["alias"],
                },
                item["username"],
            )
            item["is_group"] = item["username"].endswith("@chatroom")
            item["is_official"] = item["username"].startswith("gh_")
            expected[item["username"]] = item
    return expected

def build_head_image_db(account_dir: Path) -> set[str]:
    path = account_dir / "head_image.db"
    connection = connect(path)
    connection.execute("CREATE TABLE head_image (username TEXT, image BLOB)")
    users = [FRIEND1, GROUP, FILEHELPER, "wxid_nobody"]
    connection.executemany(
        "INSERT INTO head_image VALUES (?, ?)", [(user, b"img") for user in users]
    )
    connection.commit()
    connection.close()
    return set(users)

MESSAGE_SCHEMA = (
    "local_id INTEGER PRIMARY KEY, server_id INTEGER, local_type INTEGER, "
    "sort_seq INTEGER, real_sender_id INTEGER, create_time INTEGER, "
    "message_content BLOB, compress_content BLOB"
)

def name2id_rows() -> list[tuple[int, str]]:
    return [
        (1, SELF),
        (2, FRIEND1),
        (3, FRIEND2),
        (4, "wxid_groupmember"),
        (5, STRANGER),
    ]

def build_message_shard0(account_dir: Path) -> None:
    path = account_dir / SHARD0
    connection = connect(path)
    connection.execute("CREATE TABLE Name2Id (user_name TEXT)")
    connection.executemany(
        "INSERT INTO Name2Id (rowid, user_name) VALUES (?, ?)", name2id_rows()
    )

    friend_table = msg_table(FRIEND1)
    create_message_table(connection, friend_table)
    app_xml = "<msg><appmsg>link</appmsg></msg>"
    compressed_xml = "<msg><appmsg>compressed</appmsg></msg>"
    location_xml = (
        '<msg><location x="31.2304" y="121.4737" scale="16" '
        'label="上海市黄浦区人民广场" poiname="人民广场" maptype="0"/></msg>'
    )
    quote_xml = (
        "<msg><appmsg appid=\"\" sdkver=\"0\"><title>回复的标题</title><type>57</type>"
        "<refermsg><type>1</type><displayname>张三</displayname>"
        "<fromusr>wxid_friend01</fromusr><svrid>999</svrid>"
        "<content><![CDATA[被引用的内容]]></content></refermsg></appmsg></msg>"
    )
    pat_xml = (
        '<sysmsg type="patmsg"><patmsg><template>${wxid_friend01}拍了拍'
        "${wxid_self0001}</template></patmsg></sysmsg>"
    )
    revoke_xml = (
        '<sysmsg type="revokemsg"><revokemsg><session>wxid_friend01</session>'
        '<newmsgid>123</newmsgid><replacemsg><![CDATA["张三" 撤回了一条消息]]>'
        "</replacemsg></revokemsg></sysmsg>"
    )
    file_xml = (
        "<msg><appmsg><title>报告.pdf</title><type>6</type><totallen>2048</totallen>"
        "<md5>0123456789abcdef0123456789abcdef</md5></appmsg></msg>"
    )
    history_xml = (
        "<msg><appmsg><title>聊天记录</title><des>三条消息</des><type>19</type>"
        "<recorditem><![CDATA[<recorditem><desc>记录摘要</desc></recorditem>]]>"
        "</recorditem></appmsg></msg>"
    )
    transfer_xml = (
        "<msg><appmsg><title>转账</title><type>2000</type><wcpayinfo>"
        "<feedesc>￥12.00</feedesc><pay_memo>午饭</pay_memo><paysubtype>1</paysubtype>"
        "<receivestatus>0</receivestatus><sendertitle>转账给张三</sendertitle>"
        "<receivertitle>来自张三的转账</receivertitle><senderdes>转账说明</senderdes>"
        "<receiverdes>收款说明</receiverdes><transferid>tid-1</transferid>"
        "<invalidtime>1700000000</invalidtime></wcpayinfo></appmsg></msg>"
    )
    red_packet_xml = (
        "<msg><appmsg><title>红包</title><type>2001</type><wcpayinfo>"
        "<sendertitle>恭喜发财</sendertitle><receivertitle>收到红包</receivertitle>"
        "<senderdes>大吉大利</senderdes><receiverdes>谢谢</receiverdes>"
        "<receiverc2cshowsourceurl>https://example.com/cover.jpg</receiverc2cshowsourceurl>"
        "</wcpayinfo></appmsg></msg>"
    )
    mini_program_xml = (
        "<msg><appmsg><title>小程序标题</title><des>小程序描述</des><type>33</type>"
        "<url>https://example.com/mini</url><weappinfo><username>gh_mini</username>"
        "<nickname>小程序名</nickname><weappiconurl>https://example.com/icon.png</weappiconurl>"
        "</weappinfo></appmsg></msg>"
    )
    finder_xml = (
        "<msg><appmsg><title>当前版本不支持展示该内容，请升级至最新版本。</title>"
        "<type>51</type><finderFeed><desc>视频号描述</desc><nickname>视频号名</nickname>"
        "<username>v2_finder</username><objectid>oid-1</objectid>"
        "<objectnonceid>nonce-1</objectnonceid>"
        "<thumbUrl>https://example.com/finder.jpg</thumbUrl></finderFeed></appmsg></msg>"
    )
    music_xml = (
        "<msg><appmsg><title>歌曲名</title><des>歌手</des><type>3</type>"
        "<url>https://example.com/song</url>"
        "<songalbumurl>https://example.com/album.jpg</songalbumurl></appmsg></msg>"
    )
    fallback_app_xml = "<msg><appmsg><title>仅标题</title><type>88</type></appmsg></msg>"
    rows = [
        (1, 1001, 1, 1, 1, 1_700_000_100, "第一条消息", None),
        (2, 1002, 1, 2, 2, 1_700_000_200, "a &amp; b", None),
        (3, 1003, 49, 3, 2, 1_700_000_300, zstd_bytes(app_xml).hex(), None),
        (4, 1004, 49, 4, 1, 1_700_000_400, "placeholder", zstd_bytes(compressed_xml)),
        (5, 1005, 3, 5, 2, 1_700_000_500, zstd_bytes("<msg><img/></msg>"), None),
        (6, 1006, 1, 6, 2, 1_700_000_600, "68656c6c6f", None),
        (7, 1007, 1, 7, 1, 1_700_000_700, "&lt;not xml&gt;", b"\xff\xfe"),
        (8, 1008, 1, 8, 2, 1_700_000_800, "shard0 最后", None),
        (9, 1009, 48, 9, 2, 1_700_000_900, location_xml, None),
        (10, 1010, 244813135921, 10, 2, 1_700_001_000, quote_xml, None),
        (11, 1011, 266287972401, 11, 2, 1_700_001_100, pat_xml, None),
        (12, 1012, 10000, 12, 2, 1_700_001_200, revoke_xml, None),
        (13, 1013, 49, 13, 1, 1_700_001_300, file_xml, None),
        (14, 1014, 49, 14, 1, 1_700_001_400, history_xml, None),
        (15, 1015, 49, 15, 2, 1_700_001_500, transfer_xml, None),
        (16, 1016, 49, 16, 2, 1_700_001_600, red_packet_xml, None),
        (17, 1017, 49, 17, 2, 1_700_001_700, mini_program_xml, None),
        (18, 1018, 49, 18, 2, 1_700_001_800, finder_xml, None),
        (19, 1019, 49, 19, 2, 1_700_001_900, music_xml, None),
        (20, 1020, 49, 20, 2, 1_700_002_000, fallback_app_xml, None),
    ]
    connection.executemany(
        f'INSERT INTO "{friend_table}" VALUES (?,?,?,?,?,?,?,?)', rows
    )

    group_table = msg_table(GROUP)
    create_message_table(connection, group_table)
    group_rows = [
        (1, 3001, 1, 1, 4, 1_700_000_120, "群消息一", None),
        (2, 3002, 49, 2, 1, 1_700_000_220, zstd_bytes("<msg><appmsg>群链接</appmsg></msg>").hex(), None),
        (3, 3003, 1, 3, 5, 1_700_000_320, "陌生人发言", None),
    ]
    connection.executemany(
        f'INSERT INTO "{group_table}" VALUES (?,?,?,?,?,?,?,?)', group_rows
    )
    connection.commit()
    connection.close()

def build_message_shard1(account_dir: Path) -> None:
    path = account_dir / SHARD1
    connection = connect(path)
    connection.execute("CREATE TABLE Name2Id (user_name TEXT)")
    connection.executemany(
        "INSERT INTO Name2Id (rowid, user_name) VALUES (?, ?)", name2id_rows()
    )

    friend_table = msg_table(FRIEND1, prefix="chat")
    create_message_table(connection, friend_table)
    rows = [
        (1, 2001, 1, 1, 2, 1_700_000_250, "跨库消息", None),
        (2, 2002, 1, 8, 1, 1_700_000_800, "shard1 同时刻", None),
    ]
    connection.executemany(
        f'INSERT INTO "{friend_table}" VALUES (?,?,?,?,?,?,?,?)', rows
    )

    minimal_table = msg_table(FRIEND2)
    create_message_table(connection, minimal_table, minimal=True)
    minimal_rows = [
        (1, 1, 3, 1_700_000_150, "无排序列一"),
        (2, 1, 1, 1_700_000_350, "second minimal"),
        (3, 1, 3, 1_700_000_350, "same time minimal"),
    ]
    connection.executemany(
        f'INSERT INTO "{minimal_table}" VALUES (?,?,?,?,?)', minimal_rows
    )
    connection.commit()
    connection.close()

KIND_BY_TYPE = {
    1: "text",
    3: "image",
    34: "voice",
    43: "video",
    62: "video",
    47: "emoji",
    48: "location",
    49: "app",
    50: "voip",
    10_000: "system",
    244_813_135_921: "quote",
    266_287_972_401: "pat",
    25_769_803_825: "file",
    270_582_939_697: "live",
}

def parse_message_body(local_type: int, content: str):
    if local_type == 10000:
        return {"renderType": "system", "content": _parse_system_message_content(content)}
    if local_type == 266287972401:
        return {"renderType": "system", "content": _parse_pat_message(content, {})}
    if local_type in (49, 244813135921):
        return _parse_app_message(content)
    if local_type == 48:
        return _parse_location_message(content)
    return None

def read_shard_messages(account_dir: Path, shard_name: str, username: str):
    path = account_dir / shard_name
    connection = sqlite3.connect(path)
    connection.text_factory = bytes
    try:
        tables = [
            row[0].decode("utf-8")
            for row in connection.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )
        ]
        expected = msg_table(username)
        expected_chat = msg_table(username, prefix="chat")
        table = next((name for name in tables if name.lower() == expected.lower()), None)
        if table is None:
            table = next(
                (name for name in tables if name.lower() == expected_chat.lower()), None
            )
        if table is None:
            return [], None

        columns = {
            row[1].decode("utf-8").lower()
            for row in connection.execute(f'PRAGMA table_info("{table}")')
        }
        if "local_id" not in columns:
            return [], None

        name2id_table = next(
            (name for name in tables if name.lower() == "name2id"), None
        )
        name2id: dict[int, str] = {}
        if name2id_table is not None:
            for row in connection.execute(
                f'SELECT rowid, user_name FROM "{name2id_table}"'
            ):
                name2id[int(row[0])] = (row[1] or b"").decode("utf-8", errors="ignore")

        self_rowid = next(
            (rowid for rowid, name in name2id.items() if name == SELF), None
        )

        def expression(name: str, fallback: str) -> str:
            return f"m.{name}" if name in columns else f"({fallback})"

        select = ", ".join(
            [
                expression("local_id", "0"),
                expression("server_id", "0"),
                expression("create_time", "0"),
                expression("sort_seq", "0"),
                expression("local_type", "0"),
                expression("message_content", "NULL"),
                expression("compress_content", "NULL"),
                expression("real_sender_id", "NULL"),
            ]
        )
        join = ""
        sender = "NULL"
        if "real_sender_id" in columns and name2id_table is not None:
            join = f'LEFT JOIN "{name2id_table}" n ON n.rowid = m.real_sender_id'
            sender = "n.user_name"

        rows = connection.execute(
            f'SELECT {select}, {sender} FROM "{table}" m {join} '
            f"ORDER BY {expression('create_time', '0 + 0')} DESC, "
            f"{expression('sort_seq', '0 + 0')} DESC, {expression('local_id', '0 + 0')} DESC"
        ).fetchall()

        messages = []
        for row in rows:
            local_id, server_id, create_time, sort_seq, local_type = (
                int(row[0] or 0),
                int(row[1] or 0),
                int(row[2] or 0),
                int(row[3] or 0),
                int(row[4] or 0),
            )
            message_value = row[5]
            compress_value = row[6]
            raw_sender = row[8]
            sender_username = (
                raw_sender.decode("utf-8", errors="ignore") if raw_sender is not None else ""
            )
            content = _decode_message_content(compress_value, message_value)
            item = {
                "local_id": local_id,
                "server_id": server_id,
                "create_time": create_time,
                "sort_seq": sort_seq,
                "local_type": local_type,
                "kind": KIND_BY_TYPE.get(local_type, "other"),
                "sender_username": sender_username,
                "is_sent": self_rowid is not None and self_rowid == row[7],
                "content": content,
                "shard": shard_name,
                "table": table,
            }
            body = parse_message_body(local_type, content)
            if body is not None:
                item["body"] = body
            messages.append(item)
        return messages, self_rowid
    finally:
        connection.close()

def attach_sender_display_names(
    messages: list[dict[str, Any]],
    account_dir: Path,
    chatroom: str,
    contacts: dict[str, dict[str, Any]],
) -> list[dict[str, Any]]:
    group_map: dict[str, str] = {}
    if chatroom.endswith("@chatroom"):
        senders = sorted({m["sender_username"] for m in messages if m["sender_username"]})
        group_map = _load_group_nickname_map_from_contact_db(
            account_dir / "contact.db", chatroom, senders
        )
    for message in messages:
        sender = message["sender_username"]
        message["sender_display_name"] = (
            group_map.get(sender)
            or (contacts.get(sender) or {}).get("display_name")
            or sender
        )
    return messages

def collect_messages(account_dir: Path, username: str) -> list[dict[str, Any]]:
    messages: list[dict[str, Any]] = []
    for shard in (SHARD0, SHARD1):
        shard_messages, _ = read_shard_messages(account_dir, shard, username)
        messages.extend(shard_messages)
    messages.sort(
        key=lambda item: (
            item["create_time"],
            item["sort_seq"],
            item["local_id"],
            item["shard"],
        ),
        reverse=True,
    )
    return messages

def page(messages: list[dict[str, Any]], limit: int, before: dict[str, Any] | None):
    if before is not None:
        key = (before["create_time"], before["sort_seq"], before["local_id"])
        candidates = [
            item
            for item in messages
            if (item["create_time"], item["sort_seq"], item["local_id"]) < key
        ]
    else:
        candidates = list(messages)
    ordered = sorted(
        candidates,
        key=lambda item: (
            item["create_time"],
            item["sort_seq"],
            item["local_id"],
            item["shard"],
        ),
        reverse=True,
    )
    has_more = len(ordered) > limit
    page_items = ordered[:limit]
    page_items.reverse()
    next_before = None
    if has_more:
        first = page_items[0]
        next_before = {
            "create_time": first["create_time"],
            "sort_seq": first["sort_seq"],
            "local_id": first["local_id"],
            "shard": first["shard"],
        }
    return {"messages": page_items, "has_more": has_more, "next_before": next_before}

IMAGE_MD5 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
RESOURCE_MD5 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
EMOJI_MD5 = "cccccccccccccccccccccccccccccccc"
EMOJI_ALT_MD5 = "cccccccccccccccccccccccccccccccd"
VIDEO_MD5 = "dddddddddddddddddddddddddddddddd"
VIDEO_THUMB_MD5 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
FILE_MD5 = "ffffffffffffffffffffffffffffffff"
MISSING_MD5 = "11111111111111111111111111111111"

def write_file(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)

def build_media_tree(account_dir: Path) -> None:
    attach = account_dir / "msg" / "attach" / md5_hex(FRIEND1) / "2024-01" / "img"
    write_file(attach / f"{IMAGE_MD5}_t.dat", b"image-dat")
    write_file(account_dir / "resource" / RESOURCE_MD5[:2] / f"{RESOURCE_MD5}.png", b"png")
    write_file(account_dir / "cache" / "emoji" / f"{EMOJI_MD5}.png", b"emoji")
    write_file(account_dir / "cache" / "emoji" / f"{EMOJI_ALT_MD5}_h.png", b"emoji-alt")
    write_file(account_dir / "msg" / "video" / f"{VIDEO_MD5}.mp4", b"video")
    write_file(account_dir / "msg" / "video" / f"{VIDEO_THUMB_MD5}_thumb.jpg", b"thumb")
    write_file(account_dir / "msg" / "file" / f"{FILE_MD5}.pdf", b"file")

def build_media_index_expected(account_dir: Path) -> dict[str, str | None]:
    index = MediaPathIndex(account_dir=account_dir)
    index._roots = [account_dir.resolve()]
    index._index_decrypted_resources()
    index._scan_media_roots()
    cases = [
        ("image", IMAGE_MD5),
        ("image", RESOURCE_MD5),
        ("emoji", EMOJI_MD5),
        ("emoji", EMOJI_ALT_MD5),
        ("video", VIDEO_MD5),
        ("video_thumb", VIDEO_THUMB_MD5),
        ("video_thumb", RESOURCE_MD5),
        ("file", FILE_MD5),
        ("image", MISSING_MD5),
    ]
    result: dict[str, str | None] = {}
    root = account_dir.resolve()
    for kind, md5 in cases:
        resolved = index.resolve(kind=kind, md5=md5)
        result[f"{kind}:{md5}"] = (
            resolved.resolve().relative_to(root).as_posix() if resolved is not None else None
        )
    return result

def pb_field_varint(field_no: int, value: int) -> bytes:
    return pb_varint((field_no << 3) | 0) + pb_varint(value)

def pb_field_bytes(field_no: int, payload: bytes) -> bytes:
    return pb_varint((field_no << 3) | 2) + pb_varint(len(payload)) + payload

def build_media_extraction_expected() -> dict[str, object]:
    dat_blob = (
        b"file=0123456789abcdef0123456789abcdef_t.dat "
        b"other=ffffffffffffffffffffffffffffffff"
    )
    plain_blob = (
        b"ffffffffffffffffffffffffffffffff and 0123456789abcdef0123456789abcdef"
    )
    blob_cases = {
        "dat": dat_blob,
        "plain": plain_blob,
        "empty": b"",
    }
    packed_cases = [
        {"type": "blob", "input_hex": dat_blob.hex()},
        {"type": "blob", "input_hex": bytes(range(16)).hex()},
        {"type": "text", "input": "0x" + bytes(range(16)).hex()},
        {"type": "text", "input": "token 0123456789abcdef0123456789abcdef"},
        {"type": "int", "input": 12345},
    ]
    for case in packed_cases:
        if case["type"] == "blob":
            value = bytes.fromhex(case["input_hex"])
        else:
            value = case["input"]
        case["expected"] = _extract_md5_from_packed_info(value)

    transcript = "语音转写"
    completed = pb_field_bytes(
        5,
        pb_field_varint(1, 2) + pb_field_bytes(2, transcript.encode("utf-8")),
    )
    pending = pb_field_bytes(
        5,
        pb_field_varint(1, 1) + pb_field_bytes(2, "未完成".encode("utf-8")),
    )
    voice_cases = [
        {"type": "blob", "input_hex": completed.hex()},
        {"type": "blob", "input_hex": pending.hex()},
        {"type": "text", "input": completed.hex()},
        {"type": "blob", "input_hex": "ff"},
    ]
    for case in voice_cases:
        if case["type"] == "blob":
            value = bytes.fromhex(case["input_hex"])
        else:
            value = case["input"]
        case["expected"] = _extract_voice_transcript_from_packed_info(value)

    return {
        "media_md5_blob": {
            name: {
                "input_hex": value.hex(),
                "expected": _extract_md5_from_blob(value),
            }
            for name, value in blob_cases.items()
        },
        "media_md5_packed": packed_cases,
        "voice_transcripts": voice_cases,
    }

def patch_search_schema(account_dir: Path) -> None:
    table = msg_table(FRIEND2)
    connection = sqlite3.connect(account_dir / "message_1.db")
    try:
        columns = {
            str(row[1]).lower()
            for row in connection.execute(f'PRAGMA table_info("{table}")')
        }
        for name, declaration in (
            ("server_id", "INTEGER DEFAULT 0"),
            ("sort_seq", "INTEGER DEFAULT 0"),
            ("compress_content", "BLOB"),
        ):
            if name not in columns:
                connection.execute(
                    f'ALTER TABLE "{table}" ADD COLUMN {name} {declaration}'
                )
        connection.commit()
    finally:
        connection.close()

def build_search_expected(account_dir: Path) -> dict[str, object]:
    temp_root = Path(tempfile.mkdtemp(prefix="wechat-search-fixture-"))
    temp_account = temp_root / account_dir.name
    shutil.copytree(account_dir, temp_account)
    try:
        patch_search_schema(temp_account)
        chat_search_index._build_worker(temp_account, True)
        index_path = temp_account / "chat_search_index.db"
        if not index_path.exists():
            raise RuntimeError("Python search index build failed")
        connection = sqlite3.connect(index_path)
        try:
            message_count = int(
                connection.execute("SELECT COUNT(*) FROM message_meta").fetchone()[0]
            )
            token_count = int(
                connection.execute("SELECT COUNT(*) FROM message_token_stats").fetchone()[0]
            )
            query_cases = [
                {"q": "消息", "limit": 5},
                {"q": "张三", "username": FRIEND1, "limit": 5},
                {"q": "群", "username": GROUP, "limit": 5},
                {"q": "报告", "limit": 5},
                {"q": "人民广场", "limit": 5},
                {"q": "已读", "limit": 5},
                {"q": "消息", "limit": 3, "offset": 2},
                {"q": "消息", "start_ts": 1_700_001_300, "limit": 10},
                {"q": "消息", "limit": 50},
            ]
            queries = []
            for case in query_cases:
                fts_query = _build_fts_query(case["q"])
                where = ["message_fts MATCH ?"]
                params: list[Any] = [fts_query]
                if case.get("username"):
                    where.append("username = ?")
                    params.append(case["username"])
                if case.get("start_ts") is not None:
                    where.append("CAST(create_time AS INTEGER) >= ?")
                    params.append(int(case["start_ts"]))
                if case.get("end_ts") is not None:
                    where.append("CAST(create_time AS INTEGER) <= ?")
                    params.append(int(case["end_ts"]))
                if not case.get("include_hidden", False):
                    where.append("CAST(is_hidden AS INTEGER) = 0")
                if not case.get("include_official", False):
                    where.append("CAST(is_official AS INTEGER) = 0")
                limit = int(case["limit"])
                offset = int(case.get("offset", 0))
                sql = (
                    "SELECT username, local_id, render_type, payload_json FROM message_fts "
                    f"WHERE {' AND '.join(where)} "
                    "ORDER BY CAST(create_time AS INTEGER) DESC, "
                    "CAST(sort_seq AS INTEGER) DESC, CAST(local_id AS INTEGER) DESC "
                    "LIMIT ? OFFSET ?"
                )
                rows = connection.execute(
                    sql, params + [limit + 1, offset]
                ).fetchall()
                has_more = len(rows) > limit
                hits = [
                    {
                        "username": str(row[0]),
                        "local_id": int(row[1]),
                        "render_type": str(row[2]),
                        "payload": json.loads(row[3]),
                    }
                    for row in rows[:limit]
                ]
                queries.append(
                    {
                        "query": case,
                        "fts_query": fts_query,
                        "has_more": has_more,
                        "hits": hits,
                    }
                )
        finally:
            connection.close()
    finally:
        shutil.rmtree(temp_root, ignore_errors=True)
    return {
        "message_count": message_count,
        "unique_single_char_tokens": token_count,
        "queries": queries,
    }

def build_expected_sessions(
    session_rows: list[dict[str, Any]],
    contacts: dict[str, dict[str, Any]],
    avatars: set[str],
    *,
    include_hidden: bool,
    include_official: bool,
) -> list[dict[str, Any]]:
    kept = []
    for row in session_rows:
        username = row["username"]
        if not include_hidden and row["is_hidden"]:
            continue
        if not _should_keep_session(username, include_official):
            continue
        summary_value = row["summary"]
        decoded = _decode_message_content(None, summary_value)
        if not decoded:
            decoded = (
                summary_value.decode("utf-8", errors="ignore")
                if isinstance(summary_value, bytes)
                else ("" if summary_value is None else str(summary_value))
            )
        contact = contacts.get(username)
        kept.append(
            {
                "username": username,
                "display_name": _pick_display_name(contact, username),
                "is_group": username.endswith("@chatroom"),
                "is_official": username.startswith("gh_"),
                "is_hidden": row["is_hidden"],
                "summary": decoded,
                "draft": "" if row["draft"] is None else row["draft"],
                "last_msg_type": row["last_msg_type"],
                "last_msg_sub_type": row["last_msg_sub_type"],
                "sort_timestamp": row["sort_timestamp"],
                "last_timestamp": row["last_timestamp"],
                "has_avatar": username in avatars,
            }
        )
    kept.sort(
        key=lambda item: (
            item["sort_timestamp"] if item["sort_timestamp"] is not None else -1,
            item["last_timestamp"] if item["last_timestamp"] is not None else -1,
        ),
        reverse=True,
    )
    return kept

def main() -> None:
    if FIXTURES.exists():
        shutil.rmtree(FIXTURES)
    account_dir = ACCOUNTS_ROOT / SELF
    build_account_files(account_dir)
    session_rows = build_session_db(account_dir)
    contacts = build_contact_db(account_dir)
    avatars = build_head_image_db(account_dir)
    build_message_shard0(account_dir)
    build_message_shard1(account_dir)
    build_media_tree(account_dir)

    friend1_messages = attach_sender_display_names(
        collect_messages(account_dir, FRIEND1), account_dir, FRIEND1, contacts
    )
    friend2_messages = attach_sender_display_names(
        collect_messages(account_dir, FRIEND2), account_dir, FRIEND2, contacts
    )
    group_messages = attach_sender_display_names(
        collect_messages(account_dir, GROUP), account_dir, GROUP, contacts
    )
    first_page = page(friend1_messages, 3, None)
    second_page = page(friend1_messages, 3, first_page["next_before"])
    group_nicknames = _load_group_nickname_map_from_contact_db(
        account_dir / "contact.db",
        GROUP,
        sorted({m["sender_username"] for m in group_messages if m["sender_username"]}),
    )

    expected = {
        "account": {
            "account": SELF,
            "self_username": SELF,
            "message_shards": [SHARD0, SHARD1],
        },
        "sessions_default": build_expected_sessions(
            session_rows, contacts, avatars, include_hidden=False, include_official=False
        ),
        "sessions_with_official": build_expected_sessions(
            session_rows, contacts, avatars, include_hidden=False, include_official=True
        ),
        "sessions_with_hidden": build_expected_sessions(
            session_rows, contacts, avatars, include_hidden=True, include_official=False
        ),
        "contacts": [contacts[name] for name in sorted(contacts)],
        "messages": {
            FRIEND1: {
                "all": list(reversed(friend1_messages)),
                "page1": first_page,
                "page2": second_page,
            },
            FRIEND2: {"all": list(reversed(friend2_messages))},
            GROUP: {"all": list(reversed(group_messages))},
        },
        "group_nicknames": group_nicknames,
        "media_paths": build_media_index_expected(account_dir),
        **build_media_extraction_expected(),
        "search": build_search_expected(account_dir),
        "counts": {
            FRIEND1: len(friend1_messages),
            FRIEND2: len(friend2_messages),
            GROUP: len(group_messages),
            "wxid_missing": 0,
        },
    }
    (FIXTURES / "expected.json").write_text(
        json.dumps(expected, ensure_ascii=False, indent=2, sort_keys=True), encoding="utf-8"
    )
    print(f"fixtures written to {FIXTURES}")

if __name__ == "__main__":
    main()
