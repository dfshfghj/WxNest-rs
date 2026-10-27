use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::contact::{
    display_name, is_group_username, is_official_username, normalize_avatar_url,
    should_keep_session,
};
use crate::contacts::{contact_matches_keyword, normalize_contact, ContactRow};
use crate::decode::decode_message_content;
use crate::error::{DataError, Result};
use crate::general::{
    parse_finder_lives, parse_mini_programs, parse_payments, query_rows, FinderLive, MiniProgram,
    PaymentItem,
};
use crate::message::{load_messages_for_store, resolve_self_rowid, resolve_table_name, MessageQuery};
use crate::model::{AccountInfo, AvatarImage, Contact, ContactItem, MessagePage, Session};
use crate::sns::{parse_sns_post, SnsPost};
use crate::sqlite::{
    column_set, open_read_only, quote_ident, row_i64, row_optional_i64, row_text, row_value,
    table_columns, table_exists, table_has_column, table_names,
};
use crate::time::format_session_time;

const SKIPPED_MESSAGE_DATABASES: &[&str] = &[
    "session.db",
    "contact.db",
    "head_image.db",
    "message_resource.db",
    "message_fts.db",
    "chat_search_index.db",
    "chat_search_index.tmp.db",
    "session_last_message.db",
];

pub(crate) struct MessageShard {
    pub path: PathBuf,
    pub name: String,
    pub tables: Vec<String>,
    pub self_rowid: Option<i64>,
}

impl MessageShard {
    pub(crate) fn resolve_table(&self, username: &str) -> Option<String> {
        resolve_table_name(&self.tables, username)
    }
}

pub struct AccountStore {
    account_dir: PathBuf,
    account: String,
    self_username: String,
    self_candidates: Vec<String>,
    shards: Vec<MessageShard>,
    session_db: PathBuf,
    contact_db: PathBuf,
    head_image_db: PathBuf,
}

impl AccountStore {
    pub fn open(account_dir: &Path) -> Result<Self> {
        let account_dir = std::fs::canonicalize(account_dir)?;
        if !account_dir.is_dir() {
            return Err(DataError::InvalidAccount(format!(
                "not a directory: {}",
                account_dir.display()
            )));
        }
        let account = account_dir
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        let self_candidates = account_identity_candidates(&account_dir);
        let self_username = resolve_account_self_username(&account_dir);
        let mut shards = Vec::new();
        for path in discover_message_shard_paths(&account_dir) {
            let Ok(connection) = open_read_only(&path) else {
                continue;
            };
            let Ok(tables) = table_names(&connection) else {
                continue;
            };
            let self_rowid = resolve_self_rowid(&connection, &self_candidates);
            shards.push(MessageShard {
                name: path
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_default(),
                path,
                tables,
                self_rowid,
            });
        }
        Ok(Self {
            session_db: account_dir.join("session.db"),
            contact_db: account_dir.join("contact.db"),
            head_image_db: account_dir.join("head_image.db"),
            account_dir,
            account,
            self_username,
            self_candidates,
            shards,
        })
    }

    pub fn account_dir(&self) -> &Path {
        &self.account_dir
    }

    pub(crate) fn contact_db_path(&self) -> &Path {
        &self.contact_db
    }

