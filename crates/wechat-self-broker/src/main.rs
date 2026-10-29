use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use rusqlite::types::ValueRef;
use rusqlite::types::Value as SqlVal;
use rusqlite::{params_from_iter, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::Sha512;
use url::Url;

mod actions;
mod native_actions;

const KDF_ROUNDS: u32 = 256_000;
const PAGE_SIZE: usize = 4096;
const MAX_MEDIA_BYTES: usize = 32 * 1024 * 1024;
const MEDIA_HOST: &str = "wwfile.work.weixin.qq.com";

type ApiError = (StatusCode, String);

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    roots: Arc<Vec<PathBuf>>,
    dbs: Arc<Mutex<HashMap<u64, Arc<Mutex<Connection>>>>>,
    next_id: Arc<Mutex<u64>>,
    http: reqwest::Client,
    actions: actions::ActionMode,
    native: Option<Arc<native_actions::NativeProvider>>,
}

#[derive(Deserialize)]
struct OpenReq {
    path: String,
    key_hex: String,
    #[serde(default)]
    raw_key: bool,
    #[serde(default = "default_busy_timeout")]
    busy_timeout_ms: u64,
}

#[derive(Deserialize)]
struct QueryReq {
    db_id: u64,
    sql: String,
    #[serde(default = "default_max_rows")]
    max_rows: usize,
    #[serde(default)]
    params: Vec<Value>,
}

#[derive(Deserialize)]
struct CloseReq {
    db_id: u64,
}

#[derive(Deserialize)]
struct MediaReq {
    url: String,
    authkey: String,
}

#[derive(Serialize)]
struct MediaResp {
    content_type: String,
    bytes_b64: String,
}

fn default_busy_timeout() -> u64 {
    5000
}

fn default_max_rows() -> usize {
    512
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let got = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if got == format!("Bearer {}", state.token) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()))
    }
}

fn read_salt(path: &Path) -> Result<[u8; 16], ApiError> {
    let mut file = File::open(path)
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("打开数据库失败: {err}")))?;
    let mut salt = [0u8; 16];
    file.read_exact(&mut salt).map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            format!("数据库头部不足 16 字节: {err}"),
        )
    })?;
    Ok(salt)
}

fn derive_key(key: &[u8], salt: &[u8; 16], raw: bool) -> [u8; 32] {
    if raw {
        let mut out = [0u8; 32];
        out.copy_from_slice(&key[..32]);
        return out;
    }
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha512>(key, salt, KDF_ROUNDS, &mut out);
    out
}

fn ensure_under_roots(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, ApiError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("路径无效: {err}")))?;
    if !canonical.is_file() {
        return Err((StatusCode::BAD_REQUEST, "不是文件".to_string()));
    }
    if !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err((
            StatusCode::FORBIDDEN,
            "数据库不在允许的 db_storage 根目录内".to_string(),
        ));
    }
    Ok(canonical)
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}))
}

async fn open_db(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OpenReq>,
) -> Result<Json<Value>, ApiError> {
    check_auth(&state, &headers)?;

    let path = ensure_under_roots(Path::new(&req.path), &state.roots)?;
    let key = hex::decode(req.key_hex.trim()).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "key_hex 不是合法十六进制".to_string(),
        )
    })?;
    if key.len() != 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            "key 必须是 32 字节（64 hex）".to_string(),
        ));
    }

    let salt = read_salt(&path)?;
    let derived = derive_key(&key, &salt, req.raw_key);
    let raw_hex = format!("{}{}", hex::encode(derived), hex::encode(salt));

    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("打开数据库失败: {err}")))?;
    conn.busy_timeout(Duration::from_millis(
        req.busy_timeout_ms.clamp(100, 60_000),
    ))
    .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    conn.execute_batch(&format!(
        "PRAGMA cipher_compatibility = 4;\nPRAGMA key = \"x'{raw_hex}'\";"
    ))
    .map_err(|err| (StatusCode::BAD_REQUEST, format!("设置密钥失败: {err}")))?;

    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })
    .map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            format!("密钥或数据库不匹配: {err}"),
        )
    })?;

    conn.execute_batch("PRAGMA query_only = ON;")
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let mut next = state.next_id.lock().unwrap();
    *next += 1;
    let db_id = *next;
    drop(next);
    state
        .dbs
        .lock()
        .unwrap()
        .insert(db_id, Arc::new(Mutex::new(conn)));

    Ok(Json(json!({"db_id": db_id, "page_size": PAGE_SIZE})))
}

