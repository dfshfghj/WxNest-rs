use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::json;

use wechat_data::{
    build_search_index, discover_accounts, refresh_search_index, search_index, AccountStore,
    MessageCursor, MessageQuery, RealtimeAccountStore, SearchQuery,
};

#[derive(Debug, Parser)]
#[command(name = "wechat-data", version, about = "WeChat chat data inspector")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "List account directories under a root.")]
    Accounts { root: PathBuf },

    #[command(about = "Show account metadata.")]
    Info { account_dir: PathBuf },

    #[command(about = "List sessions.")]
    Sessions {
        account_dir: PathBuf,
        #[arg(long)]
        include_hidden: bool,
        #[arg(long)]
        include_official: bool,
    },

    #[command(about = "List contacts.")]
    Contacts { account_dir: PathBuf },

    #[command(about = "Extract a local avatar blob from head_image.db.")]
    Avatar {
        account_dir: PathBuf,
        username: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },

    #[command(about = "Fetch a Tencent CDN media file and decrypt it.")]
    FetchMedia {
        url: String,
        #[arg(long)]
        token: Option<String>,

        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        video: bool,
        #[arg(long)]
        out: PathBuf,
    },

    #[command(
        about = "Load one page of messages for a conversation, oldest first within the page."
    )]
    Messages {
        account_dir: PathBuf,
        username: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        before_time: Option<i64>,
        #[arg(long)]
        before_sort: Option<i64>,
        #[arg(long)]
        before_local: Option<i64>,
        #[arg(long)]
        before_shard: Option<String>,
    },

    #[command(about = "Count messages for one conversation.")]
    Count {
        account_dir: PathBuf,
        username: String,
    },

    #[command(about = "Build the FTS5 chat search index (writes chat_search_index.db).")]
    SearchBuild { account_dir: PathBuf },

    #[command(about = "Refresh indexed conversations that changed since the last build.")]
    SearchRefresh { account_dir: PathBuf },

    #[command(about = "Query the FTS5 chat search index.")]
    Search {
        account_dir: PathBuf,
        q: String,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        include_hidden: bool,
        #[arg(long)]
        include_official: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        start_ts: Option<i64>,
        #[arg(long)]
        end_ts: Option<i64>,
    },

    #[command(about = "Run a read-only query through the self-hosted broker.")]
    RealtimeQuery {
        #[arg(long)]
        broker: PathBuf,

        #[arg(long)]
        root: PathBuf,

        #[arg(long)]
        db: PathBuf,

        #[arg(long)]
        key_hex: Option<String>,

        #[arg(long)]
        key_store: Option<PathBuf>,
        #[arg(long, default_value = "SELECT name, type FROM sqlite_master LIMIT 20")]
        sql: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,

        #[arg(long)]
        raw_key: bool,
    },

    #[command(about = "List sessions through the broker.")]
    RealtimeSessions {
        #[arg(long)]
        broker: PathBuf,
        #[arg(long)]
        root: PathBuf,

        #[arg(long)]
        key_hex: Option<String>,

        #[arg(long)]
        key_store: Option<PathBuf>,
        #[arg(long)]
        include_hidden: bool,
        #[arg(long)]
        include_official: bool,
        #[arg(long)]
        raw_key: bool,
    },

    #[command(about = "Load messages through the broker.")]
    RealtimeMessages {
        #[arg(long)]
        broker: PathBuf,
        #[arg(long)]
        root: PathBuf,

        #[arg(long)]
        key_hex: Option<String>,

        #[arg(long)]
        key_store: Option<PathBuf>,
        username: String,
        #[arg(long, default_value_t = 40)]
        limit: usize,
        #[arg(long)]
        raw_key: bool,
    },

    #[command(about = "Recover image AES/XOR keys for an account.")]
    RecoverImageKey {
        #[arg(long)]
        account_dir: PathBuf,
        #[arg(long)]
        pid: Option<u32>,
    },

    #[command(about = "Recover the database key for a db_storage directory.")]
    RecoverKey {
        #[arg(long)]
        root: PathBuf,

        #[arg(long)]
        wechat_dir: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Accounts { root } => {
            let accounts = discover_accounts(&root)?;
            println!("{}", serde_json::to_string_pretty(&accounts)?);
        }
        Command::Info { account_dir } => {
            let store = AccountStore::open(&account_dir)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "account": store.account(),
                    "account_dir": store.account_dir(),
                    "self_username": store.self_username(),
                    "self_candidates": store.self_candidates(),
                    "message_shards": store.account_info().message_shards,
                }))?
            );
        }
        Command::Sessions {
            account_dir,
            include_hidden,
            include_official,
        } => {
            let store = AccountStore::open(&account_dir)?;
            let sessions = store.list_sessions(include_hidden, include_official)?;
            println!("{}", serde_json::to_string_pretty(&sessions)?);
        }
        Command::Contacts { account_dir } => {
            let store = AccountStore::open(&account_dir)?;
            let contacts = store.list_contacts()?;
            println!("{}", serde_json::to_string_pretty(&contacts)?);
        }
        Command::Avatar {
            account_dir,
            username,
            out,
        } => {
            let store = AccountStore::open(&account_dir)?;
            match store.load_avatar(&username)? {
                Some(avatar) => {
                    if let Some(path) = &out {
                        std::fs::write(path, &avatar.data)?;
                    }
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "username": avatar.username,
                            "media_type": avatar.media_type,
                            "md5": avatar.md5,
                            "update_time": avatar.update_time,
                            "size": avatar.size,
                            "output": out.as_ref().map(|path| path.display().to_string()),
                        }))?
                    );
                }
                None => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "status": "not_found",
                            "username": username,
                        }))?
                    );
                }
            }
        }
        Command::FetchMedia {
            url,
            token,
            key,
            video,
            out,
        } => {
            let media = wechat_data::fetch_tencent_media(
                &url,
                token.as_deref(),
                key.as_deref(),
                video,
                80 * 1024 * 1024,
            )?;
            std::fs::write(&out, &media.data)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "media_type": media.media_type,
                    "source": media.source,
                    "size": media.data.len(),
                    "output": out.display().to_string(),
                }))?
            );
        }
        Command::Messages {
            account_dir,
            username,
            limit,
            before_time,
            before_sort,
            before_local,
            before_shard,
        } => {
            let store = AccountStore::open(&account_dir)?;
            let before = match (before_time, before_sort, before_local, before_shard) {
                (Some(create_time), Some(sort_seq), Some(local_id), Some(shard)) => {
                    Some(MessageCursor {
                        create_time,
                        sort_seq,
                        local_id,
                        shard,
                    })
                }
                _ => None,
            };
            let page = store.load_messages(&username, &MessageQuery { limit, before })?;
            println!("{}", serde_json::to_string_pretty(&page)?);
        }
        Command::Count {
            account_dir,
            username,
        } => {
            let store = AccountStore::open(&account_dir)?;
            println!("{}", store.count_messages(&username)?);
        }
        Command::SearchBuild { account_dir } => {
            let stats = build_search_index(&account_dir)?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Command::SearchRefresh { account_dir } => {
            let stats = refresh_search_index(&account_dir)?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Command::Search {
            account_dir,
            q,
            username,
            include_hidden,
            include_official,
            limit,
            offset,
            start_ts,
            end_ts,
        } => {
            let page = search_index(
                &account_dir,
                &SearchQuery {
                    q,
                    username,
                    include_hidden,
                    include_official,
                    start_ts,
                    end_ts,
                    limit,
                    offset,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&page)?);
        }
        Command::RealtimeQuery {
            broker,
            root,
            db,
            key_hex,
            key_store,
            sql,
            limit,
            raw_key,
        } => {
            let (key, raw, _account) = resolve_key(key_hex, key_store, &root, raw_key)?;
            let database = wechat_data::RealtimeDatabase::open(&broker, &root, &db, &key, raw)?;
            let page = database.query(&sql, limit)?;
            database.close();
            println!(
                "{}",
                serde_json::to_string_pretty(&query_page_json(
                    page.columns,
                    page.rows,
                    page.truncated
                ))?
            );
        }
        Command::RealtimeSessions {
            broker,
            root,
            key_hex,
            key_store,
            include_hidden,
            include_official,
            raw_key,
        } => {
            let (key, raw, account) = resolve_key(key_hex, key_store, &root, raw_key)?;
            let store = RealtimeAccountStore::open_with_account(
                &broker,
                &root,
                &key,
                raw,
                account.as_deref(),
            )?;
            let sessions = store.list_sessions(include_hidden, include_official)?;
            println!("{}", serde_json::to_string_pretty(&sessions)?);
        }
        Command::RealtimeMessages {
            broker,
            root,
            key_hex,
            key_store,
            username,
            limit,
            raw_key,
        } => {
            let (key, raw, account) = resolve_key(key_hex, key_store, &root, raw_key)?;
            let store = RealtimeAccountStore::open_with_account(
                &broker,
                &root,
                &key,
                raw,
                account.as_deref(),
            )?;
            let page = store.load_messages(
                &username,
                &MessageQuery {
                    limit,
                    before: None,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&page)?);
        }
        Command::RecoverImageKey { account_dir, pid } => {
            let found = wechat_data::recover_image_key_for_account(&account_dir, pid)?;
            eprintln!("提示: aes_key 会输出到 stdout，请勿粘贴到公开渠道");
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "aes_key": found.aes_key,
                    "xor_key": found.xor_key,
                    "encoding": found.encoding,
                    "pid": found.pid,
                }))?
            );
        }
        Command::RecoverKey { root, wechat_dir } => {
            let found = wechat_data::recover_key_for_db_storage(&root, wechat_dir.as_deref())?;
            eprintln!("提示: key_hex 会输出到 stdout，请勿粘贴到公开渠道");
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "key_hex": found.key_hex,
                    "raw_key": found.raw_key,
                    "source": found.source,
                    "internal_key_hex": found.internal_key_hex,
                }))?
            );
        }
    }
    Ok(())
}