    pub fn list_contact_directory(&self, keyword: &str, limit: usize) -> Result<Vec<ContactItem>> {
        if !self.contact_db.is_file() {
            return Ok(Vec::new());
        }
        let connection = open_read_only(&self.contact_db)?;
        if !table_exists(&connection, "contact")? {
            return Ok(Vec::new());
        }
        let columns = column_set(&table_columns(&connection, "contact")?);
        if !columns.contains("username") {
            return Ok(Vec::new());
        }
        let column = |name: &str| {
            if columns.contains(name) {
                quote_ident(name)
            } else {
                "''".to_string()
            }
        };
        let local_type = if columns.contains("local_type") {
            quote_ident("local_type")
        } else {
            "1".to_string()
        };
        let flag = if columns.contains("flag") {
            quote_ident("flag")
        } else {
            "0".to_string()
        };
        let sql = format!(
            "SELECT {} AS username, {} AS remark, {} AS nick_name, {} AS alias, \
             {local_type} AS local_type, {flag} AS flag, {} AS description, {} AS pinyin \
             FROM contact LIMIT 30000",
            quote_ident("username"),
            column("remark"),
            column("nick_name"),
            column("alias"),
            column("description"),
            column("pin_yin_initial"),
        );
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query([])?;
        let mut output = Vec::new();
        let limit = limit.clamp(1, 5000);
        while let Some(row) = rows.next()? {
            let entry = normalize_contact(
                ContactRow {
                    username: row_text(row, 0),
                    remark: row_text(row, 1),
                    nick_name: row_text(row, 2),
                    alias: row_text(row, 3),
                    local_type: row_i64(row, 4),
                    flag: row_i64(row, 5),
                    description: row_text(row, 6),
                    pinyin: row_text(row, 7),
                },
                &self.self_candidates,
            );
            let Some(entry) = entry else {
                continue;
            };
            if !contact_matches_keyword(&entry, keyword) {
                continue;
            }
            output.push(entry);
            if output.len() >= limit {
                break;
            }
        }
        output.sort_by(|left, right| {
            left.pinyin_initial
                .cmp(&right.pinyin_initial)
                .then_with(|| left.display_name.cmp(&right.display_name))
        });
        Ok(output)
    }

    pub fn list_finder_lives(&self) -> Result<Vec<FinderLive>> {
        let path = self.account_dir.join("general.db");
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let connection = open_read_only(&path)?;
        if !table_exists(&connection, "wcfinderlivestatus")? {
            return Ok(Vec::new());
        }
        let statuses = query_rows(
            &connection,
            "SELECT finder_live_id, finder_username, finder_export_id, live_status, \
             replay_status, charge_flag FROM wcfinderlivestatus \
             ORDER BY live_status DESC, finder_live_id DESC LIMIT 500",
        )?;
        let users = if table_exists(&connection, "wcfinderuserpage")? {
            query_rows(
                &connection,
                "SELECT username, extra_buffer FROM wcfinderuserpage LIMIT 20000",
            )?
        } else {
            Vec::new()
        };
        Ok(parse_finder_lives(&statuses, &users))
    }

    pub fn list_mini_programs(&self) -> Result<Vec<MiniProgram>> {
        let path = self.account_dir.join("general.db");
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let connection = open_read_only(&path)?;
        if !table_exists(&connection, "wacontact")? {
            return Ok(Vec::new());
        }
        let join = if table_exists(&connection, "WeAppBizAttrSyncBufferTableV02")? {
            "LEFT JOIN WeAppBizAttrSyncBufferTableV02 a ON a.user_name = w.user_name"
        } else {
            ""
        };
        let update = if join.is_empty() {
            "0"
        } else {
            "COALESCE(a.last_update_time, 0)"
        };
        let sql = format!(
            "SELECT w.user_name, w.type, w.brand_icon_url, w.external_info, w.app_id, \
             {update} AS last_update_time FROM wacontact w {join} \
             ORDER BY {update} DESC, w.user_name ASC LIMIT 2000"
        );
        Ok(parse_mini_programs(&query_rows(&connection, &sql)?))
    }

    pub fn list_payments(&self) -> Result<Vec<PaymentItem>> {
        let path = self.account_dir.join("general.db");
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let connection = open_read_only(&path)?;
        let transfers = if table_exists(&connection, "transferTable")? {
            query_rows(
                &connection,
                "SELECT transfer_id, session_name, pay_sub_type, pay_payer, begin_transfer_time, \
                 last_modified_time, invalid_time FROM transferTable \
                 ORDER BY begin_transfer_time DESC LIMIT 1000",
            )?
        } else {
            Vec::new()
        };
        let red_packets = if table_exists(&connection, "redEnvelopeTable")? {
            query_rows(
                &connection,
                "SELECT message_server_id, session_name, sender_user_name, hb_status, \
                 receive_status FROM redEnvelopeTable LIMIT 1000",
            )?
        } else {
            Vec::new()
        };
        Ok(parse_payments(&transfers, &red_packets))
    }

