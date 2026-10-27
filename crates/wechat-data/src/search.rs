use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::account::AccountStore;
use crate::error::{DataError, Result};
use crate::message::MessageQuery;
use crate::message_body::{extract_attr, MessageBody};
use crate::model::{Message, MessageKind};
use crate::sqlite::{column_set, open_read_only, row_text, table_columns, table_exists};

pub const SEARCH_INDEX_SCHEMA_VERSION: i64 = 4;
pub const SEARCH_INDEX_FILE_NAME: &str = "chat_search_index.db";

const INSERT_FTS_SQL: &str =
    "INSERT INTO message_fts(rowid, text, username, render_type, create_time, sort_seq, \
     local_id, server_id, local_type, db_stem, table_name, sender_username, is_hidden, \
     is_official, payload_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)";
const INSERT_META_SQL: &str =
    "INSERT INTO message_meta(rowid, text, username, render_type, create_time, sort_seq, \
     local_id, server_id, local_type, db_stem, table_name, sender_username, is_hidden, \
     is_official) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)";

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub q: String,
    pub username: Option<String>,
    pub include_hidden: bool,
    pub include_official: bool,
    pub start_ts: Option<i64>,
    pub end_ts: Option<i64>,
    pub limit: usize,
    pub offset: usize,
}