fn resolve_key(
    key_hex: Option<String>,
    key_store: Option<PathBuf>,
    root: &std::path::Path,
    raw_flag: bool,
) -> Result<(String, bool, Option<String>), Box<dyn std::error::Error>> {
    let store_path =
        key_store.or_else(|| std::env::var("WECHAT_KEY_STORE").ok().map(PathBuf::from));
    if let Some(key) = key_hex
        .or_else(|| std::env::var("WCE_DB_KEY").ok())
        .filter(|value| !value.trim().is_empty())
    {
        let account = store_path.as_deref().and_then(|store| {
            wechat_data::find_key_for_db_storage_with_account(store, root)
                .ok()
                .and_then(|(_, _, account)| account)
        });
        return Ok((key.trim().to_string(), raw_flag, account));
    }
    let store = match store_path {
        Some(path) => path,
        None => {
            return Err("需要 --key-hex / WCE_DB_KEY，或 --key-store / WECHAT_KEY_STORE".into())
        }
    };
    Ok(wechat_data::find_key_for_db_storage_with_account(
        &store, root,
    )?)
}

fn query_page_json(
    columns: Vec<String>,
    rows: Vec<std::collections::BTreeMap<String, wechat_data::sqlite::SqlValue>>,
    truncated: bool,
) -> serde_json::Value {
    use base64::Engine as _;
    use wechat_data::sqlite::SqlValue;

    let rows: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            let mut object = serde_json::Map::new();
            for column in &columns {
                let value = match row.get(column).cloned().unwrap_or(SqlValue::Null) {
                    SqlValue::Null => serde_json::Value::Null,
                    SqlValue::Integer(value) => json!(value),
                    SqlValue::Real(value) => json!(value),
                    SqlValue::Text(value) => json!(value),
                    SqlValue::Blob(bytes) => json!({
                        "$b64": base64::engine::general_purpose::STANDARD.encode(bytes)
                    }),
                };
                object.insert(column.clone(), value);
            }
            serde_json::Value::Object(object)
        })
        .collect();

    json!({
        "columns": columns,
        "rows": rows,
        "truncated": truncated,
    })
}