    pub fn list_sns_posts(&self, before_tid: Option<i64>, limit: usize) -> Result<Vec<SnsPost>> {
        let path = self.account_dir.join("sns.db");
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let connection = open_read_only(&path)?;
        if !table_exists(&connection, "SnsTimeLine")? {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 200);
        let mut sql = "SELECT tid, content FROM SnsTimeLine \
             WHERE content IS NOT NULL AND content != '' \
             AND content NOT LIKE '%<type>7</type>%'"
            .to_string();
        let mut params: Vec<i64> = Vec::new();
        if let Some(before) = before_tid {
            sql.push_str(" AND tid < ?1");
            params.push(before);
        }
        sql.push_str(&format!(" ORDER BY tid DESC LIMIT ?{}", params.len() + 1));
        params.push((limit + 1) as i64);
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(params))?;
        let mut posts = Vec::new();
        while let Some(row) = rows.next()? {
            let tid = row_i64(row, 0);
            let content = row_text(row, 1);
            let Some(post) = parse_sns_post(tid, &content) else {
                continue;
            };
            posts.push(post);
            if posts.len() >= limit {
                break;
            }
        }
        if let Ok(contacts) = self.load_contact_map() {
            for post in &mut posts {
                if let Some(contact) = contacts.get(&post.username) {
                    if !contact.display_name.trim().is_empty() {
                        post.display_name = contact.display_name.clone();
                    }
                }
            }
        }
        Ok(posts)
    }

    pub fn group_nicknames(
        &self,
        chatroom: &str,
        usernames: &[String],
    ) -> Result<HashMap<String, String>> {
        crate::group::load_group_nickname_map(&self.contact_db, chatroom, usernames)
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn self_username(&self) -> &str {
        &self.self_username
    }

    pub fn self_candidates(&self) -> &[String] {
        &self.self_candidates
    }

    pub fn account_info(&self) -> AccountInfo {
        AccountInfo {
            account: self.account.clone(),
            account_dir: self.account_dir.display().to_string(),
            message_shards: self.shards.iter().map(|shard| shard.name.clone()).collect(),
        }
    }

    pub(crate) fn shards(&self) -> &[MessageShard] {
        &self.shards
    }

    pub fn load_messages(&self, username: &str, query: &MessageQuery) -> Result<MessagePage> {
        load_messages_for_store(self, username, query)
    }

    pub fn count_messages(&self, username: &str) -> Result<i64> {
        let mut total = 0i64;
        for shard in &self.shards {
            let Some(table) = shard.resolve_table(username) else {
                continue;
            };
            let connection = open_read_only(&shard.path)?;
            let count: i64 = connection.query_row(
                &format!("SELECT COUNT(*) FROM {}", quote_ident(&table)),
                [],
                |row| row.get(0),
            )?;
            total += count;
        }
        Ok(total)
    }

    pub fn list_sessions(
        &self,
        include_hidden: bool,
        include_official: bool,
    ) -> Result<Vec<Session>> {
        if !self.session_db.is_file() {
            return Ok(Vec::new());
        }
        let connection = open_read_only(&self.session_db)?;
        if !table_exists(&connection, "SessionTable")? {
            return Ok(Vec::new());
        }
        let columns = table_columns(&connection, "SessionTable")?;
        let columns = column_set(&columns);
        if !table_has_column(&columns, "username") {
            return Ok(Vec::new());
        }

        let select = |name: &str, fallback: &str| {
            if table_has_column(&columns, name) {
                quote_ident(name)
            } else {
                fallback.to_string()
            }
        };
        let order = if table_has_column(&columns, "sort_timestamp") {
            format!("ORDER BY {} DESC", quote_ident("sort_timestamp"))
        } else if table_has_column(&columns, "last_timestamp") {
            format!("ORDER BY {} DESC", quote_ident("last_timestamp"))
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT {} AS username, {} AS is_hidden, {} AS summary, {} AS draft, \
                    {} AS last_msg_type, {} AS last_msg_sub_type, {} AS sort_timestamp, \
                    {} AS last_timestamp \
             FROM {} {order}",
            quote_ident("username"),
            select("is_hidden", "0"),
            select("summary", "NULL"),
            select("draft", "NULL"),
            select("last_msg_type", "NULL"),
            select("last_msg_sub_type", "NULL"),
            select("sort_timestamp", "NULL"),
            select("last_timestamp", "NULL"),
            quote_ident("SessionTable")
        );

        let mut raw = Vec::new();
        {
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let username = row_text(row, 0).trim().to_string();
                if username.is_empty() {
                    continue;
                }
                let is_hidden = row_i64(row, 1) == 1;
                if !include_hidden && is_hidden {
                    continue;
                }
                if !should_keep_session(&username, include_official) {
                    continue;
                }
                raw.push((
                    username,
                    is_hidden,
                    row_value(row, 2),
                    row_value(row, 3),
                    row_optional_i64(row, 4),
                    row_optional_i64(row, 5),
                    row_optional_i64(row, 6),
                    row_optional_i64(row, 7),
                ));
            }
        }
        drop(connection);

        let usernames: Vec<String> = raw.iter().map(|entry| entry.0.clone()).collect();
        let contacts = self.load_contacts_for(&usernames)?;
        let avatars = self.load_avatar_usernames()?;
        let mut sessions = Vec::with_capacity(raw.len());
        for (
            username,
            is_hidden,
            summary_value,
            draft_value,
            last_msg_type,
            last_msg_sub_type,
            sort_timestamp,
            last_timestamp,
        ) in raw
        {
            let summary = {
                let decoded = decode_message_content(None, &summary_value);
                if decoded.is_empty() {
                    summary_value.text_lossy()
                } else {
                    decoded
                }
            };
            let contact = contacts.get(&username);
            let display = contact
                .map(|value| value.display_name.clone())
                .unwrap_or_else(|| username.clone());
            let timestamp = sort_timestamp.or(last_timestamp);
            let has_avatar = avatars.contains_key(&username);
            sessions.push(Session {
                display_name: display,
                is_group: is_group_username(&username),
                is_official: is_official_username(&username),
                is_hidden,
                username,
                summary,
                draft: draft_value.text_lossy(),
                last_msg_type,
                last_msg_sub_type,
                sort_timestamp,
                last_timestamp,
                last_time_text: format_session_time(timestamp),
                has_avatar,
            });
        }
        Ok(sessions)
    }

    pub fn list_contacts(&self) -> Result<Vec<Contact>> {
        let mut contacts: Vec<Contact> = self.load_contact_map()?.into_values().collect();
        contacts.sort_by(|left, right| left.username.cmp(&right.username));
        Ok(contacts)
    }

    pub fn get_contact(&self, username: &str) -> Result<Option<Contact>> {
        Ok(self.load_contact_map()?.remove(username))
    }

    pub fn load_contacts_for(&self, usernames: &[String]) -> Result<HashMap<String, Contact>> {
        if !self.contact_db.is_file() {
            return Ok(HashMap::new());
        }
        let mut wanted: Vec<String> = usernames
            .iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
        wanted.sort();
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let connection = open_read_only(&self.contact_db)?;
        let mut result = HashMap::new();
        for table in ["contact", "stranger"] {
            if !table_exists(&connection, table)? {
                continue;
            }
            let columns = column_set(&table_columns(&connection, table)?);
            if !table_has_column(&columns, "username") {
                continue;
            }
            let select = |name: &str, fallback: &str| {
                if table_has_column(&columns, name) {
                    quote_ident(name)
                } else {
                    fallback.to_string()
                }
            };
            let base_sql = format!(
                "SELECT {} AS username, {} AS remark, {} AS nick_name, {} AS alias FROM {}",
                quote_ident("username"),
                select("remark", "NULL"),
                select("nick_name", "NULL"),
                select("alias", "NULL"),
                quote_ident(table)
            );
            for chunk in wanted.chunks(300) {
                let placeholders = (1..=chunk.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "{base_sql} WHERE {} IN ({placeholders})",
                    quote_ident("username")
                );
                let mut statement = connection.prepare(&sql)?;
                let mut rows = statement.query(rusqlite::params_from_iter(chunk.iter()))?;
                while let Some(row) = rows.next()? {
                    let username = row_text(row, 0).trim().to_string();
                    if username.is_empty() || result.contains_key(&username) {
                        continue;
                    }
                    let remark = row_text(row, 1);
                    let nick_name = row_text(row, 2);
                    let alias = row_text(row, 3);
                    result.insert(
                        username.clone(),
                        Contact {
                            display_name: display_name(&remark, &nick_name, &alias, &username),
                            username: username.clone(),
                            remark,
                            nick_name,
                            alias,
                            big_head_url: None,
                            small_head_url: None,
                            is_group: is_group_username(&username),
                            is_official: is_official_username(&username),
                        },
                    );
                }
            }
        }
        Ok(result)
    }

    pub fn load_contact_map(&self) -> Result<HashMap<String, Contact>> {
        let mut result = HashMap::new();
        if !self.contact_db.is_file() {
            return Ok(result);
        }
        let connection = open_read_only(&self.contact_db)?;
        for table in ["contact", "stranger"] {
            if !table_exists(&connection, table)? {
                continue;
            }
            let columns = table_columns(&connection, table)?;
            let columns = column_set(&columns);
            if !table_has_column(&columns, "username") {
                continue;
            }
            let select = |name: &str, fallback: &str| {
                if table_has_column(&columns, name) {
                    quote_ident(name)
                } else {
                    fallback.to_string()
                }
            };
            let sql = format!(
                "SELECT {} AS username, {} AS remark, {} AS nick_name, {} AS alias, \
                        {} AS big_head_url, {} AS small_head_url FROM {}",
                quote_ident("username"),
                select("remark", "NULL"),
                select("nick_name", "NULL"),
                select("alias", "NULL"),
                select("big_head_url", "NULL"),
                select("small_head_url", "NULL"),
                quote_ident(table)
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let username = row_text(row, 0).trim().to_string();
                if username.is_empty() || result.contains_key(&username) {
                    continue;
                }
                let remark = row_text(row, 1).trim().to_string();
                let nick_name = row_text(row, 2).trim().to_string();
                let alias = row_text(row, 3).trim().to_string();
                let big_head_url = normalize_avatar_url(&row_text(row, 4));
                let small_head_url = normalize_avatar_url(&row_text(row, 5));
                result.insert(
                    username.clone(),
                    Contact {
                        display_name: display_name(&remark, &nick_name, &alias, &username),
                        username: username.clone(),
                        remark,
                        nick_name,
                        alias,
                        big_head_url,
                        small_head_url,
                        is_group: is_group_username(&username),
                        is_official: is_official_username(&username),
                    },
                );
            }
        }
        Ok(result)
    }

    pub fn load_avatar(&self, username: &str) -> Result<Option<AvatarImage>> {
        let username = username.trim();
        if username.is_empty() {
            return Ok(None);
        }
        let Some((connection, sql)) = self.avatar_query()? else {
            return Ok(None);
        };
        let mut statement = connection.prepare(&sql)?;
        read_avatar_row(&mut statement, username)
    }

    pub fn load_avatars(&self, usernames: &[String]) -> Result<HashMap<String, AvatarImage>> {
        let Some((connection, sql)) = self.avatar_query()? else {
            return Ok(HashMap::new());
        };
        let mut statement = connection.prepare(&sql)?;
        let mut result = HashMap::new();
        for username in usernames {
            let username = username.trim();
            if username.is_empty() {
                continue;
            }
            if let Some(avatar) = read_avatar_row(&mut statement, username)? {
                result.insert(avatar.username.clone(), avatar);
            }
        }
        Ok(result)
    }

    fn avatar_query(&self) -> Result<Option<(rusqlite::Connection, String)>> {
        if !self.head_image_db.is_file() {
            return Ok(None);
        }
        let connection = open_read_only(&self.head_image_db)?;
        if !table_exists(&connection, "head_image")? {
            return Ok(None);
        }
        let columns = column_set(&table_columns(&connection, "head_image")?);
        if !table_has_column(&columns, "username") {
            return Ok(None);
        }
        let Some(buffer_column) = ["image_buffer", "image", "img_buffer", "buffer"]
            .into_iter()
            .find(|name| table_has_column(&columns, name))
        else {
            return Ok(None);
        };
        let update_expr = if table_has_column(&columns, "update_time") {
            quote_ident("update_time")
        } else {
            "0".to_string()
        };
        let sort_expr = if table_has_column(&columns, "update_time") {
            quote_ident("update_time")
        } else {
            "0 + 0".to_string()
        };
        let md5_expr = if table_has_column(&columns, "md5") {
            quote_ident("md5")
        } else {
            "''".to_string()
        };
        let sql = format!(
            "SELECT {}, {}, {} FROM {} WHERE {} = ?1 ORDER BY {} DESC LIMIT 1",
            quote_ident(buffer_column),
            md5_expr,
            update_expr,
            quote_ident("head_image"),
            quote_ident("username"),
            sort_expr,
        );
        Ok(Some((connection, sql)))
    }

    fn load_avatar_usernames(&self) -> Result<HashMap<String, ()>> {
        let mut result = HashMap::new();
        if !self.head_image_db.is_file() {
            return Ok(result);
        }
        let connection = open_read_only(&self.head_image_db)?;
        if !table_exists(&connection, "head_image")? {
            return Ok(result);
        }
        let columns = table_columns(&connection, "head_image")?;
        if !column_set(&columns).contains("username") {
            return Ok(result);
        }
        let mut statement = connection.prepare(&format!(
            "SELECT {} FROM {}",
            quote_ident("username"),
            quote_ident("head_image")
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let username = row_text(row, 0).trim().to_string();
            if !username.is_empty() {
                result.insert(username, ());
            }
        }
        Ok(result)
    }
}