fn json_to_sql_value(value: &Value) -> Result<SqlVal, ApiError> {
    Ok(match value {
        Value::Null => SqlVal::Null,
        Value::Bool(flag) => SqlVal::Integer(i64::from(*flag)),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                SqlVal::Integer(integer)
            } else if let Some(real) = number.as_f64() {
                SqlVal::Real(real)
            } else {
                return Err((StatusCode::BAD_REQUEST, "不支持的参数数值".to_string()));
            }
        }
        Value::String(text) => SqlVal::Text(text.clone()),
        Value::Object(object) => match object.get("$b64") {
            Some(Value::String(encoded)) => SqlVal::Blob(
                B64.decode(encoded)
                    .map_err(|_| (StatusCode::BAD_REQUEST, "非法的 $b64 参数".to_string()))?,
            ),
            _ => return Err((StatusCode::BAD_REQUEST, "不支持的参数对象".to_string())),
        },
        _ => return Err((StatusCode::BAD_REQUEST, "不支持的参数类型".to_string())),
    })
}

async fn query_db(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QueryReq>,
) -> Result<Json<Value>, ApiError> {
    check_auth(&state, &headers)?;
    let max_rows = req.max_rows.clamp(1, 4096);

    let conn = {
        let dbs = state.dbs.lock().unwrap();
        dbs.get(&req.db_id).cloned()
    }
    .ok_or((StatusCode::NOT_FOUND, "db_id 不存在".to_string()))?;
    let conn = conn.lock().unwrap();

    let mut stmt = conn
        .prepare(&req.sql)
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("SQL 错误: {err}")))?;
    let columns: Vec<String> = {
        let names = stmt.column_names();
        names.iter().map(|name| (*name).to_string()).collect()
    };

    let bound: Vec<SqlVal> = req
        .params
        .iter()
        .map(json_to_sql_value)
        .collect::<Result<_, _>>()?;
    let mut rows = stmt
        .query(params_from_iter(bound))
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("查询失败: {err}")))?;
    let mut records: Vec<Value> = Vec::new();
    let mut truncated = false;
    while let Some(row) = rows
        .next()
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("读取行失败: {err}")))?
    {
        if records.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut obj = Map::new();
        for (index, name) in columns.iter().enumerate() {
            let value = match row
                .get_ref(index)
                .map_err(|err| (StatusCode::BAD_REQUEST, format!("读取列失败: {err}")))?
            {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(number) => json!(number),
                ValueRef::Real(number) => json!(number),
                ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
                ValueRef::Blob(bytes) => json!({"$b64": B64.encode(bytes)}),
            };
            obj.insert(name.clone(), value);
        }
        records.push(Value::Object(obj));
    }

    Ok(Json(json!({
        "columns": columns,
        "rows": records,
        "truncated": truncated,
    })))
}

async fn close_db(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CloseReq>,
) -> Result<Json<Value>, ApiError> {
    check_auth(&state, &headers)?;
    let removed = state.dbs.lock().unwrap().remove(&req.db_id).is_some();
    Ok(Json(json!({"closed": removed})))
}

fn validate_media_url(raw: &str) -> Result<Url, ApiError> {
    let url = Url::parse(raw).map_err(|_| (StatusCode::BAD_REQUEST, "URL 无效".to_string()))?;
    if url.scheme() != "https" {
        return Err((StatusCode::BAD_REQUEST, "只允许 https".to_string()));
    }
    if url.host_str() != Some(MEDIA_HOST) {
        return Err((StatusCode::FORBIDDEN, "host 不在白名单".to_string()));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err((StatusCode::BAD_REQUEST, "不允许 query/fragment".to_string()));
    }
    let path = url.path().trim_start_matches('/');
    if path.is_empty() || !path.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err((StatusCode::BAD_REQUEST, "路径必须是十六进制".to_string()));
    }
    Ok(url)
}

async fn fetch_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MediaReq>,
) -> Result<Json<MediaResp>, ApiError> {
    check_auth(&state, &headers)?;
    let url = validate_media_url(&req.url)?;
    let response = state
        .http
        .get(url)
        .header("authkey", &req.authkey)
        .header("user-agent", "wce_cdn_image/1.0")
        .send()
        .await
        .map_err(|err| (StatusCode::BAD_GATEWAY, format!("请求失败: {err}")))?;

    let status = response.status();
    if !status.is_success() {
        return Err((StatusCode::BAD_GATEWAY, format!("CDN 返回 {status}")));
    }
    if let Some(length) = response.content_length() {
        if length as usize > MAX_MEDIA_BYTES {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "媒体超过 32MB".to_string()));
        }
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| (StatusCode::BAD_GATEWAY, format!("读取失败: {err}")))?;
    if bytes.len() > MAX_MEDIA_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "媒体超过 32MB".to_string()));
    }

    Ok(Json(MediaResp {
        content_type,
        bytes_b64: B64.encode(bytes),
    }))
}

async fn list_wechat_actions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    check_auth(&state, &headers)?;
    Ok(Json(actions::catalog(state.actions)))
}

