use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde_json::Value;
use wechat_data::model::Session;
use wechat_data::sqlite::SqlValue;
use wechat_data::{
    build_search_index, discover_accounts, extract_md5_from_blob, extract_md5_from_packed_info,
    extract_voice_transcript_from_packed_info, refresh_search_index, search_index, AccountStore,
    MediaIndex, MessageQuery, SearchQuery,
};

const SELF: &str = "wxid_self0001";
const FRIEND1: &str = "wxid_friend01";
const FRIEND2: &str = "wxid_friend02";
const GROUP: &str = "123456@chatroom";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn account_dir() -> PathBuf {
    fixture_root().join("accounts").join(SELF)
}

fn expected() -> Value {
    let data = std::fs::read(fixture_root().join("expected.json")).expect("read expected.json");
    serde_json::from_slice(&data).expect("parse expected.json")
}

fn open_store() -> AccountStore {
    AccountStore::open(&account_dir()).expect("open fixture account")
}

fn sessions_json(sessions: &[Session]) -> Value {
    let mut value = serde_json::to_value(sessions).expect("serialize sessions");
    for item in value.as_array_mut().expect("session array") {
        item.as_object_mut()
            .expect("session object")
            .remove("last_time_text");
    }
    value
}

#[test]
fn discovers_account_and_message_shards() {
    let accounts = discover_accounts(&fixture_root().join("accounts")).expect("discover accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].account, SELF);
    let expected = expected();
    assert_eq!(
        serde_json::to_value(&accounts[0].message_shards).unwrap(),
        expected["account"]["message_shards"]
    );

    let store = open_store();
    assert_eq!(store.account(), SELF);
    assert_eq!(store.self_username(), SELF);
    assert_eq!(store.account_info().message_shards.len(), 2);
}

#[test]
fn session_list_matches_python_reference() {
    let expected = expected();
    let store = open_store();

    let sessions = store.list_sessions(false, false).expect("sessions");
    assert_eq!(sessions_json(&sessions), expected["sessions_default"]);

    let with_official = store.list_sessions(false, true).expect("sessions");
    assert_eq!(
        sessions_json(&with_official),
        expected["sessions_with_official"]
    );

    let with_hidden = store.list_sessions(true, false).expect("sessions");
    assert_eq!(
        sessions_json(&with_hidden),
        expected["sessions_with_hidden"]
    );
}

#[test]
fn contacts_match_python_reference() {
    let expected = expected();
    let store = open_store();
    let contacts = store.list_contacts().expect("contacts");
    assert_eq!(
        serde_json::to_value(&contacts).unwrap(),
        expected["contacts"]
    );

    assert!(store
        .get_contact("wxid_missing")
        .expect("contact lookup")
        .is_none());
    let friend = store
        .get_contact(FRIEND1)
        .expect("contact lookup")
        .expect("friend contact");
    assert_eq!(friend.display_name, "张三");
    assert_eq!(
        friend.big_head_url.as_deref(),
        Some("http://example.com/zhang.jpg")
    );
}

#[test]
fn merged_messages_match_python_reference() {
    let expected = expected();
    let store = open_store();
    let page = store
        .load_messages(FRIEND1, &MessageQuery::default())
        .expect("messages");
    assert!(!page.has_more);
    assert_eq!(
        serde_json::to_value(&page.messages).unwrap(),
        expected["messages"][FRIEND1]["all"]
    );
}

#[test]
fn keyset_pagination_matches_python_reference() {
    let expected = expected();
    let store = open_store();

    let first = store
        .load_messages(
            FRIEND1,
            &MessageQuery {
                limit: 3,
                before: None,
            },
        )
        .expect("first page");
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        expected["messages"][FRIEND1]["page1"]
    );

    let second = store
        .load_messages(
            FRIEND1,
            &MessageQuery {
                limit: 3,
                before: first.next_before.clone(),
            },
        )
        .expect("second page");
    assert_eq!(
        serde_json::to_value(&second).unwrap(),
        expected["messages"][FRIEND1]["page2"]
    );
}

