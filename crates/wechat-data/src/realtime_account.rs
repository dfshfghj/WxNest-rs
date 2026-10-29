use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use wechat_broker::{BrokerClient, BrokerOptions, Cell as RealtimeCell};

use crate::decode::decode_message_content;
use crate::error::{DataError, Result};
use crate::contacts::{contact_matches_keyword, normalize_contact, ContactRow};
use crate::message::{MessageQuery, resolve_table_name};
use crate::message_body::{
    message_wxid_references, parse_message_body_with_resolver, strip_group_sender_prefix,
};
use crate::general::{
    parse_finder_lives, parse_mini_programs, parse_payments, FinderLive, MiniProgram, PaymentItem,
};
use crate::model::{
    AccountInfo, AvatarImage, Contact, ContactItem, Message, MessageAfterPage, MessageCursor,
    MessageKind, MessagePage, Session,
};
use crate::sns::{parse_sns_post, SnsPost};
use crate::search::{SearchHit, SearchPage};
use crate::realtime::{RealtimeDatabase, RealtimeHost};
use crate::session::{display_name, is_group_username, is_official_username, should_keep_session};
use crate::sqlite::{quote_ident, SqlValue};
use crate::time::format_session_time;

struct RealtimeShard {
    name: String,
    db: RealtimeDatabase,
    tables: Vec<String>,
    self_rowid: Option<i64>,
    column_cache: std::sync::Mutex<HashMap<String, HashSet<String>>>,
}

pub struct RealtimeAccountStore {
    _host: RealtimeHost,
    native_actions: bool,
    account: String,
    account_dir: PathBuf,
    self_username: String,
    self_candidates: Vec<String>,
    session: RealtimeDatabase,
    contact: Option<RealtimeDatabase>,
    head_image: Option<RealtimeDatabase>,
    sns: Option<RealtimeDatabase>,
    general: Option<RealtimeDatabase>,
    shards: Vec<RealtimeShard>,
    _snapshot: Option<tempfile::TempDir>,
    group_name_cache: std::sync::Mutex<HashMap<String, (std::time::Instant, Vec<u8>)>>,
}

impl RealtimeAccountStore {
    pub fn open(
        broker_exe: &Path,
        db_storage: &Path,
        key_hex: &str,
        raw_key: bool,
    ) -> Result<Self> {
        Self::open_with_account(broker_exe, db_storage, key_hex, raw_key, None)
    }

    pub fn open_with_account(
        broker_exe: &Path,
        db_storage: &Path,
        key_hex: &str,
        raw_key: bool,
        account_hint: Option<&str>,
    ) -> Result<Self> {
        Self::open_with_options(
            broker_exe,
            db_storage,
            key_hex,
            raw_key,
            account_hint,
            &BrokerOptions::default(),
        )
    }

    pub fn open_with_options(
        broker_exe: &Path,
        db_storage: &Path,
        key_hex: &str,
        raw_key: bool,
        account_hint: Option<&str>,
        options: &BrokerOptions,
    ) -> Result<Self> {
        let root = std::fs::canonicalize(db_storage)?;
        if !root.is_dir() {
            return Err(DataError::InvalidAccount(format!(
                "不是目录: {}",
                root.display()
            )));
        }

        let account = account_hint
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                root.parent()
                    .and_then(|path| path.file_name())
                    .map(|value| value.to_string_lossy().to_string())
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| {
                root.file_name()
                    .map(|value| value.to_string_lossy().to_string())
            })
            .unwrap_or_default();
        let self_candidates = account_candidates(&account);
        let self_username = self_candidates
            .first()
            .cloned()
            .unwrap_or_else(|| account.clone());

        let force_snapshot = std::env::var("WECHAT_REALTIME_SNAPSHOT").ok().as_deref() == Some("1");
        let snapshot = tempfile::Builder::new()
            .prefix("wce-realtime-")
            .tempdir()
            .ok();
        let mut broker_roots = vec![root.clone()];
        if let Some(directory) = &snapshot {
            broker_roots.push(directory.path().to_path_buf());
        }
        let native_actions = options.native_bridge.is_some();
        let host = RealtimeHost::spawn_with_options(broker_exe, &broker_roots, options)?;
        let snapshot_root = snapshot.as_ref().map(|directory| directory.path());
        let timing = std::env::var_os("WCE_TIMING").is_some();
        let started = std::time::Instant::now();

        let session_path = find_db(&root, "session", "session.db")
            .ok_or_else(|| DataError::NotFound(format!("{}/session/session.db", root.display())))?;
        let shard_paths = discover_shards(&root);
        let host_ref = &host;
        let root_ref = &root;

        let (session_result, contact, head_image, sns, general, shards) =
            std::thread::scope(|scope| {
                let session = scope.spawn(|| {
                    open_db(
                        &host,
                        &session_path,
                        key_hex,
                        raw_key,
                        snapshot_root,
                        &root,
                        force_snapshot,
                    )
                });
                let contact = scope.spawn(|| {
                    find_db(root_ref, "contact", "contact.db").and_then(|path| {
                        open_db(
                            host_ref,
                            &path,
                            key_hex,
                            raw_key,
                            snapshot_root,
                            root_ref,
                            force_snapshot,
                        )
                        .ok()
                    })
                });
                let head_image = scope.spawn(|| {
                    find_db(root_ref, "head_image", "head_image.db").and_then(|path| {
                        open_db(
                            host_ref,
                            &path,
                            key_hex,
                            raw_key,
                            snapshot_root,
                            root_ref,
                            force_snapshot,
                        )
                        .ok()
                    })
                });
                let sns = scope.spawn(|| {
                    find_db(root_ref, "sns", "sns.db").and_then(|path| {
                        open_db(
                            host_ref,
                            &path,
                            key_hex,
                            raw_key,
                            snapshot_root,
                            root_ref,
                            force_snapshot,
                        )
                        .ok()
                    })
                });
                let general = scope.spawn(|| {
                    find_db(root_ref, "general", "general.db").and_then(|path| {
                        open_db(
                            host_ref,
                            &path,
                            key_hex,
                            raw_key,
                            snapshot_root,
                            root_ref,
                            force_snapshot,
                        )
                        .ok()
                    })
                });
                let shard_handles: Vec<_> = shard_paths
                    .iter()
                    .map(|path| {
                        let path = path.clone();
                        let self_candidates = self_candidates.clone();
                        scope.spawn(move || {
                            let db = open_db(
                                host_ref,
                                &path,
                                key_hex,
                                raw_key,
                                snapshot_root,
                                root_ref,
                                force_snapshot,
                            )
                            .ok()?;
                            let tables = table_names(&db);
                            let self_rowid = resolve_self_rowid(&db, &tables, &self_candidates);
                            Some(RealtimeShard {
                                name: path
                                    .file_name()
                                    .map(|value| value.to_string_lossy().to_string())
                                    .unwrap_or_default(),
                                db,
                                tables,
                                self_rowid,
                                column_cache: std::sync::Mutex::new(HashMap::new()),
                            })
                        })
                    })
                    .collect();
                let session_result = session.join().unwrap();
                let contact = contact.join().unwrap();
                let head_image = head_image.join().unwrap();
                let sns = sns.join().unwrap();
                let general = general.join().unwrap();
                let shards: Vec<RealtimeShard> = shard_handles
                    .into_iter()
                    .filter_map(|handle| handle.join().unwrap())
                    .collect();
                (session_result, contact, head_image, sns, general, shards)
            });
        let session = session_result?;
        if timing {
            eprintln!(
                "realtime open: shards={} total={:?}",
                shards.len(),
                started.elapsed()
            );
        }

