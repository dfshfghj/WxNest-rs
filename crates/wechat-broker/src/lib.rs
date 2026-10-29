use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("broker 启动失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("broker 未就绪: {0}")]
    NotReady(String),
    #[error("HTTP 错误: {0}")]
    Http(String),
    #[error("协议错误: {0}")]
    Protocol(String),
    #[error("服务端错误 {status}: {message}")]
    Server { status: u16, message: String },
}

pub type Result<T> = std::result::Result<T, BrokerError>;

#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct QueryPage {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    pub truncated: bool,
}

impl QueryPage {
    pub fn rows_as_maps(&self) -> Vec<BTreeMap<String, Cell>> {
        self.rows
            .iter()
            .map(|row| {
                self.columns
                    .iter()
                    .cloned()
                    .zip(row.iter().cloned())
                    .collect()
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct OpenResponse {
    db_id: u64,
}

#[derive(Deserialize)]
struct QueryResponse {
    columns: Vec<String>,
    rows: Vec<serde_json::Map<String, Value>>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BrokerOptions {
    pub native_bridge: Option<PathBuf>,
    pub native_work_dir: Option<PathBuf>,
    pub native_timeout_ms: Option<u64>,
}

pub struct BrokerHost {
    child: Child,
    base_url: String,
    token: String,
}

impl BrokerHost {
    pub fn spawn(broker_exe: &Path, roots: &[PathBuf]) -> Result<Self> {
        Self::spawn_with_options(broker_exe, roots, &BrokerOptions::default())
    }

    pub fn spawn_with_options(
        broker_exe: &Path,
        roots: &[PathBuf],
        options: &BrokerOptions,
    ) -> Result<Self> {
        if !broker_exe.is_file() {
            return Err(BrokerError::NotReady(format!(
                "broker 可执行文件不存在: {}",
                broker_exe.display()
            )));
        }

        let mut token_bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut token_bytes);
        let token = hex::encode(token_bytes);

        let mut command = Command::new(broker_exe);
        if broker_exe
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("wechat-agent"))
        {
            command.arg("db");
        }
        command.arg("--port").arg("0").arg("--token").arg(&token);
        for root in roots {
            command.arg("--root").arg(root);
        }
        if let Some(bridge) = options.native_bridge.as_ref() {
            command.arg("--actions").arg("native");
            command.arg("--actions-dll").arg(bridge);
            if let Some(work_dir) = options.native_work_dir.as_ref() {
                command.arg("--actions-work-dir").arg(work_dir);
            }
            if let Some(timeout) = options.native_timeout_ms {
                command.arg("--actions-timeout-ms").arg(timeout.to_string());
            }
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }

        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BrokerError::NotReady("无法读取 broker stdout".to_string()))?;
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for _ in reader.lines().map_while(|line| line.ok()) {}
            });
        }

        let (tx, rx) = mpsc::channel::<u16>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(|line| line.ok()) {
                if let Some(port) = parse_ready_line(&line) {
                    let _ = tx.send(port);
                    break;
                }
            }
        });

        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(port) => {
                let host = Self {
                    child,
                    base_url: format!("http://127.0.0.1:{port}"),
                    token,
                };
                for _ in 0..50 {
                    if host.client().health().is_ok() {
                        return Ok(host);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(BrokerError::NotReady(
                    "broker 已启动但健康检查失败（5s）".to_string(),
                ))
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(BrokerError::NotReady(
                    "等待 broker 端口超时（15s）".to_string(),
                ))
            }
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn client(&self) -> BrokerClient {
        BrokerClient::new(&self.base_url, &self.token)
    }
}

impl Drop for BrokerHost {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone)]
pub struct BrokerClient {
    base_url: String,
    token: String,
    agent: ureq::Agent,
}