#[test]
fn minimal_shard_columns_fall_back_to_zero() {
    let expected = expected();
    let store = open_store();
    let page = store
        .load_messages(FRIEND2, &MessageQuery::default())
        .expect("messages");
    assert_eq!(
        serde_json::to_value(&page.messages).unwrap(),
        expected["messages"][FRIEND2]["all"]
    );

    let paged = store
        .load_messages(
            FRIEND2,
            &MessageQuery {
                limit: 2,
                before: None,
            },
        )
        .expect("first page");
    assert_eq!(paged.messages.len(), 2);
    assert!(paged.has_more);
    assert_eq!(
        paged.next_before.as_ref().map(|cursor| cursor.sort_seq),
        Some(0)
    );

    let rest = store
        .load_messages(
            FRIEND2,
            &MessageQuery {
                limit: 2,
                before: paged.next_before,
            },
        )
        .expect("second page");
    assert_eq!(rest.messages.len(), 1);
    assert!(!rest.has_more);
}

#[test]
fn group_messages_resolve_member_names() {
    let expected = expected();
    let store = open_store();
    let page = store
        .load_messages(GROUP, &MessageQuery::default())
        .expect("group messages");
    assert_eq!(
        serde_json::to_value(&page.messages).unwrap(),
        expected["messages"][GROUP]["all"]
    );
    assert_eq!(page.messages[0].sender_username, "wxid_groupmember");
    assert_eq!(page.messages[0].sender_display_name, "群昵称甲");
    assert!(!page.messages[0].is_sent);

    let nicknames = store
        .group_nicknames(
            GROUP,
            &[
                "wxid_groupmember".to_string(),
                "wxid_self0001".to_string(),
                "wxid_stranger01".to_string(),
            ],
        )
        .expect("group nicknames");
    assert_eq!(
        serde_json::to_value(&nicknames).unwrap(),
        expected["group_nicknames"]
    );
}

#[test]
fn counts_match_python_reference() {
    let expected = expected();
    let store = open_store();
    for username in [FRIEND1, FRIEND2, GROUP, "wxid_missing"] {
        assert_eq!(
            store.count_messages(username).expect("count"),
            expected["counts"][username].as_i64().unwrap(),
            "{username}"
        );
    }
}

#[test]
fn unknown_user_returns_empty_page() {
    let store = open_store();
    let page = store
        .load_messages("wxid_missing", &MessageQuery::default())
        .expect("messages");
    assert!(page.messages.is_empty());
    assert!(!page.has_more);
    assert!(page.next_before.is_none());
}

fn value_from_case(case: &Value) -> SqlValue {
    match case["type"].as_str().expect("case type") {
        "blob" => SqlValue::Blob(
            hex::decode(case["input_hex"].as_str().expect("input hex")).expect("hex input"),
        ),
        "text" => SqlValue::Text(case["input"].as_str().expect("text input").to_string()),
        "int" => SqlValue::Integer(case["input"].as_i64().expect("int input")),
        other => panic!("unknown case type {other}"),
    }
}

#[test]
fn media_md5_extraction_matches_python_reference() {
    let expected = expected();
    let cases = expected["media_md5_blob"]
        .as_object()
        .expect("blob cases object");
    for (name, case) in cases {
        let input = hex::decode(case["input_hex"].as_str().expect("input hex")).unwrap();
        assert_eq!(
            extract_md5_from_blob(&input),
            case["expected"].as_str().unwrap(),
            "blob case {name}"
        );
    }

    for (index, case) in expected["media_md5_packed"]
        .as_array()
        .expect("packed cases array")
        .iter()
        .enumerate()
    {
        assert_eq!(
            extract_md5_from_packed_info(&value_from_case(case)),
            case["expected"].as_str().unwrap(),
            "packed case {index}"
        );
    }
}