impl SearchQuery {
    pub fn new(q: impl Into<String>) -> Self {
        Self {
            q: q.into(),
            limit: 20,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub username: String,
    pub db_stem: String,
    pub table_name: String,
    pub local_id: i64,
    pub server_id: i64,
    pub local_type: i64,
    pub sender_username: String,
    pub render_type: String,
    pub create_time: i64,
    pub sort_seq: i64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub has_more: bool,
    pub total_estimate: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchBuildStats {
    pub conversations: usize,
    pub indexed_messages: i64,
    pub unique_single_char_tokens: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchRefreshStats {
    pub conversations: usize,
    pub refreshed_conversations: usize,
    pub removed_conversations: usize,
    pub inserted_messages: i64,
    pub removed_messages: i64,
    pub message_count: i64,
    pub unique_single_char_tokens: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ConversationFingerprint {
    count: i64,
    max_local_id: i64,
    sum_local_id: i64,
    max_create_time: i64,
    max_sort_seq: i64,
    sum_text_len: i64,
    sum_local_type: i64,
}

impl ConversationFingerprint {
    fn observe(&mut self, document: &SearchDocument) {
        self.count += 1;
        self.max_local_id = self.max_local_id.max(document.local_id);
        self.sum_local_id = self.sum_local_id.saturating_add(document.local_id);
        self.max_create_time = self.max_create_time.max(document.create_time);
        self.max_sort_seq = self.max_sort_seq.max(document.sort_seq);
        self.sum_text_len = self.sum_text_len.saturating_add(document.text.len() as i64);
        self.sum_local_type = self.sum_local_type.saturating_add(document.local_type);
    }
}

#[derive(Debug, Clone)]
struct SearchDocument {
    text: String,
    username: String,
    render_type: String,
    create_time: i64,
    sort_seq: i64,
    local_id: i64,
    server_id: i64,
    local_type: i64,
    db_stem: String,
    table_name: String,
    sender_username: String,
    is_hidden: i64,
    is_official: i64,
    payload: Value,
}

pub fn chat_search_index_path(account_dir: &Path) -> PathBuf {
    account_dir.join(SEARCH_INDEX_FILE_NAME)
}

pub fn char_token_text(value: &str) -> String {
    let chars: Vec<String> = value
        .trim()
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(|ch| ch.to_lowercase().collect::<String>())
        .collect();
    chars.join(" ")
}

pub fn build_fts_query(q: &str) -> String {
    let tokens: Vec<&str> = q.split_whitespace().take(8).collect();
    let parts: Vec<String> = tokens
        .into_iter()
        .filter_map(|token| {
            let clean: String = token.chars().filter(|ch| *ch != '"').collect();
            let clean = clean.trim();
            if clean.is_empty() {
                return None;
            }
            let phrase: Vec<String> = clean
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .map(|ch| ch.to_string())
                .collect();
            (!phrase.is_empty()).then(|| format!("\"{}\"", phrase.join(" ")))
        })
        .collect();
    parts.join(" AND ")
}

pub fn build_search_index(account_dir: &Path) -> Result<SearchBuildStats> {
    let store = AccountStore::open(account_dir)?;
    let targets = collect_targets(&store)?;
    let final_path = chat_search_index_path(store.account_dir());
    let tmp_path = final_path.with_extension("tmp.db");
    if tmp_path.exists() {
        let _ = std::fs::remove_file(&tmp_path);
    }

    let mut connection = Connection::open(&tmp_path)?;
    for pragma in [
        "PRAGMA journal_mode=OFF",
        "PRAGMA synchronous=OFF",
        "PRAGMA temp_store=MEMORY",
        "PRAGMA cache_size=-131072",
    ] {
        let _ = connection.execute_batch(pragma);
    }
    init_schema(&connection)?;

    let mut stats = SearchBuildStats {
        conversations: targets.len(),
        ..SearchBuildStats::default()
    };
    let mut token_stats: HashMap<String, i64> = HashMap::new();
    let transaction = connection.transaction()?;
    let mut next_rowid: i64 = 1;
    {
        let mut insert_fts = transaction.prepare(INSERT_FTS_SQL)?;
        let mut insert_meta = transaction.prepare(INSERT_META_SQL)?;

        for target in &targets {
            for_each_document(&store, target, |document| {
                for token in single_char_tokens(&document.text) {
                    *token_stats.entry(token).or_insert(0) += 1;
                }
                insert_document(&mut insert_fts, &mut insert_meta, next_rowid, document)?;
                next_rowid += 1;
                stats.indexed_messages += 1;
                Ok(())
            })?;
        }

        for row in [
            "CREATE INDEX IF NOT EXISTS idx_message_meta_visible_time ON message_meta(is_hidden, is_official, create_time DESC, sort_seq DESC, local_id DESC)",
            "CREATE INDEX IF NOT EXISTS idx_message_meta_username_time ON message_meta(username, create_time DESC, sort_seq DESC, local_id DESC)",
            "CREATE INDEX IF NOT EXISTS idx_message_meta_sender_time ON message_meta(sender_username, create_time DESC, sort_seq DESC, local_id DESC)",
        ] {
            transaction.execute_batch(row)?;
        }
    }

    if !token_stats.is_empty() {
        let mut insert_stats = transaction
            .prepare("INSERT INTO message_token_stats(token, doc_count) VALUES(?1, ?2)")?;
        let mut sorted: Vec<(&String, &i64)> = token_stats.iter().collect();
        sorted.sort_by(|left, right| left.0.cmp(right.0));
        for (token, count) in sorted {
            insert_stats.execute(params![token, count])?;
        }
    }
    stats.unique_single_char_tokens = token_stats.len();

    let meta = [
        ("schema_version", SEARCH_INDEX_SCHEMA_VERSION.to_string()),
        (
            "built_at",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_secs() as i64)
                .unwrap_or_default()
                .to_string(),
        ),
        ("message_count", stats.indexed_messages.to_string()),
        ("source", "decrypted".to_string()),
    ];
    for (key, value) in meta {
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }
    transaction.commit()?;
    drop(connection);

    if final_path.exists() {
        std::fs::remove_file(&final_path)?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(stats)
}

pub fn refresh_search_index(account_dir: &Path) -> Result<SearchRefreshStats> {
    let store = AccountStore::open(account_dir)?;
    let index_path = chat_search_index_path(store.account_dir());
    if !index_path.is_file() {
        let built = build_search_index(account_dir)?;
        return Ok(SearchRefreshStats {
            conversations: built.conversations,
            refreshed_conversations: built.conversations,
            removed_conversations: 0,
            inserted_messages: built.indexed_messages,
            removed_messages: 0,
            message_count: built.indexed_messages,
            unique_single_char_tokens: built.unique_single_char_tokens,
        });
    }

    let targets = collect_targets(&store)?;
    let target_names: HashSet<&str> = targets
        .iter()
        .map(|target| target.username.as_str())
        .collect();
    let mut connection = Connection::open(&index_path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    ensure_refreshable_index(&connection)?;
    let indexed = read_index_fingerprints(&connection)?;

    let mut stats = SearchRefreshStats {
        conversations: targets.len(),
        ..SearchRefreshStats::default()
    };
    let transaction = connection.transaction()?;
    let mut next_rowid: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(rowid), 0) + 1 FROM message_meta",
        [],
        |row| row.get(0),
    )?;
    let mut token_delta: HashMap<String, i64> = HashMap::new();
    {
        let mut delete_fts = transaction.prepare("DELETE FROM message_fts WHERE username = ?1")?;
        let mut delete_meta =
            transaction.prepare("DELETE FROM message_meta WHERE username = ?1")?;
        let mut insert_fts = transaction.prepare(INSERT_FTS_SQL)?;
        let mut insert_meta = transaction.prepare(INSERT_META_SQL)?;

        for target in &targets {
            let mut fingerprint = ConversationFingerprint::default();
            for_each_document(&store, target, |document| {
                fingerprint.observe(document);
                Ok(())
            })?;
            let previous = indexed.get(&target.username).copied();
            if previous == Some(fingerprint) {
                continue;
            }
            if previous.is_none() && fingerprint.count == 0 {
                continue;
            }

            let texts = indexed_texts(&transaction, &target.username)?;
            for text in &texts {
                for token in single_char_tokens(text) {
                    *token_delta.entry(token).or_insert(0) -= 1;
                }
            }
            stats.removed_messages += texts.len() as i64;
            delete_fts.execute([target.username.as_str()])?;
            delete_meta.execute([target.username.as_str()])?;

            for_each_document(&store, target, |document| {
                for token in single_char_tokens(&document.text) {
                    *token_delta.entry(token).or_insert(0) += 1;
                }
                insert_document(&mut insert_fts, &mut insert_meta, next_rowid, document)?;
                next_rowid += 1;
                stats.inserted_messages += 1;
                Ok(())
            })?;
            stats.refreshed_conversations += 1;
        }

        for username in indexed.keys() {
            if target_names.contains(username.as_str()) {
                continue;
            }
            let texts = indexed_texts(&transaction, username)?;
            for text in &texts {
                for token in single_char_tokens(text) {
                    *token_delta.entry(token).or_insert(0) -= 1;
                }
            }
            stats.removed_messages += texts.len() as i64;
            delete_fts.execute([username.as_str()])?;
            delete_meta.execute([username.as_str()])?;
            stats.removed_conversations += 1;
        }
    }

    apply_token_deltas(&transaction, &token_delta)?;
    stats.message_count =
        transaction.query_row("SELECT COUNT(*) FROM message_meta", [], |row| row.get(0))?;
    stats.unique_single_char_tokens =
        transaction.query_row("SELECT COUNT(*) FROM message_token_stats", [], |row| {
            row.get(0)
        })?;
    let now = current_unix_seconds();
    for (key, value) in [
        ("built_at", now.to_string()),
        ("last_refresh_at", now.to_string()),
        ("message_count", stats.message_count.to_string()),
    ] {
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }
    transaction.commit()?;
    Ok(stats)
}

fn ensure_refreshable_index(connection: &Connection) -> Result<()> {
    for table in ["meta", "message_fts", "message_meta", "message_token_stats"] {
        if !table_exists(connection, table)? {
            return Err(DataError::NotFound(format!(
                "search index is incomplete (missing {table}); rebuild it"
            )));
        }
    }
    let version: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match version.and_then(|value| value.parse::<i64>().ok()) {
        Some(value) if value == SEARCH_INDEX_SCHEMA_VERSION => Ok(()),
        Some(value) => Err(DataError::InvalidAccount(format!(
            "search index schema {value} does not match {SEARCH_INDEX_SCHEMA_VERSION}; rebuild it"
        ))),
        None => Err(DataError::InvalidAccount(
            "search index has no schema version; rebuild it".into(),
        )),
    }
}

fn read_index_fingerprints(
    connection: &Connection,
) -> Result<HashMap<String, ConversationFingerprint>> {
    let mut statement = connection.prepare(
        "SELECT username, COUNT(*), COALESCE(MAX(local_id), 0), COALESCE(SUM(local_id), 0), \
         COALESCE(MAX(create_time), 0), COALESCE(MAX(sort_seq), 0), \
         COALESCE(SUM(LENGTH(CAST(text AS BLOB))), 0), COALESCE(SUM(local_type), 0) \
         FROM message_meta GROUP BY username",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            ConversationFingerprint {
                count: row.get(1)?,
                max_local_id: row.get(2)?,
                sum_local_id: row.get(3)?,
                max_create_time: row.get(4)?,
                max_sort_seq: row.get(5)?,
                sum_text_len: row.get(6)?,
                sum_local_type: row.get(7)?,
            },
        ))
    })?;
    let mut result = HashMap::new();
    for row in rows {
        let (username, fingerprint) = row?;
        result.insert(username, fingerprint);
    }
    Ok(result)
}

fn indexed_texts(connection: &Connection, username: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare("SELECT text FROM message_meta WHERE username = ?1")?;
    let rows = statement.query_map([username], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn apply_token_deltas(connection: &Connection, deltas: &HashMap<String, i64>) -> Result<()> {
    if deltas.is_empty() {
        return Ok(());
    }
    let mut bump = connection.prepare(
        "INSERT INTO message_token_stats(token, doc_count) VALUES(?1, ?2) \
         ON CONFLICT(token) DO UPDATE SET doc_count = message_token_stats.doc_count + excluded.doc_count",
    )?;
    let mut lower = connection
        .prepare("UPDATE message_token_stats SET doc_count = doc_count + ?2 WHERE token = ?1")?;
    let mut prune = connection
        .prepare("DELETE FROM message_token_stats WHERE token = ?1 AND doc_count <= 0")?;
    for (token, delta) in deltas {
        if *delta > 0 {
            bump.execute(params![token, delta])?;
        } else if *delta < 0 {
            lower.execute(params![token, delta])?;
            prune.execute([token])?;
        }
    }
    Ok(())
}

fn for_each_document(
    store: &AccountStore,
    target: &SearchTarget,
    mut visit: impl FnMut(&SearchDocument) -> Result<()>,
) -> Result<()> {
    let mut before = None;
    loop {
        let page = store.load_messages(
            &target.username,
            &MessageQuery {
                limit: 500,
                before: before.clone(),
            },
        )?;
        for message in &page.messages {
            let document = document_from_message(message, target);
            if document.text.is_empty() {
                continue;
            }
            visit(&document)?;
        }
        if !page.has_more {
            break;
        }
        before = page.next_before;
        if before.is_none() {
            break;
        }
    }
    Ok(())
}

fn insert_document(
    insert_fts: &mut rusqlite::Statement<'_>,
    insert_meta: &mut rusqlite::Statement<'_>,
    rowid: i64,
    document: &SearchDocument,
) -> Result<()> {
    insert_fts.execute(params![
        rowid,
        &document.text,
        &document.username,
        &document.render_type,
        document.create_time,
        document.sort_seq,
        document.local_id,
        document.server_id,
        document.local_type,
        &document.db_stem,
        &document.table_name,
        &document.sender_username,
        document.is_hidden,
        document.is_official,
        document.payload.to_string(),
    ])?;
    insert_meta.execute(params![
        rowid,
        &document.text,
        &document.username,
        &document.render_type,
        document.create_time,
        document.sort_seq,
        document.local_id,
        document.server_id,
        document.local_type,
        &document.db_stem,
        &document.table_name,
        &document.sender_username,
        document.is_hidden,
        document.is_official,
    ])?;
    Ok(())
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or_default()
}

fn init_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
             text,
             username UNINDEXED,
             render_type UNINDEXED,
             create_time UNINDEXED,
             sort_seq UNINDEXED,
             local_id UNINDEXED,
             server_id UNINDEXED,
             local_type UNINDEXED,
             db_stem UNINDEXED,
             table_name UNINDEXED,
             sender_username UNINDEXED,
             is_hidden UNINDEXED,
             is_official UNINDEXED,
             payload_json UNINDEXED,
             tokenize='unicode61'
         );
         CREATE TABLE IF NOT EXISTS message_meta (
             rowid INTEGER PRIMARY KEY,
             text TEXT NOT NULL,
             username TEXT NOT NULL,
             render_type TEXT NOT NULL,
             create_time INTEGER NOT NULL DEFAULT 0,
             sort_seq INTEGER NOT NULL DEFAULT 0,
             local_id INTEGER NOT NULL DEFAULT 0,
             server_id INTEGER NOT NULL DEFAULT 0,
             local_type INTEGER NOT NULL DEFAULT 0,
             db_stem TEXT NOT NULL,
             table_name TEXT NOT NULL,
             sender_username TEXT NOT NULL,
             is_hidden INTEGER NOT NULL DEFAULT 0,
             is_official INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS message_token_stats (
             token TEXT PRIMARY KEY,
             doc_count INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
struct SearchTarget {
    username: String,
    is_hidden: i64,
    is_official: i64,
}

fn collect_targets(store: &AccountStore) -> Result<Vec<SearchTarget>> {
    let mut targets: Vec<SearchTarget> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for session in store.list_sessions(true, true)? {
        if !seen.insert(session.username.clone()) {
            continue;
        }
        targets.push(SearchTarget {
            is_hidden: i64::from(session.is_hidden),
            is_official: i64::from(session.is_official),
            username: session.username,
        });
    }

    let self_aliases: HashSet<String> = [
        store.account().to_string(),
        store.self_username().to_string(),
    ]
    .into_iter()
    .filter(|value| !value.is_empty())
    .collect();
    let contacts = store.load_contact_map()?;
    for username in contacts.keys() {
        add_message_backed_target(store, &mut targets, &mut seen, &self_aliases, username)?;
    }

    for shard in store.shards() {
        let Ok(connection) = open_read_only(&shard.path) else {
            continue;
        };
        if !table_exists(&connection, "Name2Id")? {
            continue;
        }
        let columns = table_columns(&connection, "Name2Id")?;
        let columns = column_set(&columns);
        let column = if columns.contains("user_name") {
            "user_name"
        } else if columns.contains("username") {
            "username"
        } else {
            continue;
        };
        let sql = format!("SELECT {} FROM Name2Id", crate::sqlite::quote_ident(column));
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query([])?;
        let mut usernames = Vec::new();
        while let Some(row) = rows.next()? {
            let username = row_text(row, 0).trim().to_string();
            if !username.is_empty() {
                usernames.push(username);
            }
        }
        drop(rows);
        drop(statement);
        for username in usernames {
            add_message_backed_target(store, &mut targets, &mut seen, &self_aliases, &username)?;
        }
    }
    Ok(targets)
}

fn add_message_backed_target(
    store: &AccountStore,
    targets: &mut Vec<SearchTarget>,
    seen: &mut HashSet<String>,
    self_aliases: &HashSet<String>,
    username: &str,
) -> Result<()> {
    let username = username.trim();
    if username.is_empty() || self_aliases.contains(username) || seen.contains(username) {
        return Ok(());
    }
    if !crate::session::should_keep_session(username, true) {
        return Ok(());
    }
    if !store
        .shards()
        .iter()
        .any(|shard| shard.resolve_table(username).is_some())
    {
        return Ok(());
    }
    seen.insert(username.to_string());
    targets.push(SearchTarget {
        username: username.to_string(),
        is_hidden: 0,
        is_official: i64::from(username.starts_with("gh_")),
    });
    Ok(())
}

fn single_char_tokens(text: &str) -> HashSet<String> {
    text.split_whitespace()
        .filter(|token| token.chars().count() == 1)
        .map(str::to_string)
        .collect()
}

fn document_from_message(message: &Message, target: &SearchTarget) -> SearchDocument {
    let mut render_type = kind_render_type(message.kind).to_string();
    let mut content: String;
    let mut title = String::new();
    let mut url = String::new();
    let mut quote_title = String::new();
    let mut quote_content = String::new();
    let mut amount = String::new();
    let mut location_poiname = String::new();
    let mut location_label = String::new();

    match &message.body {
        Some(MessageBody::Text { content: value }) => {
            render_type = "text".to_string();
            content = value.clone();
        }
        Some(MessageBody::Link {
            content: value,
            title: link_title,
            url: link_url,
            link_type,
            ..
        }) => {
            render_type = "link".to_string();
            content = value.clone();
            title = link_title.clone();
            url = link_url.clone();
            if link_type.is_empty() {
                render_type = "text".to_string();
            }
        }
        Some(MessageBody::ChatHistory {
            content: value,
            title: chat_title,
            ..
        }) => {
            render_type = "chatHistory".to_string();
            content = value.clone();
            title = chat_title.clone();
        }
        Some(MessageBody::File {
            content: value,
            title: file_title,
            ..
        }) => {
            render_type = "file".to_string();
            content = value.clone();
            title = file_title.clone();
        }
        Some(MessageBody::Quote {
            content: value,
            quote_title: quoted_title,
            quote_content: quoted_content,
            ..
        }) => {
            render_type = "quote".to_string();
            content = value.clone();
            quote_title = quoted_title.clone();
            quote_content = quoted_content.clone();
        }
        Some(MessageBody::Transfer {
            content: value,
            title: transfer_title,
            amount: transfer_amount,
            ..
        }) => {
            render_type = "transfer".to_string();
            content = value.clone();
            title = transfer_title.clone();
            amount = transfer_amount.clone();
        }
        Some(MessageBody::RedPacket {
            content: value,
            title: packet_title,
            ..
        }) => {
            render_type = "redPacket".to_string();
            content = value.clone();
            title = packet_title.clone();
        }
        Some(MessageBody::Location {
            content: value,
            location_poiname: poi,
            location_label: label,
            ..
        }) => {
            render_type = "location".to_string();
            content = value.clone();
            location_poiname = poi.clone();
            location_label = label.clone();
        }
        Some(MessageBody::Voip { content: value, .. }) => {
            render_type = "voip".to_string();
            content = value.clone();
        }
        Some(MessageBody::System { content: value }) => {
            render_type = "system".to_string();
            content = if message.local_type == 266_287_972_401 {
                "[拍一拍]".to_string()
            } else {
                value.clone()
            };
        }
        None => match message.kind {
            MessageKind::Image => content = "[图片]".to_string(),
            MessageKind::Voice => {
                let duration = extract_attr(&message.content, "voicelength");
                content = if duration.is_empty() {
                    "[语音]".to_string()
                } else {
                    format!("[语音 {duration}秒]")
                };
            }
            MessageKind::Video => content = "[视频]".to_string(),
            MessageKind::Emoji => content = "[表情]".to_string(),
            MessageKind::Voip => content = "[通话]".to_string(),
            _ => content = message.content.clone(),
        },
    }

    if content.is_empty() {
        content = message.content.clone();
    }
    if content.is_empty() {
        content = format!("[{}]", render_type);
    }

    let haystack: Vec<&str> = [
        content.as_str(),
        title.as_str(),
        url.as_str(),
        quote_title.as_str(),
        quote_content.as_str(),
        amount.as_str(),
    ]
    .into_iter()
    .filter(|value| !value.trim().is_empty())
    .collect();
    let text = char_token_text(&haystack.join("\n"));
    let db_stem = Path::new(&message.shard)
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| message.shard.clone());

    let payload = serde_json::json!({
        "id": format!("{}:{}:{}", db_stem, message.table, message.local_id),
        "db": db_stem,
        "table": message.table,
        "username": target.username,
        "localId": message.local_id,
        "serverId": message.server_id,
        "type": message.local_type,
        "renderType": render_type,
        "createTime": message.create_time,
        "sortSeq": message.sort_seq,
        "senderUsername": message.sender_username,
        "isSent": message.is_sent,
        "content": content,
        "title": title,
        "url": url,
        "quoteTitle": quote_title,
        "quoteContent": quote_content,
        "amount": amount,
        "locationPoiname": location_poiname,
        "locationLabel": location_label,
        "source": "decrypted",
    });

    SearchDocument {
        text,
        username: target.username.clone(),
        render_type,
        create_time: message.create_time,
        sort_seq: message.sort_seq,
        local_id: message.local_id,
        server_id: message.server_id,
        local_type: message.local_type,
        db_stem,
        table_name: message.table.clone(),
        sender_username: message.sender_username.clone(),
        is_hidden: target.is_hidden,
        is_official: target.is_official,
        payload,
    }
}

fn kind_render_type(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Text => "text",
        MessageKind::Image => "image",
        MessageKind::Voice => "voice",
        MessageKind::Video => "video",
        MessageKind::Emoji => "emoji",
        MessageKind::Location => "location",
        MessageKind::App => "link",
        MessageKind::Voip => "voip",
        MessageKind::System => "system",
        MessageKind::Quote => "quote",
        MessageKind::Pat => "system",
        MessageKind::File => "file",
        MessageKind::Live => "link",
        MessageKind::Other => "text",
    }
}

pub fn search_index(account_dir: &Path, query: &SearchQuery) -> Result<SearchPage> {
    let fts_query = build_fts_query(&query.q);
    if fts_query.is_empty() {
        return Ok(SearchPage {
            hits: Vec::new(),
            has_more: false,
            total_estimate: 0,
        });
    }
    let limit = query.limit.clamp(1, 200);
    let offset = query.offset;
    let path = chat_search_index_path(account_dir);
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let mut where_parts = vec!["message_fts MATCH ?1".to_string()];
    let mut parameters: Vec<rusqlite::types::Value> = vec![fts_query.into()];
    let mut bind = |value: rusqlite::types::Value, predicate: &str, parts: &mut Vec<String>| {
        parameters.push(value);
        parts.push(format!("{predicate} ?{}", parameters.len()));
    };
    if let Some(username) = query
        .username
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        bind(username.to_string().into(), "username =", &mut where_parts);
    }
    if let Some(start_ts) = query.start_ts {
        bind(
            start_ts.into(),
            "CAST(create_time AS INTEGER) >=",
            &mut where_parts,
        );
    }
    if let Some(end_ts) = query.end_ts {
        bind(
            end_ts.into(),
            "CAST(create_time AS INTEGER) <=",
            &mut where_parts,
        );
    }
    if !query.include_hidden {
        where_parts.push("CAST(is_hidden AS INTEGER) = 0".to_string());
    }
    if !query.include_official {
        where_parts.push("CAST(is_official AS INTEGER) = 0".to_string());
    }

    let sql = format!(
        "SELECT username, db_stem, table_name, local_id, server_id, local_type, sender_username, \
                render_type, create_time, sort_seq, payload_json \
         FROM message_fts WHERE {} \
         ORDER BY CAST(create_time AS INTEGER) DESC, CAST(sort_seq AS INTEGER) DESC, \
                  CAST(local_id AS INTEGER) DESC LIMIT ?{} OFFSET ?{}",
        where_parts.join(" AND "),
        parameters.len() + 1,
        parameters.len() + 2
    );
    parameters.push(i64::try_from(limit + 1).unwrap_or(i64::MAX).into());
    parameters.push(i64::try_from(offset).unwrap_or(i64::MAX).into());

    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(rusqlite::params_from_iter(parameters))?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        let payload_text: String = row.get(10).unwrap_or_default();
        hits.push(SearchHit {
            username: row.get(0).unwrap_or_default(),
            db_stem: row.get(1).unwrap_or_default(),
            table_name: row.get(2).unwrap_or_default(),
            local_id: row.get(3).unwrap_or(0),
            server_id: row.get(4).unwrap_or(0),
            local_type: row.get(5).unwrap_or(0),
            sender_username: row.get(6).unwrap_or_default(),
            render_type: row.get(7).unwrap_or_default(),
            create_time: row.get(8).unwrap_or(0),
            sort_seq: row.get(9).unwrap_or(0),
            payload: serde_json::from_str(&payload_text).unwrap_or(Value::Null),
        });
    }
    let has_more = hits.len() > limit;
    hits.truncate(limit);
    let total_estimate = offset as i64 + hits.len() as i64 + i64::from(has_more);
    Ok(SearchPage {
        hits,
        has_more,
        total_estimate,
    })
}