        Ok(Self {
            _host: host,
            native_actions,
            account,
            account_dir: root,
            self_username,
            self_candidates,
            session,
            contact,
            head_image,
            sns,
            general,
            shards,
            _snapshot: snapshot,
            group_name_cache: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn native_actions(&self) -> bool {
        self.native_actions
    }

    pub fn action_client(&self) -> ActionClient {
        let account_directory = self
            .account_dir
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| self.account.clone());
        ActionClient {
            client: self._host.client(),
            account: self.account.clone(),
            self_wxid: self.self_username.clone(),
            account_directory,
            account_dir: self.account_dir.clone(),
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn account_dir(&self) -> &Path {
        &self.account_dir
    }

    pub fn account_root(&self) -> Option<&Path> {
        self.account_dir.parent()
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

    pub fn group_members(&self, chatroom: &str) -> Result<Vec<(String, String)>> {
        let Some(buffer) = self.group_ext_buffer(chatroom)? else {
            return Ok(Vec::new());
        };
        Ok(crate::group::parse_group_members(&buffer))
    }

    pub fn group_nicknames(
        &self,
        chatroom: &str,
        senders: &[String],
    ) -> Result<HashMap<String, String>> {
        let Some(buffer) = self.group_ext_buffer(chatroom)? else {
            return Ok(HashMap::new());
        };
        Ok(crate::group::parse_group_nickname_map(&buffer, senders))
    }

    pub fn contact_db_path(&self) -> Option<PathBuf> {
        let mut roots = vec![self.account_dir.clone()];
        if let Some(root) = self.account_root() {
            roots.push(root.to_path_buf());
        }
        for root in roots {
            if let Some(path) = find_db(&root, "contact", "contact.db") {
                return Some(path);
            }
        }
        None
    }

    fn group_ext_buffer(&self, chatroom: &str) -> Result<Option<Vec<u8>>> {
        let Some(contact) = &self.contact else {
            return Ok(None);
        };
        if !table_names(contact)
            .iter()
            .any(|table| table.eq_ignore_ascii_case("chat_room"))
        {
            return Ok(None);
        }
        let page = contact.query_with_params(
            "SELECT ext_buffer FROM chat_room WHERE username = ?1 LIMIT 1",
            &[RealtimeCell::Text(chatroom.to_string())],
            1,
        )?;
        for row in page.rows_as_maps() {
            if let Some(SqlValue::Blob(bytes)) = row.get("ext_buffer") {
                return Ok(Some(bytes.clone()));
            }
        }
        Ok(None)
    }

    pub fn list_contact_directory(&self, keyword: &str, limit: usize) -> Result<Vec<ContactItem>> {
        let Some(db) = &self.contact else {
            return Ok(Vec::new());
        };
        let tables = table_names(db);
        if !tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("contact"))
        {
            return Ok(Vec::new());
        }
        let probe = db.query("SELECT * FROM \"contact\" LIMIT 0", 0)?;
        let columns: HashSet<String> = probe.columns.into_iter().collect();
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
             FROM contact LIMIT 20000",
            quote_ident("username"),
            column("remark"),
            column("nick_name"),
            column("alias"),
            column("description"),
            column("pin_yin_initial"),
        );
        let page = db.query(&sql, 20000)?;
        let limit = limit.clamp(1, 5000);
        let mut output = Vec::new();
        for row in page.rows_as_maps() {
            let entry = normalize_contact(
                ContactRow {
                    username: cell_text(&row, "username"),
                    remark: cell_text(&row, "remark"),
                    nick_name: cell_text(&row, "nick_name"),
                    alias: cell_text(&row, "alias"),
                    local_type: cell_i64(&row, "local_type"),
                    flag: cell_i64(&row, "flag"),
                    description: cell_text(&row, "description"),
                    pinyin: cell_text(&row, "pinyin"),
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
        let Some(db) = &self.general else {
            return Ok(Vec::new());
        };
        let tables = table_names(db);
        if !tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("wcfinderlivestatus"))
        {
            return Ok(Vec::new());
        }
        let statuses = db
            .query(
                "SELECT finder_live_id, finder_username, finder_export_id, live_status, \
                 replay_status, charge_flag FROM wcfinderlivestatus \
                 ORDER BY live_status DESC, finder_live_id DESC LIMIT 500",
                500,
            )?
            .rows_as_maps();
        let users = if tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("wcfinderuserpage"))
        {
            db.query(
                "SELECT username, extra_buffer FROM wcfinderuserpage LIMIT 20000",
                20000,
            )?
            .rows_as_maps()
        } else {
            Vec::new()
        };
        Ok(parse_finder_lives(&statuses, &users))
    }

    pub fn list_mini_programs(&self) -> Result<Vec<MiniProgram>> {
        let Some(db) = &self.general else {
            return Ok(Vec::new());
        };
        let tables = table_names(db);
        if !tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("wacontact"))
        {
            return Ok(Vec::new());
        }
        let join = if tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("WeAppBizAttrSyncBufferTableV02"))
        {
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
        let rows = db.query(&sql, 2000)?.rows_as_maps();
        Ok(parse_mini_programs(&rows))
    }

    pub fn list_payments(&self) -> Result<Vec<PaymentItem>> {
        let Some(db) = &self.general else {
            return Ok(Vec::new());
        };
        let tables = table_names(db);
        let transfers = if tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("transferTable"))
        {
            db.query(
                "SELECT transfer_id, session_name, pay_sub_type, pay_payer, begin_transfer_time, \
                 last_modified_time, invalid_time FROM transferTable \
                 ORDER BY begin_transfer_time DESC LIMIT 1000",
                1000,
            )?
            .rows_as_maps()
        } else {
            Vec::new()
        };
        let red_packets = if tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("redEnvelopeTable"))
        {
            db.query(
                "SELECT message_server_id, session_name, sender_user_name, hb_status, \
                 receive_status FROM redEnvelopeTable LIMIT 1000",
                1000,
            )?
            .rows_as_maps()
        } else {
            Vec::new()
        };
        Ok(parse_payments(&transfers, &red_packets))
    }