async fn run_wechat_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<actions::ActionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    check_auth(&state, &headers)?;
    if state.actions == actions::ActionMode::Disabled {
        return Ok((StatusCode::FORBIDDEN, Json(json!({ "status": "disabled" }))));
    }
    let plan = actions::plan(state.actions, &request)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    if !request.dry_run {
        if state.actions == actions::ActionMode::Native {
            return execute_native_action(state, request, plan).await;
        }
        return Ok((
            StatusCode::NOT_IMPLEMENTED,
            Json(actions::not_implemented(&plan)),
        ));
    }
    Ok((StatusCode::OK, Json(json!(plan))))
}

async fn execute_native_action(
    state: AppState,
    request: actions::ActionRequest,
    plan: actions::ActionPlan,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if request.action == "moments.refresh" {
        return Ok((
            StatusCode::OK,
            Json(json!({
                "status": "local_refresh",
                "action": "moments.refresh",
                "executed": true,
                "native": false,
                "note": "本地缓存重读；未触发微信网络刷新",
            })),
        ));
    }
    let provider = state.native.ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "native provider is not configured".to_string(),
    ))?;
    let account = request.account.clone();
    let recipient = request.recipient.clone();
    let payload = request.payload.clone();
    let result = match request.action.as_str() {
        "message.send_text" => {
            let value =
                tokio::task::spawn_blocking(move || provider.send_text(&recipient, &payload))
                    .await
                    .map_err(native_join_error)?
                    .map_err(native_action_error)?;
            if !value.completion_success {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    format!("native send failed with status {}", value.native_status),
                ));
            }
            json!(value)
        }
        "message.send_image" | "message.send_file" | "message.send_emoticon" => {
            let action = request.action.clone();
            let path = payload.clone();
            tokio::task::spawn_blocking(move || match action.as_str() {
                "message.send_image" => provider.send_image(&account, &recipient, &path),
                "message.send_file" => provider.send_file(&account, &recipient, &path),
                _ => provider.send_emoticon(&account, &recipient, &path),
            })
            .await
            .map_err(native_join_error)?
            .map_err(native_action_error)?
        }
        "message.send_voice" => {
            let (path, duration) = payload.split_once('').ok_or((
                StatusCode::BAD_REQUEST,
                "voice payload must be path\x1fduration".to_string(),
            ))?;
            let duration = duration.parse::<u32>().map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "voice duration is invalid".to_string(),
                )
            })?;
            let path = path.to_string();
            tokio::task::spawn_blocking(move || {
                provider.send_voice(&account, &recipient, &path, duration)
            })
            .await
            .map_err(native_join_error)?
            .map_err(native_action_error)?
        }
        "message.send_at_text" => {
            let (members, text) = payload.split_once('').ok_or((
                StatusCode::BAD_REQUEST,
                "at text payload must be members\x1ftext".to_string(),
            ))?;
            let members = members.to_string();
            let text = text.to_string();
            tokio::task::spawn_blocking(move || {
                provider.send_at_text(&account, &recipient, &text, &members)
            })
            .await
            .map_err(native_join_error)?
            .map_err(native_action_error)?
        }
        "moments.publish" => {
            let fields: Vec<&str> = payload.split('\u{1f}').collect();
            let privacy = fields
                .first()
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or((
                    StatusCode::BAD_REQUEST,
                    "moments.publish privacy is invalid".to_string(),
                ))?;
            if privacy > 2 {
                return Err((
                    StatusCode::NOT_IMPLEMENTED,
                    "native moments.publish supports public/private only".to_string(),
                ));
            }
            let content = fields.get(1).copied().unwrap_or("").to_string();
            let images: Vec<String> = fields
                .get(2)
                .map(|value| {
                    value
                        .split('\u{1e}')
                        .filter(|item| !item.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            tokio::task::spawn_blocking(move || {
                provider.publish_moments(&account, &content, &images, privacy)
            })
            .await
            .map_err(native_join_error)?
            .map_err(native_action_error)?
        }
        "moments.like" => {
            let feed: u64 = recipient.parse().map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "moments.like recipient must be a feed id".to_string(),
                )
            })?;
            tokio::task::spawn_blocking(move || provider.moments_action(&account, feed, 1, ""))
                .await
                .map_err(native_join_error)?
                .map_err(native_action_error)?
        }
        "moments.comment_image" => {
            let feed: u64 = recipient.parse().map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "moments.comment_image recipient must be a feed id".to_string(),
                )
            })?;
            let image = payload
                .split_once('\u{1f}')
                .map(|(_, path)| path.to_string())
                .filter(|path| !path.is_empty())
                .ok_or((
                    StatusCode::BAD_REQUEST,
                    "moments.comment_image payload must be user\\x1fpath".to_string(),
                ))?;
            tokio::task::spawn_blocking(move || provider.moments_action(&account, feed, 3, &image))
                .await
                .map_err(native_join_error)?
                .map_err(native_action_error)?
        }
        _ => {
            return Ok((
                StatusCode::NOT_IMPLEMENTED,
                Json(actions::not_implemented(&plan)),
            ));
        }
    };
    Ok((
        StatusCode::OK,
        Json(json!({
            "status": "sent",
            "action": plan.action,
            "executed": true,
            "result": result,
        })),
    ))
}