pub fn discover_accounts(root: &Path) -> Result<Vec<AccountInfo>> {
    let mut accounts = Vec::new();
    if !root.is_dir() {
        return Ok(accounts);
    }
    if has_any_database(root) {
        if let Ok(store) = AccountStore::open(root) {
            accounts.push(store.account_info());
        }
        return Ok(accounts);
    }

    let mut bases = vec![root.to_path_buf()];
    let export_databases = root.join("databases");
    if export_databases.is_dir() {
        bases.push(export_databases);
    }
    for base in bases {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !has_any_database(&path) {
                continue;
            }
            if let Ok(store) = AccountStore::open(&path) {
                let info = store.account_info();
                if !accounts
                    .iter()
                    .any(|existing: &AccountInfo| existing.account == info.account)
                {
                    accounts.push(info);
                }
            }
        }
    }
    accounts.sort_by(|left, right| left.account.cmp(&right.account));
    Ok(accounts)
}

fn has_any_database(path: &Path) -> bool {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.path().is_file()
                && entry
                    .path()
                    .extension()
                    .map(|value| value.to_string_lossy().eq_ignore_ascii_case("db"))
                    .unwrap_or(false)
        })
}

pub(crate) fn discover_message_shard_paths(account_dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(account_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_message_database_name(&name) {
                return None;
            }
            Some(path)
        })
        .collect();
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    paths
}