impl BrokerClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(120))
            .build();
        Self {
            base_url: base_url.into(),
            token: token.into(),
            agent,
        }
    }

    fn authed(&self, request: ureq::Request) -> ureq::Request {
        request.set("Authorization", &format!("Bearer {}", self.token))
    }

    pub fn health(&self) -> Result<()> {
        let response = self
            .agent
            .get(&format!("{}/v1/health", self.base_url))
            .call()
            .map_err(map_ureq_error)?;
        let _: Value = response
            .into_json()
            .map_err(|err| BrokerError::Protocol(err.to_string()))?;
        Ok(())
    }

    pub fn open(&self, db_path: &Path, key_hex: &str, raw_key: bool) -> Result<u64> {
        let body = json!({
            "path": db_path.to_string_lossy(),
            "key_hex": key_hex,
            "raw_key": raw_key,
        });
        let response = self
            .authed(self.agent.post(&format!("{}/v1/db/open", self.base_url)))
            .send_json(body)
            .map_err(map_ureq_error)?;
        let parsed: OpenResponse = response
            .into_json()
            .map_err(|err| BrokerError::Protocol(err.to_string()))?;
        Ok(parsed.db_id)
    }

    pub fn query(&self, db_id: u64, sql: &str, max_rows: usize) -> Result<QueryPage> {
        self.query_with_params(db_id, sql, &[], max_rows)
    }

    pub fn query_with_params(
        &self,
        db_id: u64,
        sql: &str,
        params: &[Cell],
        max_rows: usize,
    ) -> Result<QueryPage> {
        let body = json!({
            "db_id": db_id,
            "sql": sql,
            "max_rows": max_rows,
            "params": params.iter().map(cell_to_json).collect::<Vec<_>>(),
        });
        let response = self
            .authed(self.agent.post(&format!("{}/v1/db/query", self.base_url)))
            .send_json(body)
            .map_err(map_ureq_error)?;
        let parsed: QueryResponse = response
            .into_json()
            .map_err(|err| BrokerError::Protocol(err.to_string()))?;

        let mut rows = Vec::with_capacity(parsed.rows.len());
        for row in &parsed.rows {
            let mut cells = Vec::with_capacity(parsed.columns.len());
            for column in &parsed.columns {
                cells.push(cell_from_json(row.get(column)));
            }
            rows.push(cells);
        }
        Ok(QueryPage {
            columns: parsed.columns,
            rows,
            truncated: parsed.truncated,
        })
    }

    pub fn action(
        &self,
        action: &str,
        account: &str,
        account_directory: &str,
        recipient: &str,
        payload: &str,
        dry_run: bool,
    ) -> Result<Value> {
        let body = json!({
            "action": action,
            "account": account,
            "account_directory": account_directory,
            "recipient": recipient,
            "payload": payload,
            "dry_run": dry_run,
        });
        let response = self
            .authed(
                self.agent
                    .post(&format!("{}/v1/wechat/action", self.base_url)),
            )
            .send_json(body)
            .map_err(map_ureq_error)?;
        response
            .into_json()
            .map_err(|err| BrokerError::Protocol(err.to_string()))
    }

    pub fn close(&self, db_id: u64) -> Result<()> {
        let body = json!({ "db_id": db_id });
        self.authed(self.agent.post(&format!("{}/v1/db/close", self.base_url)))
            .send_json(body)
            .map_err(map_ureq_error)?;
        Ok(())
    }
}

fn map_ureq_error(error: ureq::Error) -> BrokerError {
    match error {
        ureq::Error::Status(status, response) => {
            let message = response.into_string().unwrap_or_default();
            BrokerError::Server { status, message }
        }
        ureq::Error::Transport(transport) => BrokerError::Http(transport.to_string()),
    }
}

fn cell_to_json(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Integer(value) => json!(value),
        Cell::Real(value) => json!(value),
        Cell::Text(value) => json!(value),
        Cell::Blob(bytes) => json!({
            "$b64": base64::engine::general_purpose::STANDARD.encode(bytes)
        }),
    }
}

fn cell_from_json(value: Option<&Value>) -> Cell {
    match value {
        None | Some(Value::Null) => Cell::Null,
        Some(Value::Bool(flag)) => Cell::Integer(i64::from(*flag)),
        Some(Value::Number(number)) => {
            if let Some(integer) = number.as_i64() {
                Cell::Integer(integer)
            } else if let Some(real) = number.as_f64() {
                Cell::Real(real)
            } else {
                Cell::Null
            }
        }
        Some(Value::String(text)) => Cell::Text(text.clone()),
        Some(Value::Object(object)) => match object.get("$b64") {
            Some(Value::String(encoded)) => Cell::Blob(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap_or_default(),
            ),
            _ => Cell::Text(Value::Object(object.clone()).to_string()),
        },
        Some(Value::Array(items)) => Cell::Text(Value::Array(items.clone()).to_string()),
    }
}

pub fn parse_ready_line(line: &str) -> Option<u16> {
    let tail = line.split("listening on http://").nth(1)?;
    tail.rsplit(':').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready_line() {
        assert_eq!(
            parse_ready_line("wce-self-broker listening on http://127.0.0.1:8765"),
            Some(8765)
        );
        assert_eq!(parse_ready_line("token: abc"), None);
        assert_eq!(
            parse_ready_line("listening on http://127.0.0.1:notaport"),
            None
        );
    }

    #[test]
    fn decodes_blob_marker() {
        let value = json!({"$b64": "AQID"});
        assert_eq!(cell_from_json(Some(&value)), Cell::Blob(vec![1, 2, 3]));
    }

    #[test]
    fn maps_scalar_cells() {
        assert_eq!(cell_from_json(None), Cell::Null);
        assert_eq!(cell_from_json(Some(&json!(42))), Cell::Integer(42));
        assert_eq!(cell_from_json(Some(&json!(1.5))), Cell::Real(1.5));
        assert_eq!(
            cell_from_json(Some(&json!("hello"))),
            Cell::Text("hello".into())
        );
    }
}