fn native_join_error(err: tokio::task::JoinError) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("native task failed: {err}"),
    )
}

fn native_action_error(message: String) -> ApiError {
    let code = if message.contains("timed out") {
        StatusCode::GATEWAY_TIMEOUT
    } else {
        StatusCode::BAD_GATEWAY
    };
    (code, message)
}

fn parse_args() -> (
    Vec<PathBuf>,
    u16,
    String,
    actions::ActionMode,
    Option<Arc<native_actions::NativeProvider>>,
) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut port: u16 = 0;
    let mut token = std::env::var("WCE_BROKER_TOKEN").unwrap_or_default();
    let mut action_mode = actions::ActionMode::DryRun;
    let mut action_dll: Option<PathBuf> = None;
    let mut action_work_dir = std::env::temp_dir().join("wce-native-actions");
    let mut action_timeout = Duration::from_millis(15_000);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                if let Some(value) = args.next() {
                    roots.push(PathBuf::from(value));
                }
            }
            "--port" => {
                if let Some(value) = args.next() {
                    port = value.parse().unwrap_or(0);
                }
            }
            "--token" => {
                if let Some(value) = args.next() {
                    token = value;
                }
            }
            "--actions" => {
                if let Some(value) = args.next() {
                    match actions::ActionMode::from_arg(&value) {
                        Some(mode) => action_mode = mode,
                        None => eprintln!("未知动作模式: {value}"),
                    }
                }
            }
            "--actions-dll" => {
                if let Some(value) = args.next() {
                    action_dll = Some(PathBuf::from(value));
                }
            }
            "--actions-work-dir" => {
                if let Some(value) = args.next() {
                    action_work_dir = PathBuf::from(value);
                }
            }
            "--actions-timeout-ms" => {
                if let Some(value) = args.next() {
                    action_timeout = Duration::from_millis(value.parse().unwrap_or(15_000));
                }
            }
            other => eprintln!("忽略未知参数: {other}"),
        }
    }

    if token.is_empty() {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        token = hex::encode(bytes);
    }

    let roots: Vec<PathBuf> = roots
        .into_iter()
        .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
        .collect();
    let target_db = roots
        .iter()
        .find_map(|root| {
            let nested = root.join("session").join("session.db");
            if nested.is_file() {
                return Some(nested);
            }
            let flat = root.join("session.db");
            flat.is_file().then_some(flat)
        })
        .unwrap_or_default();
    let native = if action_mode == actions::ActionMode::Native {
        match action_dll {
            Some(dll) if dll.is_file() => Some(Arc::new(native_actions::NativeProvider::new(
                dll,
                action_work_dir,
                action_timeout,
                target_db,
            ))),
            Some(dll) => {
                eprintln!("native bridge not found: {}", dll.display());
                std::process::exit(2);
            }
            None => {
                eprintln!("--actions native 需要 --actions-dll <text_bridge.dll>");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    (roots, port, token, action_mode, native)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (roots, port, token, action_mode, native) = parse_args();
    if roots.is_empty() {
        eprintln!(
            "用法: wce-self-broker --root <db_storage> [--root ...] [--port 8765] [--token <t>] [--actions disabled|dry-run|native] [--actions-dll <text_bridge.dll>]"
        );
        std::process::exit(2);
    }

    #[cfg(not(feature = "sqlcipher"))]
    eprintln!("警告：未启用 SQLCipher（plain-sqlite 构建），无法打开加密数据库，仅用于编译检查。");

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let state = AppState {
        token: Arc::new(token.clone()),
        roots: Arc::new(roots),
        dbs: Arc::new(Mutex::new(HashMap::new())),
        next_id: Arc::new(Mutex::new(0)),
        http,
        actions: action_mode,
        native,
    };

    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/wechat/actions", get(list_wechat_actions))
        .route("/v1/wechat/action", post(run_wechat_action))
        .route("/v1/db/open", post(open_db))
        .route("/v1/db/query", post(query_db))
        .route("/v1/db/close", post(close_db))
        .route("/v1/media/fetch", post(fetch_media))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let address = listener.local_addr()?;
    println!("wce-self-broker listening on http://{address}");
    println!("token: {token}");
    axum::serve(listener, app).await?;
    Ok(())
}