fn is_message_database_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if SKIPPED_MESSAGE_DATABASES.contains(&lower.as_str()) {
        return false;
    }
    if !lower.ends_with(".db") {
        return false;
    }
    if lower == "message.db" || lower == "biz_message.db" {
        return true;
    }
    if lower.starts_with("message_") || lower.starts_with("biz_message_") {
        return true;
    }
    lower.contains("message")
}

pub fn account_identity_candidates(account_dir: &Path) -> Vec<String> {
    let mut output = Vec::new();
    let account_info = read_json_object(&account_dir.join("account.json"));
    for key in ["username", "user_name", "wxid", "account"] {
        if let Some(value) = account_info.get(key).and_then(Value::as_str) {
            append_candidate(&mut output, value);
        }
    }
    let source_info = read_json_object(&account_dir.join("_source.json"));
    for key in ["native_wxid", "username", "user_name", "wxid", "account"] {
        if let Some(value) = source_info.get(key).and_then(Value::as_str) {
            append_candidate(&mut output, value);
        }
    }
    if let Some(value) = source_info.get("wxid_dir").and_then(Value::as_str) {
        append_path_candidate(&mut output, value, false);
    }
    if let Some(value) = source_info.get("db_storage_path").and_then(Value::as_str) {
        append_path_candidate(&mut output, value, true);
    }
    if let Some(original) = source_info.get("original_info").and_then(Value::as_object) {
        for key in [
            "username",
            "user_name",
            "wxid",
            "account",
            "account_dir",
            "wxid_dir",
        ] {
            if let Some(value) = original.get(key).and_then(Value::as_str) {
                append_candidate(&mut output, value);
            }
        }
    }
    if let Some(name) = account_dir.file_name() {
        append_candidate(&mut output, &name.to_string_lossy());
    }
    output
}

