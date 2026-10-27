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

fn parse_args() -> (Vec<PathBuf>, u16, String) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut port: u16 = 0;
    let mut token = std::env::var("WCE_BROKER_TOKEN").unwrap_or_default();

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
            other => eprintln!("忽略未知参数: {other}"),
        }
    }

    if token.is_empty() {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        token = hex::encode(bytes);
    }

    let roots = roots
        .into_iter()
        .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
        .collect();
    (roots, port, token)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (roots, port, token) = parse_args();
    if roots.is_empty() {
        eprintln!(
            "用法: wce-self-broker --root <db_storage> [--root ...] [--port 8765] [--token <t>]"
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
    };

    let app = Router::new()
        .route("/v1/health", get(health))
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