    pub fn search_messages(&self, query: &str, limit: usize) -> Result<SearchPage> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(SearchPage {
                hits: Vec::new(),
                has_more: false,
                total_estimate: 0,
            });
        }
        let limit = limit.clamp(1, 200);
        let sessions = self.list_sessions(false, false).unwrap_or_default();
        let mut table_owner: HashMap<(String, String), String> = HashMap::new();
        for shard in &self.shards {
            for session in &sessions {
                if let Some(table) = resolve_table_name(&shard.tables, &session.username) {
                    table_owner.insert(
                        (shard.name.clone(), table.to_ascii_lowercase()),
                        session.username.clone(),
                    );
                }
            }
        }
        let mut hits = Vec::new();
        for shard in &self.shards {
            for table in &shard.tables {
                if !table.to_ascii_lowercase().starts_with("msg_") {
                    continue;
                }
                let username = table_owner
                    .get(&(shard.name.clone(), table.to_ascii_lowercase()))
                    .cloned()
                    .unwrap_or_default();
                let name2id = shard
                    .tables
                    .iter()
                    .find(|name| name.eq_ignore_ascii_case("Name2Id"));
                let join = name2id
                    .map(|name| {
                        format!(
                            "LEFT JOIN {} n ON n.rowid = m.real_sender_id",
                            quote_ident(name)
                        )
                    })
                    .unwrap_or_default();
                let sender = if name2id.is_some() {
                    "n.user_name"
                } else {
                    "''"
                };
                let sql = format!(
                    "SELECT m.local_id, m.server_id, m.local_type, m.create_time, m.sort_seq, \
                     m.message_content, m.compress_content, {sender} AS sender_username \
                     FROM {} m {join} \
                     WHERE instr(m.message_content, ?1) > 0 \
                        OR instr(lower(m.message_content), lower(?1)) > 0 \
                     ORDER BY m.create_time DESC, m.sort_seq DESC, m.local_id DESC LIMIT ?2",
                    quote_ident(table)
                );
                let Ok(page) = shard.db.query_with_params(
                    &sql,
                    &[
                        RealtimeCell::Text(query.to_string()),
                        RealtimeCell::Integer(limit.min(60) as i64),
                    ],
                    limit.min(60),
                ) else {
                    continue;
                };
                for row in page.rows_as_maps() {
                    let message_value = cell_value(&row, "message_content");
                    let compress_value = cell_value(&row, "compress_content");
                    let compress_ref = (!compress_value.is_null()).then_some(&compress_value);
                    let content = decode_message_content(compress_ref, &message_value);
                    if !content.to_lowercase().contains(&query.to_lowercase()) {
                        continue;
                    }
                    let local_type = cell_i64(&row, "local_type");
                    let kind = MessageKind::from_local_type(local_type);
                    hits.push(SearchHit {
                        username: username.clone(),
                        db_stem: shard.name.clone(),
                        table_name: table.clone(),
                        local_id: cell_i64(&row, "local_id"),
                        server_id: cell_i64(&row, "server_id"),
                        local_type,
                        sender_username: cell_text(&row, "sender_username"),
                        render_type: format!("{kind:?}").to_ascii_lowercase(),
                        create_time: cell_i64(&row, "create_time"),
                        sort_seq: cell_i64(&row, "sort_seq"),
                        payload: serde_json::json!({
                            "content": strip_sender_prefix(&content),
                        }),
                    });
                }
            }
        }
        hits.sort_by(|left, right| {
            right
                .create_time
                .cmp(&left.create_time)
                .then_with(|| right.sort_seq.cmp(&left.sort_seq))
        });
        let total = hits.len();
        hits.truncate(limit);
        Ok(SearchPage {
            hits,
            has_more: total > limit,
            total_estimate: total as i64,
        })
    }

    pub fn list_sns_posts(&self, before_tid: Option<i64>, limit: usize) -> Result<Vec<SnsPost>> {
        let Some(db) = &self.sns else {
            return Ok(Vec::new());
        };
        let tables = table_names(db);
        let Some(table) = tables
            .iter()
            .find(|table| table.eq_ignore_ascii_case("SnsTimeLine"))
        else {
            return Ok(Vec::new());
        };
        let probe = db.query(&format!("SELECT * FROM {} LIMIT 0", quote_ident(table)), 0)?;
        let columns: HashSet<String> = probe.columns.into_iter().collect();
        if !columns.contains("tid") || !columns.contains("content") {
            return Ok(Vec::new());
        }
        let username_column = if columns.contains("user_name") {
            quote_ident("user_name")
        } else {
            "''".to_string()
        };
        let limit = limit.clamp(1, 200);
        let mut sql = format!(
            "SELECT {} AS tid, {username_column} AS user_name, {} AS content \
             FROM {} WHERE content IS NOT NULL AND content != '' \
             AND content NOT LIKE '%<type>7</type>%'",
            quote_ident("tid"),
            quote_ident("content"),
            quote_ident(table),
        );
        let mut params: Vec<RealtimeCell> = Vec::new();
        if let Some(before) = before_tid {
            sql.push_str(" AND tid < ?1");
            params.push(RealtimeCell::Integer(before));
        }
        sql.push_str(&format!(" ORDER BY tid DESC LIMIT ?{}", params.len() + 1));
        params.push(RealtimeCell::Integer((limit + 1) as i64));
        let page = db.query_with_params(&sql, &params, limit + 1)?;
        let mut posts = Vec::new();
        for row in page.rows_as_maps() {
            let tid = cell_i64(&row, "tid");
            let content = cell_text(&row, "content");
            let Some(mut post) = parse_sns_post(tid, &content) else {
                continue;
            };
            if post.username.is_empty() {
                post.username = cell_text(&row, "user_name");
            }
            posts.push(post);
            if posts.len() >= limit {
                break;
            }
        }
        self.apply_sns_display_names(&mut posts);
        Ok(posts)
    }

    fn apply_sns_display_names(&self, posts: &mut [SnsPost]) {
        let usernames: Vec<String> = posts.iter().map(|post| post.username.clone()).collect();
        let Ok(contacts) = self.load_contacts_for(&usernames) else {
            return;
        };
        for post in posts {
            if let Some(contact) = contacts.get(&post.username) {
                if !contact.display_name.trim().is_empty() {
                    post.display_name = contact.display_name.clone();
                }
            }
        }
    }

    pub fn list_sessions(
        &self,
        include_hidden: bool,
        include_official: bool,
    ) -> Result<Vec<Session>> {
        let tables = table_names(&self.session);
        if !tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("SessionTable"))
        {
            return Ok(Vec::new());
        }
        let probe = self
            .session
            .query("SELECT * FROM \"SessionTable\" LIMIT 0", 0)?;
        let columns: HashSet<String> = probe.columns.into_iter().collect();
        if !columns.contains("username") {
            return Ok(Vec::new());
        }

        let select = |name: &str, fallback: &str| {
            if columns.contains(name) {
                quote_ident(name)
            } else {
                fallback.to_string()
            }
        };
        let order = if columns.contains("sort_timestamp") {
            format!("ORDER BY {} DESC", quote_ident("sort_timestamp"))
        } else if columns.contains("last_timestamp") {
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

        let page = self.session.query(&sql, 200_000)?;
        let mut raw = Vec::new();
        for row in page.rows_as_maps() {
            let username = cell_text(&row, "username").trim().to_string();
            if username.is_empty() {
                continue;
            }
            let is_hidden = cell_i64(&row, "is_hidden") == 1;
            if !include_hidden && is_hidden {
                continue;
            }
            if !should_keep_session(&username, include_official) {
                continue;
            }
            raw.push((
                username,
                is_hidden,
                cell_value(&row, "summary"),
                cell_value(&row, "draft"),
                cell_opt_i64(&row, "last_msg_type"),
                cell_opt_i64(&row, "last_msg_sub_type"),
                cell_opt_i64(&row, "sort_timestamp"),
                cell_opt_i64(&row, "last_timestamp"),
            ));
        }

        let usernames: Vec<String> = raw.iter().map(|entry| entry.0.clone()).collect();
        let contacts = self.load_contacts_for(&usernames).unwrap_or_default();
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
            let display = contacts
                .get(&username)
                .map(|contact| contact.display_name.clone())
                .unwrap_or_else(|| username.clone());
            let timestamp = sort_timestamp.or(last_timestamp);
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
                has_avatar: false,
            });
        }
        Ok(sessions)
    }

    pub fn load_contacts_for(&self, usernames: &[String]) -> Result<HashMap<String, Contact>> {
        let Some(contact) = &self.contact else {
            return Ok(HashMap::new());
        };
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
        let tables = table_names(contact);
        let mut result = HashMap::new();
        for table_name in ["contact", "stranger"] {
            let Some(actual) = tables
                .iter()
                .find(|table| table.eq_ignore_ascii_case(table_name))
            else {
                continue;
            };
            let probe =
                contact.query(&format!("SELECT * FROM {} LIMIT 0", quote_ident(actual)), 0)?;
            let columns: HashSet<String> = probe.columns.into_iter().collect();
            if !columns.contains("username") {
                continue;
            }
            let select = |name: &str, fallback: &str| {
                if columns.contains(name) {
                    quote_ident(name)
                } else {
                    fallback.to_string()
                }
            };
            for chunk in wanted.chunks(300) {
                let placeholders = (1..=chunk.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT {} AS username, {} AS remark, {} AS nick_name, {} AS alias \
                     FROM {} WHERE {} IN ({placeholders})",
                    quote_ident("username"),
                    select("remark", "NULL"),
                    select("nick_name", "NULL"),
                    select("alias", "NULL"),
                    quote_ident(actual),
                    quote_ident("username"),
                );
                let params: Vec<RealtimeCell> = chunk
                    .iter()
                    .map(|name| RealtimeCell::Text(name.clone()))
                    .collect();
                let page = contact.query_with_params(&sql, &params, chunk.len())?;
                for row in page.rows_as_maps() {
                    let username = cell_text(&row, "username").trim().to_string();
                    if username.is_empty() || result.contains_key(&username) {
                        continue;
                    }
                    let remark = cell_text(&row, "remark");
                    let nick_name = cell_text(&row, "nick_name");
                    let alias = cell_text(&row, "alias");
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
        let Some(contact) = &self.contact else {
            return Ok(result);
        };
        let tables = table_names(contact);
        for table_name in ["contact", "stranger"] {
            let Some(actual) = tables
                .iter()
                .find(|table| table.eq_ignore_ascii_case(table_name))
            else {
                continue;
            };
            let probe =
                contact.query(&format!("SELECT * FROM {} LIMIT 0", quote_ident(actual)), 0)?;
            let columns: HashSet<String> = probe.columns.into_iter().collect();
            if !columns.contains("username") {
                continue;
            }
            let select = |name: &str, fallback: &str| {
                if columns.contains(name) {
                    quote_ident(name)
                } else {
                    fallback.to_string()
                }
            };
            let sql = format!(
                "SELECT {} AS username, {} AS remark, {} AS nick_name, {} AS alias FROM {}",
                quote_ident("username"),
                select("remark", "NULL"),
                select("nick_name", "NULL"),
                select("alias", "NULL"),
                quote_ident(actual)
            );
            let page = contact.query(&sql, 200_000)?;
            for row in page.rows_as_maps() {
                let username = cell_text(&row, "username").trim().to_string();
                if username.is_empty() {
                    continue;
                }
                let remark = cell_text(&row, "remark");
                let nick_name = cell_text(&row, "nick_name");
                let alias = cell_text(&row, "alias");
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
        Ok(result)
    }

    pub fn load_messages(&self, username: &str, query: &MessageQuery) -> Result<MessagePage> {
        let username = username.trim();
        let limit = query.limit.clamp(1, 500);
        if username.is_empty() {
            return Ok(MessagePage {
                messages: Vec::new(),
                has_more: false,
                next_before: None,
            });
        }

        let timing = std::env::var_os("WCE_TIMING").is_some();
        let started = std::time::Instant::now();
        let tasks: Vec<(&RealtimeShard, String)> = self
            .shards
            .iter()
            .filter_map(|shard| {
                resolve_table_name(&shard.tables, username).map(|table| (shard, table))
            })
            .collect();
        let results: Vec<Result<Vec<Message>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = tasks
                .iter()
                .map(|(shard, table)| {
                    scope.spawn(move || {
                        let shard_started = std::time::Instant::now();
                        let result =
                            query_shard_realtime(shard, table, query.before.as_ref(), limit + 1);
                        if timing {
                            eprintln!(
                                "  shard {} {}: {:?}",
                                shard.name,
                                table,
                                shard_started.elapsed()
                            );
                        }
                        result
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        let after_shards = started.elapsed();
        let mut all = Vec::new();
        let mut any_extra = false;
        for result in results {
            let mut rows = result?;
            if rows.len() > limit {
                any_extra = true;
            }
            all.append(&mut rows);
        }

        all.sort_by(|left, right| {
            right
                .create_time
                .cmp(&left.create_time)
                .then_with(|| right.sort_seq.cmp(&left.sort_seq))
                .then_with(|| right.local_id.cmp(&left.local_id))
                .then_with(|| right.shard.cmp(&left.shard))
        });
        let has_more = any_extra || all.len() > limit;
        all.truncate(limit);
        all.reverse();
        self.attach_sender_display_names(username, &mut all);
        if timing {
            eprintln!(
                "load_messages {username}: shards={after_shards:?} total={:?}",
                started.elapsed()
            );
        }

        let next_before = if has_more {
            all.first().map(|message| MessageCursor {
                create_time: message.create_time,
                sort_seq: message.sort_seq,
                local_id: message.local_id,
                shard: message.shard.clone(),
            })
        } else {
            None
        };

        Ok(MessagePage {
            messages: all,
            has_more,
            next_before,
        })
    }

    pub fn load_messages_after(
        &self,
        username: &str,
        after: Option<&MessageCursor>,
        limit: usize,
    ) -> Result<MessageAfterPage> {
        let username = username.trim();
        let limit = limit.clamp(1, 500);
        if username.is_empty() {
            return Ok(MessageAfterPage {
                messages: Vec::new(),
                has_more: false,
                next_after: after.cloned(),
            });
        }
        let tasks: Vec<(&RealtimeShard, String)> = self
            .shards
            .iter()
            .filter_map(|shard| {
                resolve_table_name(&shard.tables, username).map(|table| (shard, table))
            })
            .collect();
        let results: Vec<Result<Vec<Message>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = tasks
                .iter()
                .map(|(shard, table)| {
                    scope.spawn(move || query_shard_realtime_after(shard, table, after, limit + 1))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        let mut all = Vec::new();
        let mut any_extra = false;
        for result in results {
            let mut rows = result?;
            if rows.len() > limit {
                any_extra = true;
            }
            all.append(&mut rows);
        }
        all.sort_by(|left, right| {
            left.create_time
                .cmp(&right.create_time)
                .then_with(|| left.sort_seq.cmp(&right.sort_seq))
                .then_with(|| left.local_id.cmp(&right.local_id))
                .then_with(|| left.shard.cmp(&right.shard))
        });
        let has_more = any_extra || all.len() > limit;
        all.truncate(limit);
        self.attach_sender_display_names(username, &mut all);
        let next_after = all
            .last()
            .map(|message| MessageCursor {
                create_time: message.create_time,
                sort_seq: message.sort_seq,
                local_id: message.local_id,
                shard: message.shard.clone(),
            })
            .or_else(|| after.cloned());
        Ok(MessageAfterPage {
            messages: all,
            has_more,
            next_after,
        })
    }

    pub fn load_avatars(&self, usernames: &[String]) -> Result<HashMap<String, AvatarImage>> {
        let mut result = HashMap::new();
        let Some(db) = &self.head_image else {
            return Ok(result);
        };
        let targets: Vec<String> = usernames
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        if targets.is_empty() {
            return Ok(result);
        }
        let tables = table_names(db);
        if !tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("head_image"))
        {
            return Ok(result);
        }
        let probe = db.query("SELECT * FROM \"head_image\" LIMIT 0", 0)?;
        let columns: HashSet<String> = probe.columns.into_iter().collect();
        if !columns.contains("username") {
            return Ok(result);
        }
        let Some(buffer) = ["image_buffer", "image", "img_buffer", "buffer"]
            .into_iter()
            .find(|name| columns.contains(*name))
        else {
            return Ok(result);
        };
        let update = if columns.contains("update_time") {
            quote_ident("update_time")
        } else {
            "0".to_string()
        };
        let sort = if columns.contains("update_time") {
            quote_ident("update_time")
        } else {
            "0 + 0".to_string()
        };
        let md5 = if columns.contains("md5") {
            quote_ident("md5")
        } else {
            "''".to_string()
        };
        let placeholders = (1..=targets.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {} AS username, {} AS buffer, {} AS md5, {} AS update_time              FROM \"head_image\" WHERE {} IN ({}) ORDER BY {} DESC",
            quote_ident("username"),
            quote_ident(buffer),
            md5,
            update,
            quote_ident("username"),
            placeholders,
            sort,
        );
        let params: Vec<RealtimeCell> = targets.into_iter().map(RealtimeCell::Text).collect();
        let page = db.query_with_params(&sql, &params, 200_000)?;
        for row in page.rows_as_maps() {
            let username = cell_text(&row, "username").trim().to_string();
            if username.is_empty() || result.contains_key(&username) {
                continue;
            }
            let data = row
                .get("buffer")
                .and_then(SqlValue::bytes)
                .unwrap_or_default();
            if data.is_empty() {
                continue;
            }
            result.insert(
                username.clone(),
                AvatarImage {
                    username,
                    media_type: crate::account::detect_image_media_type(&data).to_string(),
                    md5: cell_text(&row, "md5"),
                    update_time: cell_i64(&row, "update_time"),
                    size: data.len(),
                    data,
                },
            );
        }
        Ok(result)
    }

    pub fn load_avatar(&self, username: &str) -> Result<Option<AvatarImage>> {
        let username = username.trim();
        if username.is_empty() {
            return Ok(None);
        }
        let Some(db) = &self.head_image else {
            return Ok(None);
        };
        let tables = table_names(db);
        if !tables
            .iter()
            .any(|table| table.eq_ignore_ascii_case("head_image"))
        {
            return Ok(None);
        }
        let probe = db.query("SELECT * FROM \"head_image\" LIMIT 0", 0)?;
        let columns: HashSet<String> = probe.columns.into_iter().collect();
        if !columns.contains("username") {
            return Ok(None);
        }
        let Some(buffer) = ["image_buffer", "image", "img_buffer", "buffer"]
            .into_iter()
            .find(|name| columns.contains(*name))
        else {
            return Ok(None);
        };
        let update = if columns.contains("update_time") {
            quote_ident("update_time")
        } else {
            "0".to_string()
        };
        let sort = if columns.contains("update_time") {
            quote_ident("update_time")
        } else {
            "0 + 0".to_string()
        };
        let md5 = if columns.contains("md5") {
            quote_ident("md5")
        } else {
            "''".to_string()
        };
        let sql = format!(
            "SELECT {} AS buffer, {} AS md5, {} AS update_time FROM \"head_image\"              WHERE {} = ?1 ORDER BY {} DESC LIMIT 1",
            quote_ident(buffer),
            md5,
            update,
            quote_ident("username"),
            sort,
        );
        let page = db.query_with_params(&sql, &[RealtimeCell::Text(username.to_string())], 1)?;
        let Some(row) = page.rows_as_maps().into_iter().next() else {
            return Ok(None);
        };
        let data = row
            .get("buffer")
            .and_then(SqlValue::bytes)
            .unwrap_or_default();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(AvatarImage {
            username: username.to_string(),
            media_type: crate::account::detect_image_media_type(&data).to_string(),
            md5: cell_text(&row, "md5"),
            update_time: cell_i64(&row, "update_time"),
            size: data.len(),
            data,
        }))
    }

    fn attach_sender_display_names(&self, username: &str, messages: &mut [Message]) {
        let is_group = username.ends_with("@chatroom");
        let mut referenced: Vec<String> = messages
            .iter()
            .map(|message| message.sender_username.trim().to_string())
            .filter(|sender| !sender.is_empty())
            .collect();
        for message in messages.iter() {
            referenced.extend(message_wxid_references(&message.content));
        }
        referenced.sort();
        referenced.dedup();
        let timing = std::env::var_os("WCE_TIMING").is_some();
        let names_started = std::time::Instant::now();
        let contacts = self.load_contacts_for(&referenced).unwrap_or_default();
        let after_contacts = names_started.elapsed();
        let group = if is_group {
            self.load_group_nickname_map_realtime(username, &referenced)
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        if timing {
            eprintln!(
                "  names {username}: contacts={after_contacts:?} group={:?}",
                names_started.elapsed() - after_contacts
            );
        }
        for message in messages {
            let sender = message.sender_username.trim().to_string();
            let group_name = group
                .get(&sender)
                .filter(|nickname| !nickname.trim().is_empty())
                .cloned();
            let sender_display = group_name
                .clone()
                .or_else(|| {
                    contacts
                        .get(&sender)
                        .map(|contact| contact.display_name.clone())
                })
                .unwrap_or_else(|| sender.clone());
            message.sender_display_name = sender_display.clone();
            if is_group && !sender.is_empty() {
                strip_group_sender_prefix(message, &sender_display, group_name.as_deref());
            }
            let resolve = |wxid: &str| -> String {
                group
                    .get(wxid)
                    .filter(|nickname| !nickname.trim().is_empty())
                    .cloned()
                    .or_else(|| {
                        contacts
                            .get(wxid)
                            .map(|contact| contact.display_name.clone())
                    })
                    .or_else(|| (wxid == self.self_username()).then(|| "我".to_string()))
                    .unwrap_or_default()
            };
            message.body =
                parse_message_body_with_resolver(message.local_type, &message.content, &resolve);
        }
    }

    fn load_group_nickname_map_realtime(
        &self,
        chatroom: &str,
        senders: &[String],
    ) -> Result<HashMap<String, String>> {
        let Some(contact) = &self.contact else {
            return Ok(HashMap::new());
        };
        if let Ok(cache) = self.group_name_cache.lock() {
            if let Some((loaded_at, blob)) = cache.get(chatroom) {
                if loaded_at.elapsed() < std::time::Duration::from_secs(300) {
                    return Ok(crate::group::parse_group_nickname_map(blob, senders));
                }
            }
        }
        let page = contact.query_with_params(
            "SELECT ext_buffer FROM \"chat_room\" WHERE username = ?1 LIMIT 1",
            &[RealtimeCell::Text(chatroom.to_string())],
            1,
        )?;
        let Some(row) = page.rows_as_maps().into_iter().next() else {
            return Ok(HashMap::new());
        };
        let Some(SqlValue::Blob(blob)) = row.get("ext_buffer") else {
            return Ok(HashMap::new());
        };
        let result = crate::group::parse_group_nickname_map(blob, senders);
        if let Ok(mut cache) = self.group_name_cache.lock() {
            cache.insert(
                chatroom.to_string(),
                (std::time::Instant::now(), blob.clone()),
            );
        }
        Ok(result)
    }
}

fn shard_columns(shard: &RealtimeShard, table: &str) -> Result<HashSet<String>> {
    if let Ok(cache) = shard.column_cache.lock() {
        if let Some(columns) = cache.get(table) {
            return Ok(columns.clone());
        }
    }
    let probe = shard
        .db
        .query(&format!("SELECT * FROM {} LIMIT 0", quote_ident(table)), 0)?;
    let columns: HashSet<String> = probe.columns.into_iter().collect();
    if let Ok(mut cache) = shard.column_cache.lock() {
        cache.insert(table.to_string(), columns.clone());
    }
    Ok(columns)
}

fn query_shard_realtime(
    shard: &RealtimeShard,
    table: &str,
    before: Option<&MessageCursor>,
    limit: usize,
) -> Result<Vec<Message>> {
    let columns = shard_columns(shard, table)?;
    if !columns.contains("local_id") {
        return Ok(Vec::new());
    }

    let local_id = "m.local_id";
    let server_id = if columns.contains("server_id") {
        "m.server_id"
    } else {
        "0"
    };
    let create_time = if columns.contains("create_time") {
        "m.create_time"
    } else {
        "0 + 0"
    };
    let sort_seq = if columns.contains("sort_seq") {
        "m.sort_seq"
    } else {
        "0 + 0"
    };
    let local_type = if columns.contains("local_type") {
        "m.local_type"
    } else {
        "0"
    };
    let message_content = if columns.contains("message_content") {
        "m.message_content"
    } else {
        "NULL"
    };
    let compress_content = if columns.contains("compress_content") {
        "m.compress_content"
    } else {
        "NULL"
    };
    let real_sender_id = if columns.contains("real_sender_id") {
        "m.real_sender_id"
    } else {
        "NULL"
    };
    let packed_info = if columns.contains("packed_info_data") {
        "m.packed_info_data"
    } else {
        "NULL"
    };

    let name2id = name2id_table(&shard.tables);
    let (join, sender_username) = match &name2id {
        Some(name) if columns.contains("real_sender_id") => (
            format!(
                "LEFT JOIN {} n ON n.rowid = m.real_sender_id",
                quote_ident(name)
            ),
            "n.user_name",
        ),
        _ => (String::new(), "NULL"),
    };

    let has_sort_seq = columns.contains("sort_seq");
    let (where_clause, order_clause, limit_placeholder) = if has_sort_seq {
        (
            if before.is_some() {
                "WHERE (m.sort_seq, m.local_id) < (?1, ?2)"
            } else {
                ""
            }
            .to_string(),
            "ORDER BY m.sort_seq DESC, m.local_id DESC".to_string(),
            if before.is_some() { "?3" } else { "?1" },
        )
    } else {
        (
            if before.is_some() {
                "WHERE (m.create_time, m.sort_seq, m.local_id) < (?1, ?2, ?3)"
            } else {
                ""
            }
            .to_string(),
            format!("ORDER BY {create_time} DESC, {sort_seq} DESC, {local_id} DESC"),
            if before.is_some() { "?4" } else { "?1" },
        )
    };
    let sql = format!(
        "SELECT {local_id} AS local_id, {server_id} AS server_id, \
                {create_time} AS create_time, {sort_seq} AS sort_seq, \
                {local_type} AS local_type, {message_content} AS message_content, \
                {compress_content} AS compress_content, {real_sender_id} AS real_sender_id, \
                {packed_info} AS packed_info, {sender_username} AS sender_username \
         FROM {} m {join} {where_clause} {order_clause} LIMIT {limit_placeholder}",
        quote_ident(table)
    );

    let mut parameters: Vec<RealtimeCell> = Vec::new();
    if let Some(cursor) = before {
        if has_sort_seq {
            parameters.push(RealtimeCell::Integer(cursor.sort_seq));
            parameters.push(RealtimeCell::Integer(cursor.local_id));
        } else {
            parameters.push(RealtimeCell::Integer(cursor.create_time));
            parameters.push(RealtimeCell::Integer(cursor.sort_seq));
            parameters.push(RealtimeCell::Integer(cursor.local_id));
        }
    }
    parameters.push(RealtimeCell::Integer(limit as i64));

    let page = shard.db.query_with_params(&sql, &parameters, limit)?;
    let mut output = Vec::new();
    let mut seen: HashSet<(i64, i64, i64)> = HashSet::new();
    for row in page.rows_as_maps() {
        let local_id = cell_i64(&row, "local_id");
        let server_id = cell_i64(&row, "server_id");
        let create_time = cell_i64(&row, "create_time");
        let sort_seq = cell_i64(&row, "sort_seq");
        let local_type = cell_i64(&row, "local_type");
        let message_value = cell_value(&row, "message_content");
        let compress_value = cell_value(&row, "compress_content");
        let compress_ref = if compress_value.is_null() {
            None
        } else {
            Some(&compress_value)
        };
        let real_sender_id = cell_opt_i64(&row, "real_sender_id");
        let packed_value = cell_value(&row, "packed_info");
        let sender_username = cell_text(&row, "sender_username");

        if !seen.insert((create_time, sort_seq, local_id)) {
            continue;
        }

        let content = decode_message_content(compress_ref, &message_value);
        let media_md5 = crate::media_index::extract_md5_from_packed_info(&packed_value);
        output.push(Message {
            local_id,
            server_id,
            create_time,
            sort_seq,
            local_type,
            kind: MessageKind::from_local_type(local_type),
            sender_username,
            sender_display_name: String::new(),
            is_sent: shard.self_rowid.is_some() && shard.self_rowid == real_sender_id,
            body: None,
            content,
            media_md5: (!media_md5.is_empty()).then_some(media_md5),
            shard: shard.name.clone(),
            table: table.to_string(),
        });
    }
    Ok(output)
}

fn query_shard_realtime_after(
    shard: &RealtimeShard,
    table: &str,
    after: Option<&MessageCursor>,
    limit: usize,
) -> Result<Vec<Message>> {
    let columns = shard_columns(shard, table)?;
    if !columns.contains("local_id") {
        return Ok(Vec::new());
    }

    let local_id = "m.local_id";
    let server_id = if columns.contains("server_id") {
        "m.server_id"
    } else {
        "0"
    };
    let create_time = if columns.contains("create_time") {
        "m.create_time"
    } else {
        "0 + 0"
    };
    let sort_seq = if columns.contains("sort_seq") {
        "m.sort_seq"
    } else {
        "0 + 0"
    };
    let local_type = if columns.contains("local_type") {
        "m.local_type"
    } else {
        "0"
    };
    let message_content = if columns.contains("message_content") {
        "m.message_content"
    } else {
        "NULL"
    };
    let compress_content = if columns.contains("compress_content") {
        "m.compress_content"
    } else {
        "NULL"
    };
    let real_sender_id = if columns.contains("real_sender_id") {
        "m.real_sender_id"
    } else {
        "NULL"
    };
    let packed_info = if columns.contains("packed_info_data") {
        "m.packed_info_data"
    } else {
        "NULL"
    };
    let name2id = name2id_table(&shard.tables);
    let (join, sender_username) = match &name2id {
        Some(name) if columns.contains("real_sender_id") => (
            format!(
                "LEFT JOIN {} n ON n.rowid = m.real_sender_id",
                quote_ident(name)
            ),
            "n.user_name",
        ),
        _ => (String::new(), "NULL"),
    };
    let has_sort_seq = columns.contains("sort_seq");
    let (where_clause, order_clause, limit_placeholder) = if has_sort_seq {
        (
            if after.is_some() {
                "WHERE (m.sort_seq, m.local_id) > (?1, ?2)"
            } else {
                ""
            }
            .to_string(),
            "ORDER BY m.sort_seq ASC, m.local_id ASC".to_string(),
            if after.is_some() { "?3" } else { "?1" },
        )
    } else {
        (
            if after.is_some() {
                "WHERE (m.create_time, m.sort_seq, m.local_id) > (?1, ?2, ?3)"
            } else {
                ""
            }
            .to_string(),
            format!("ORDER BY {create_time} ASC, {sort_seq} ASC, {local_id} ASC"),
            if after.is_some() { "?4" } else { "?1" },
        )
    };
    let sql = format!(
        "SELECT {local_id} AS local_id, {server_id} AS server_id, \
                {create_time} AS create_time, {sort_seq} AS sort_seq, \
                {local_type} AS local_type, {message_content} AS message_content, \
                {compress_content} AS compress_content, {real_sender_id} AS real_sender_id, \
                {packed_info} AS packed_info, {sender_username} AS sender_username \
         FROM {} m {join} {where_clause} {order_clause} LIMIT {limit_placeholder}",
        quote_ident(table)
    );
    let mut parameters: Vec<RealtimeCell> = Vec::new();
    if let Some(cursor) = after {
        if has_sort_seq {
            parameters.push(RealtimeCell::Integer(cursor.sort_seq));
            parameters.push(RealtimeCell::Integer(cursor.local_id));
        } else {
            parameters.push(RealtimeCell::Integer(cursor.create_time));
            parameters.push(RealtimeCell::Integer(cursor.sort_seq));
            parameters.push(RealtimeCell::Integer(cursor.local_id));
        }
    }
    parameters.push(RealtimeCell::Integer(limit as i64));

    let page = shard.db.query_with_params(&sql, &parameters, limit)?;
    let mut output = Vec::new();
    let mut seen: HashSet<(i64, i64, i64)> = HashSet::new();
    for row in page.rows_as_maps() {
        let local_id_value = cell_i64(&row, "local_id");
        let server_id_value = cell_i64(&row, "server_id");
        let create_time_value = cell_i64(&row, "create_time");
        let sort_seq_value = cell_i64(&row, "sort_seq");
        let local_type_value = cell_i64(&row, "local_type");
        let message_value = cell_value(&row, "message_content");
        let compress_value = cell_value(&row, "compress_content");
        let compress_ref = if compress_value.is_null() {
            None
        } else {
            Some(&compress_value)
        };
        let real_sender_id_value = cell_opt_i64(&row, "real_sender_id");
        let packed_value = cell_value(&row, "packed_info");
        let sender_username_value = cell_text(&row, "sender_username");

        if !seen.insert((create_time_value, sort_seq_value, local_id_value)) {
            continue;
        }

        let content = decode_message_content(compress_ref, &message_value);
        let media_md5 = crate::media_index::extract_md5_from_packed_info(&packed_value);
        output.push(Message {
            local_id: local_id_value,
            server_id: server_id_value,
            create_time: create_time_value,
            sort_seq: sort_seq_value,
            local_type: local_type_value,
            kind: MessageKind::from_local_type(local_type_value),
            sender_username: sender_username_value,
            sender_display_name: String::new(),
            is_sent: shard.self_rowid.is_some() && shard.self_rowid == real_sender_id_value,
            body: None,
            content,
            media_md5: (!media_md5.is_empty()).then_some(media_md5),
            shard: shard.name.clone(),
            table: table.to_string(),
        });
    }
    Ok(output)
}

fn open_db(
    host: &RealtimeHost,
    path: &Path,
    key_hex: &str,
    raw_key: bool,
    snapshot_root: Option<&Path>,
    db_root: &Path,
    force: bool,
) -> Result<RealtimeDatabase> {
    if force {
        if let Some(snapshot_root) = snapshot_root {
            let copy = snapshot_database(path, db_root, snapshot_root)?;
            return host.open(&copy, key_hex, raw_key);
        }
    }
    match host.open(path, key_hex, raw_key) {
        Ok(database) => Ok(database),
        Err(error) => {
            if let Some(snapshot_root) = snapshot_root {
                if let Ok(copy) = snapshot_database(path, db_root, snapshot_root) {
                    return host.open(&copy, key_hex, raw_key);
                }
            }
            Err(error)
        }
    }
}

fn snapshot_database(source: &Path, root: &Path, destination_root: &Path) -> Result<PathBuf> {
    let relative = source.strip_prefix(root).unwrap_or(source);
    let destination = destination_root.join(relative);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, &destination)?;
    for suffix in ["-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{}", source.display(), suffix));
        if from.is_file() {
            let to = PathBuf::from(format!("{}{}", destination.display(), suffix));
            let _ = std::fs::copy(from, to);
        }
    }
    Ok(destination)
}

fn strip_sender_prefix(content: &str) -> String {
    if let Some((prefix, rest)) = content.split_once('\n') {
        if prefix.len() <= 64
            && prefix.ends_with(':')
            && !prefix.contains('<')
            && !prefix.contains('>')
        {
            return rest.to_string();
        }
    }
    content.to_string()
}

fn table_names(db: &RealtimeDatabase) -> Vec<String> {
    db.query("SELECT name FROM sqlite_master WHERE type='table'", 4000)
        .map(|page| {
            page.rows_as_maps()
                .iter()
                .map(|row| cell_text(row, "name"))
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn name2id_table(tables: &[String]) -> Option<String> {
    tables
        .iter()
        .find(|table| table.eq_ignore_ascii_case("Name2Id"))
        .cloned()
}

fn resolve_self_rowid(
    db: &RealtimeDatabase,
    tables: &[String],
    candidates: &[String],
) -> Option<i64> {
    let table = name2id_table(tables)?;
    for candidate in candidates {
        let sql = format!(
            "SELECT rowid AS rid FROM {} WHERE user_name = ?1 LIMIT 1",
            quote_ident(&table)
        );
        let Ok(page) = db.query_with_params(&sql, &[RealtimeCell::Text(candidate.clone())], 1)
        else {
            continue;
        };
        if let Some(row) = page.rows_as_maps().first() {
            if let Some(value) = row.get("rid").and_then(SqlValue::as_i64) {
                return Some(value);
            }
        }
    }
    None
}

fn account_candidates(account: &str) -> Vec<String> {
    let mut output = vec![account.to_string()];
    if let Some((prefix, suffix)) = account.rsplit_once('_') {
        if !prefix.is_empty()
            && suffix.len() <= 8
            && suffix.chars().all(|character| character.is_ascii_alphanumeric())
        {
            output.push(prefix.to_string());
        }
    }
    output
}

fn find_db(root: &Path, group: &str, name: &str) -> Option<PathBuf> {
    let nested = root.join(group).join(name);
    if nested.is_file() {
        return Some(nested);
    }
    let flat = root.join(name);
    flat.is_file().then_some(flat)
}

fn discover_shards(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for directory in [root.join("message"), root.to_path_buf()] {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name.ends_with(".db") && (name.starts_with("message_") || name.starts_with("msg")) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn cell_text(row: &BTreeMap<String, SqlValue>, name: &str) -> String {
    row.get(name)
        .map(|value| value.text_lossy())
        .unwrap_or_default()
}

fn cell_i64(row: &BTreeMap<String, SqlValue>, name: &str) -> i64 {
    row.get(name).and_then(|value| value.as_i64()).unwrap_or(0)
}

fn cell_opt_i64(row: &BTreeMap<String, SqlValue>, name: &str) -> Option<i64> {
    row.get(name).and_then(|value| value.as_i64())
}

fn cell_value(row: &BTreeMap<String, SqlValue>, name: &str) -> SqlValue {
    row.get(name).cloned().unwrap_or(SqlValue::Null)
}

fn native_completed(value: &serde_json::Value, action: &str) -> Result<()> {
    let Some(log) = value
        .get("result")
        .and_then(|result| result.get("log"))
    else {
        return Ok(());
    };
    match log.get("completion_success").and_then(|flag| flag.as_bool()) {
        None | Some(true) => Ok(()),
        Some(false) => {
            let field = |name: &str| log.get(name).and_then(|item| item.as_u64()).unwrap_or(0);
            Err(DataError::Action(format!(
                "{action} did not complete (stage {}, status {}, native_status {})",
                field("stage"),
                field("status"),
                field("native_status")
            )))
        }
    }
}

#[derive(Clone)]
pub struct ActionClient {
    client: BrokerClient,
    account: String,
    self_wxid: String,
    account_directory: String,
    account_dir: PathBuf,
}

impl ActionClient {
    pub fn send_text(&self, recipient: &str, text: &str) -> Result<serde_json::Value> {
        let value = self.client.action(
            "message.send_text",
            &self.account,
            &self.account_directory,
            recipient,
            text,
            false,
        )?;
        native_completed(&value, "message.send_text")?;
        Ok(value)
    }

    pub fn send_image(&self, recipient: &str, path: &str) -> Result<serde_json::Value> {
        self.send_attachment("message.send_image", recipient, path)
    }

    pub fn send_file(&self, recipient: &str, path: &str) -> Result<serde_json::Value> {
        self.send_attachment("message.send_file", recipient, path)
    }

    pub fn send_emoticon(&self, recipient: &str, path: &str) -> Result<serde_json::Value> {
        self.send_attachment("message.send_emoticon", recipient, path)
    }

    pub fn send_voice(
        &self,
        recipient: &str,
        path: &str,
        duration_ms: u32,
    ) -> Result<serde_json::Value> {
        self.send_attachment(
            "message.send_voice",
            recipient,
            &format!("{path}\u{1f}{duration_ms}"),
        )
    }

    fn send_attachment(
        &self,
        action: &str,
        recipient: &str,
        payload: &str,
    ) -> Result<serde_json::Value> {
        let mut accounts = vec![self.self_wxid.clone()];
        if let Some((prefix, suffix)) = self.self_wxid.rsplit_once('_') {
            if !prefix.is_empty()
                && suffix.len() <= 8
                && suffix.chars().all(|character| character.is_ascii_alphanumeric())
            {
                accounts.push(prefix.to_string());
            }
        }
        let mut failure = None;
        for account in accounts {
            let value = self.client.action(
                action,
                &account,
                &self.account_directory,
                recipient,
                payload,
                false,
            )?;
            match native_completed(&value, action) {
                Ok(()) => return Ok(value),
                Err(error) => failure = Some(error),
            }
        }
        Err(failure.unwrap_or_else(|| DataError::Action(format!("{action} failed"))))
    }

    pub fn send_at_text(
        &self,
        recipient: &str,
        text: &str,
        members: &str,
    ) -> Result<serde_json::Value> {
        Ok(self.client.action(
            "message.send_at_text",
            &self.self_wxid,
            &self.account_directory,
            recipient,
            &format!("{members}\u{1f}{text}"),
            false,
        )?)
    }

    pub fn like_moments(&self, feed_id: u64, user: &str) -> Result<serde_json::Value> {
        Ok(self.client.action(
            "moments.like",
            &self.self_wxid,
            &self.account_directory,
            &feed_id.to_string(),
            &format!("0\u{1f}{user}"),
            false,
        )?)
    }

    pub fn comment_image_moments(
        &self,
        feed_id: u64,
        user: &str,
        image: &str,
    ) -> Result<serde_json::Value> {
        Ok(self.client.action(
            "moments.comment_image",
            &self.self_wxid,
            &self.account_directory,
            &feed_id.to_string(),
            &format!("{user}\u{1f}{image}"),
            false,
        )?)
    }

    pub fn publish_moments(
        &self,
        content: &str,
        images: &[String],
        visibility: u32,
    ) -> Result<serde_json::Value> {
        let recipient = self.account_dir.display().to_string();
        let payload = format!(
            "{visibility}\u{1f}{content}\u{1f}{}\u{1f}\u{1f}",
            images.join("\u{1e}")
        );
        Ok(self.client.action(
            "moments.publish",
            &self.self_wxid,
            &self.account_directory,
            &recipient,
            &payload,
            false,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find_key_for_db_storage_with_account;

    #[test]
    #[ignore]
    fn lists_group_members_from_store() {
        let root = std::env::var_os("WECHAT_REALTIME_ROOT")
            .map(PathBuf::from)
            .expect("root");
        let broker = std::env::var_os("WCE_SELF_BROKER")
            .map(PathBuf::from)
            .expect("broker");
        let key_store = std::env::var_os("WECHAT_KEY_STORE")
            .map(PathBuf::from)
            .expect("key store");
        let chatroom = std::env::var("WECHAT_TEST_CHATROOM")
            .unwrap_or_else(|_| "42979783184@chatroom".to_string());
        let (key, raw, _) = find_key_for_db_storage_with_account(&key_store, &root).expect("key");
        let store = RealtimeAccountStore::open_with_options(
            &broker,
            &root,
            &key,
            raw,
            None,
            &BrokerOptions::default(),
        )
        .expect("store");
        let members = store.group_members(&chatroom).expect("members");
        eprintln!(
            "{} members: {:?}",
            members.len(),
            members.iter().take(8).collect::<Vec<_>>()
        );
        assert!(!members.is_empty());
    }
}