pub fn resolve_account_self_username(account_dir: &Path) -> String {
    let account_info = read_json_object(&account_dir.join("account.json"));
    for key in ["username", "user_name", "wxid", "account"] {
        if let Some(value) = account_info.get(key).and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    account_dir
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn read_json_object(path: &Path) -> serde_json::Map<String, Value> {
    std::fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn append_path_candidate(output: &mut Vec<String>, value: &str, db_storage: bool) {
    let raw = value.trim();
    if raw.is_empty() {
        return;
    }
    let normalized = raw.trim_end_matches(['/', '\\']).replace('\\', "/");
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let mut leaf = parts.last().copied().unwrap_or_default();
    if db_storage && leaf.eq_ignore_ascii_case("db_storage") && parts.len() > 1 {
        leaf = parts[parts.len() - 2];
    }
    append_candidate(output, leaf);
}

fn append_candidate(output: &mut Vec<String>, value: &str) {
    let raw = value.trim();
    if raw.is_empty() {
        return;
    }
    let mut values = vec![raw.to_string()];
    if raw.contains('/') || raw.contains('\\') {
        let leaf = raw
            .trim_end_matches(['/', '\\'])
            .replace('\\', "/")
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        values.insert(0, leaf);
    }
    for item in values {
        for candidate in [canonical_account_name(&item), item] {
            let candidate = candidate.trim().to_string();
            if !candidate.is_empty() && !output.contains(&candidate) {
                output.push(candidate);
            }
        }
    }
}

fn canonical_account_name(value: &str) -> String {
    let name = value.trim();
    if name.is_empty() {
        return String::new();
    }
    if let Some(captures) = backup_pattern().captures(name) {
        return captures[1].to_string();
    }
    if let Some(captures) = wxid_source_pattern().captures(name) {
        return captures[1].to_string();
    }
    name.to_string()
}

fn backup_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)^(.+)\.backup-\d{8}-\d{6}(?:-\d+)?$").expect("valid backup regex")
    })
}