#[test]
fn voice_transcripts_match_python_reference() {
    let expected = expected();
    for (index, case) in expected["voice_transcripts"]
        .as_array()
        .expect("voice cases array")
        .iter()
        .enumerate()
    {
        assert_eq!(
            extract_voice_transcript_from_packed_info(&value_from_case(case)),
            case["expected"].as_str().unwrap(),
            "voice case {index}"
        );
    }
}

#[test]
fn media_index_resolves_paths_like_python() {
    let expected = expected();
    let account_dir = account_dir();
    let index = MediaIndex::build(&account_dir);
    let paths = expected["media_paths"].as_object().expect("media paths");
    for (key, expected_path) in paths {
        let (kind, md5) = key.split_once(':').expect("kind:md5 key");
        let actual = index.resolve(kind, md5).map(|path| {
            path.strip_prefix(&account_dir)
                .expect("fixture path")
                .to_string_lossy()
                .replace('\\', "/")
        });
        let expected_path = expected_path.as_str();
        assert_eq!(actual.as_deref(), expected_path, "{key}");
    }
}

fn copy_dir(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create destination");
    for entry in std::fs::read_dir(source).expect("read source") {
        let entry = entry.expect("entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

fn patch_search_schema(account_dir: &Path) {
    let mut digest = Md5::new();
    digest.update(FRIEND2.as_bytes());
    let table = format!("msg_{}", hex::encode(digest.finalize()));
    let connection =
        rusqlite::Connection::open(account_dir.join("message_1.db")).expect("open shard");
    connection
        .execute_batch(&format!(
            "ALTER TABLE \"{table}\" ADD COLUMN server_id INTEGER DEFAULT 0;\
             ALTER TABLE \"{table}\" ADD COLUMN sort_seq INTEGER DEFAULT 0;\
             ALTER TABLE \"{table}\" ADD COLUMN compress_content BLOB;"
        ))
        .expect("patch search schema");
}

#[test]
fn search_index_matches_python_reference() {
    let expected = expected();
    let search = &expected["search"];
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join(SELF);
    copy_dir(&account_dir(), &account);
    patch_search_schema(&account);

    let stats = build_search_index(&account).expect("build search index");
    assert_eq!(
        stats.indexed_messages,
        search["message_count"].as_i64().unwrap()
    );
    assert_eq!(
        stats.unique_single_char_tokens,
        usize::try_from(search["unique_single_char_tokens"].as_u64().unwrap()).unwrap()
    );

    for case in search["queries"].as_array().expect("search queries") {
        let query_value = &case["query"];
        let limit = usize::try_from(query_value["limit"].as_u64().unwrap()).unwrap();
        let offset = usize::try_from(query_value["offset"].as_u64().unwrap_or(0)).unwrap();
        let query = SearchQuery {
            q: query_value["q"].as_str().unwrap().to_string(),
            username: query_value["username"].as_str().map(str::to_string),
            include_hidden: query_value["include_hidden"].as_bool().unwrap_or(false),
            include_official: query_value["include_official"].as_bool().unwrap_or(false),
            start_ts: query_value["start_ts"].as_i64(),
            end_ts: query_value["end_ts"].as_i64(),
            limit,
            offset,
        };
        let page = search_index(&account, &query).expect("search");
        assert_eq!(
            page.has_more,
            case["has_more"].as_bool().unwrap(),
            "has_more for {:?}",
            query.q
        );
        let expected_hits = case["hits"].as_array().expect("hits");
        assert_eq!(
            page.hits.len(),
            expected_hits.len(),
            "hit count for {:?}",
            query.q
        );
        for (hit, expected_hit) in page.hits.iter().zip(expected_hits) {
            assert_eq!(hit.username, expected_hit["username"].as_str().unwrap());
            assert_eq!(hit.local_id, expected_hit["local_id"].as_i64().unwrap());
            assert_eq!(
                hit.render_type,
                expected_hit["render_type"].as_str().unwrap()
            );
            assert_eq!(
                hit.payload, expected_hit["payload"],
                "payload for {:?} local_id {}",
                query.q, hit.local_id
            );
        }
    }
}

#[test]
fn search_refresh_is_noop_when_nothing_changed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join(SELF);
    copy_dir(&account_dir(), &account);

    let built = build_search_index(&account).expect("build search index");
    let refreshed = refresh_search_index(&account).expect("refresh search index");
    assert_eq!(refreshed.conversations, built.conversations);
    assert_eq!(refreshed.refreshed_conversations, 0);
    assert_eq!(refreshed.removed_conversations, 0);
    assert_eq!(refreshed.inserted_messages, 0);
    assert_eq!(refreshed.removed_messages, 0);
    assert_eq!(refreshed.message_count, built.indexed_messages);
    assert_eq!(
        refreshed.unique_single_char_tokens,
        built.unique_single_char_tokens
    );
}

#[test]
fn search_refresh_reindexes_changed_conversation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join(SELF);
    copy_dir(&account_dir(), &account);
    let built = build_search_index(&account).expect("build search index");

    let mut digest = Md5::new();
    digest.update(FRIEND1.as_bytes());
    let table = format!("msg_{}", hex::encode(digest.finalize()));
    let connection = rusqlite::Connection::open(account.join("message_0.db")).expect("open shard");
    connection
        .execute(
            &format!(
                "INSERT INTO \"{table}\" (local_id, server_id, local_type, sort_seq, \
                 real_sender_id, create_time, message_content, compress_content) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)"
            ),
            rusqlite::params![21, 1021, 1, 21, 2, 1_700_001_999i64, "刷新新增"],
        )
        .expect("insert message");
    drop(connection);

    let refreshed = refresh_search_index(&account).expect("refresh search index");
    assert_eq!(refreshed.refreshed_conversations, 1);
    assert_eq!(refreshed.removed_conversations, 0);
    assert_eq!(
        refreshed.inserted_messages,
        refreshed.removed_messages + 1,
        "the changed conversation is fully reindexed with one new message"
    );
    assert_eq!(refreshed.message_count, built.indexed_messages + 1);

    let page = search_index(&account, &SearchQuery::new("刷新新增")).expect("search");
    assert_eq!(page.hits.len(), 1);
    assert_eq!(page.hits[0].username, FRIEND1);
    assert_eq!(page.hits[0].local_id, 21);

    let refreshed_index = temp.path().join("refreshed_index.db");
    std::fs::copy(account.join("chat_search_index.db"), &refreshed_index)
        .expect("copy refreshed index");
    let rebuilt = build_search_index(&account).expect("rebuild search index");
    assert_eq!(rebuilt.indexed_messages, refreshed.message_count);
    assert_eq!(
        token_stats(&refreshed_index),
        token_stats(&account.join("chat_search_index.db")),
        "incremental token stats must match a full rebuild"
    );
}

fn token_stats(path: &Path) -> Vec<(String, i64)> {
    let connection = rusqlite::Connection::open(path).expect("open index");
    let mut statement = connection
        .prepare("SELECT token, doc_count FROM message_token_stats ORDER BY token")
        .expect("prepare token stats");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .expect("query token stats");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect token stats")
}

#[test]
fn search_refresh_builds_missing_index() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join(SELF);
    copy_dir(&account_dir(), &account);

    let stats = refresh_search_index(&account).expect("refresh search index");
    assert_eq!(stats.conversations, stats.refreshed_conversations);
    assert_eq!(stats.message_count, stats.inserted_messages);
    assert!(stats.message_count > 0);
    assert!(account.join("chat_search_index.db").is_file());
}

#[test]
fn discovers_export_output_root_with_nested_databases() {
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("databases").join(SELF);
    copy_account(&account_dir(), &target);
    let accounts = discover_accounts(temp.path()).expect("discover export root");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].account, SELF);
}

fn copy_account(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("create dir");
    for entry in std::fs::read_dir(source).expect("read dir").flatten() {
        let path = entry.path();
        let destination = target.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_account(&path, &destination);
        } else {
            std::fs::copy(&path, &destination).expect("copy file");
        }
    }
}