fn wxid_source_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)^(wxid_[^\s]+)_[0-9a-f]{4}$").expect("valid wxid source regex")
    })
}

fn read_avatar_row(
    statement: &mut rusqlite::Statement<'_>,
    username: &str,
) -> Result<Option<AvatarImage>> {
    let mut rows = statement.query([username])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let data = row_value(row, 0).bytes().unwrap_or_default();
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(AvatarImage {
        username: username.to_string(),
        media_type: detect_image_media_type(&data).to_string(),
        md5: row_text(row, 1).trim().to_string(),
        update_time: row_i64(row, 2),
        size: data.len(),
        data,
    }))
}

pub(crate) fn detect_image_media_type(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "image/gif"
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && data[8..12].starts_with(b"WEBP") {
        "image/webp"
    } else if data.starts_with(b"BM") {
        "image/bmp"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avatar_db(statement: &str, rows: &[(&str, &str, &[u8], i64)]) -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        let connection =
            rusqlite::Connection::open(temp.path().join("head_image.db")).expect("open head_image");
        connection.execute_batch(statement).expect("create table");
        for (username, md5, data, update_time) in rows {
            connection
                .execute(
                    "INSERT INTO head_image VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![username, md5, data, update_time],
                )
                .expect("insert avatar");
        }
        temp
    }

    #[test]
    fn reads_latest_avatar_blob() {
        let temp = avatar_db(
            "CREATE TABLE head_image (username TEXT, md5 TEXT, image_buffer BLOB, update_time INTEGER)",
            &[
                ("wxid_a", "old", &b"old"[..], 1),
                ("wxid_a", "new", &b"\x89PNG\r\n\x1a\nrest"[..], 2),
            ],
        );
        let store = AccountStore::open(temp.path()).expect("open store");
        let avatar = store.load_avatar("wxid_a").expect("load").expect("avatar");
        assert_eq!(avatar.media_type, "image/png");
        assert_eq!(avatar.md5, "new");
        assert_eq!(avatar.update_time, 2);
        assert_eq!(avatar.size, 12);
        assert!(avatar.data.starts_with(b"\x89PNG"));
        assert!(store.load_avatar("missing").expect("load").is_none());
    }

    #[test]
    fn reads_avatar_with_legacy_image_column() {
        let temp = tempfile::tempdir().expect("tempdir");
        let connection =
            rusqlite::Connection::open(temp.path().join("head_image.db")).expect("open head_image");
        connection
            .execute_batch("CREATE TABLE head_image (username TEXT, image BLOB)")
            .expect("create table");
        connection
            .execute(
                "INSERT INTO head_image VALUES (?1, ?2)",
                rusqlite::params!["wxid_b", &[0xFFu8, 0xD8, 0xFF, 0x00][..]],
            )
            .expect("insert avatar");
        drop(connection);

        let store = AccountStore::open(temp.path()).expect("open store");
        let avatar = store.load_avatar("wxid_b").expect("load").expect("avatar");
        assert_eq!(avatar.media_type, "image/jpeg");
        assert_eq!(avatar.md5, "");
        assert_eq!(avatar.update_time, 0);
    }

    #[test]
    fn loads_avatars_in_batch() {
        let temp = avatar_db(
            "CREATE TABLE head_image (username TEXT, md5 TEXT, image_buffer BLOB, update_time INTEGER)",
            &[
                ("wxid_a", "a", &b"aaa"[..], 1),
                ("wxid_b", "b", &b"bbb"[..], 1),
            ],
        );
        let store = AccountStore::open(temp.path()).expect("open store");
        let avatars = store
            .load_avatars(&[String::from("wxid_a"), String::from("missing")])
            .expect("load avatars");
        assert_eq!(avatars.len(), 1);
        assert!(avatars.contains_key("wxid_a"));
    }

    #[test]
    fn detects_common_image_types() {
        assert_eq!(detect_image_media_type(&[0xFF, 0xD8, 0xFF]), "image/jpeg");
        assert_eq!(detect_image_media_type(b"\x89PNG\r\n\x1a\nxx"), "image/png");
        assert_eq!(detect_image_media_type(b"GIF89a"), "image/gif");
        assert_eq!(detect_image_media_type(b"RIFF1234WEBP"), "image/webp");
        assert_eq!(detect_image_media_type(b"BMxx"), "image/bmp");
        assert_eq!(
            detect_image_media_type(b"not an image"),
            "application/octet-stream"
        );
    }

    #[test]
    fn canonicalizes_backup_and_source_suffixes() {
        assert_eq!(
            canonical_account_name("wxid_abc_1234.backup-20260101-120000"),
            "wxid_abc_1234"
        );
        assert_eq!(canonical_account_name("wxid_abc1234_00ff"), "wxid_abc1234");
        assert_eq!(canonical_account_name("plain"), "plain");
    }

    #[test]
    fn identifies_message_shard_names() {
        assert!(is_message_database_name("message_0.db"));
        assert!(is_message_database_name("biz_message_3.db"));
        assert!(is_message_database_name("message.db"));
        assert!(!is_message_database_name("session.db"));
        assert!(!is_message_database_name("session_last_message.db"));
    }
}
