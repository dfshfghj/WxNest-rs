use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use wechat_data::{
    find_key_for_db_storage_with_account, parse_message_body, BrokerOptions, Message,
    MessageCursor, MessageKind, MessageQuery, RealtimeAccountStore,
};

mod media;
mod stream;

use media::MediaContext;
use stream::{StreamAgent, StreamRecord};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchConfig {
    #[serde(default)]
    pub account_root: PathBuf,
    #[serde(default)]
    pub key_store: PathBuf,
    #[serde(default)]
    pub broker: PathBuf,
    #[serde(default)]
    pub account_hint: Option<String>,
    #[serde(default)]
    pub cursor_dir: PathBuf,
    #[serde(default)]
    pub log_dir: PathBuf,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    #[serde(default)]
    pub include_self: bool,
    #[serde(default)]
    pub bridge: PathBuf,
    #[serde(default = "default_reply_mode")]
    pub reply_mode: String,
    #[serde(default = "default_prompt_template")]
    pub prompt_template: String,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub sessions: Vec<SessionConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub new_session_args: Vec<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_args: Vec<String>,
    #[serde(default = "default_stream_idle")]
    pub stream_idle_secs: u64,
    #[serde(default = "default_mcp_server")]
    pub mcp_server: String,
    #[serde(default = "default_permission")]
    pub permission: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::new(),
            args: Vec::new(),
            new_session_args: Vec::new(),
            resume_args: Vec::new(),
            stream: false,
            stream_args: Vec::new(),
            stream_idle_secs: default_stream_idle(),
            mcp_server: default_mcp_server(),
            permission: default_permission(),
            timeout_secs: default_timeout(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionConfig {
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub permission: String,
    #[serde(default)]
    pub project_dir: Option<PathBuf>,
    #[serde(default = "default_session_mode")]
    pub session_mode: String,
    #[serde(default)]
    pub agent_program: PathBuf,
    #[serde(default)]
    pub agent_args: Vec<String>,
    #[serde(default)]
    pub new_session_args: Vec<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_args: Vec<String>,
    #[serde(default)]
    pub reply_mode: String,
}

#[derive(Clone, Debug)]
struct AgentSpec {
    program: PathBuf,
    args: Vec<String>,
    new_session_args: Vec<String>,
    resume_args: Vec<String>,
    stream: bool,
    stream_args: Vec<String>,
    reply_mode: String,
}

fn resolve_agent(config: &WatchConfig, session: &SessionConfig) -> AgentSpec {
    let own = !session.agent_program.as_os_str().is_empty();
    AgentSpec {
        program: if own {
            session.agent_program.clone()
        } else {
            config.agent.program.clone()
        },
        args: if own && !session.agent_args.is_empty() {
            session.agent_args.clone()
        } else {
            config.agent.args.clone()
        },
        new_session_args: if own && !session.new_session_args.is_empty() {
            session.new_session_args.clone()
        } else {
            config.agent.new_session_args.clone()
        },
        resume_args: if own && !session.resume_args.is_empty() {
            session.resume_args.clone()
        } else {
            config.agent.resume_args.clone()
        },
        stream: if own {
            session.stream
        } else {
            config.agent.stream
        },
        stream_args: if own && !session.stream_args.is_empty() {
            session.stream_args.clone()
        } else {
            config.agent.stream_args.clone()
        },
        reply_mode: if own && !session.reply_mode.is_empty() {
            session.reply_mode.clone()
        } else {
            config.reply_mode.clone()
        },
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AgentSession {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_used: u64,
    #[serde(default)]
    pub started: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionStore {
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub sessions: Vec<AgentSession>,
}

pub const SESSION_STORE_FILE: &str = ".wce-sessions.json";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub fn load_session_store(project_dir: &Path) -> SessionStore {
    let path = project_dir.join(SESSION_STORE_FILE);
    let mut store: SessionStore = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let legacy = project_dir.join(".wce-session");
    if let Ok(id) = std::fs::read_to_string(&legacy) {
        let id = id.trim().to_string();
        if !id.is_empty() && !store.sessions.iter().any(|item| item.id == id) {
            let now = now_secs();
            store.sessions.push(AgentSession {
                id: id.clone(),
                created_at: now,
                last_used: now,
                started: true,
            });
            store.current = Some(id);
            save_session_store(project_dir, &store);
        }
    }
    store
}

pub fn save_session_store(project_dir: &Path, store: &SessionStore) {
    if std::fs::create_dir_all(project_dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(store) {
        let _ = std::fs::write(project_dir.join(SESSION_STORE_FILE), text);
    }
}

pub fn add_session(project_dir: &Path, store: &mut SessionStore) -> AgentSession {
    let now = now_secs();
    let entry = AgentSession {
        id: new_session_id(),
        created_at: now,
        last_used: 0,
        started: false,
    };
    store.current = Some(entry.id.clone());
    store.sessions.push(entry.clone());
    save_session_store(project_dir, store);
    entry
}

pub fn set_current_session(project_dir: &Path, store: &mut SessionStore, id: &str) -> bool {
    if !store.sessions.iter().any(|item| item.id == id) {
        return false;
    }
    store.current = Some(id.to_string());
    save_session_store(project_dir, store);
    true
}

pub fn effective_session(store: &SessionStore) -> Option<AgentSession> {
    if let Some(current) = store.current.as_ref() {
        if let Some(found) = store.sessions.iter().find(|item| &item.id == current) {
            return Some(found.clone());
        }
    }
    store
        .sessions
        .iter()
        .max_by_key(|item| (item.last_used, item.created_at))
        .cloned()
}

fn default_poll_ms() -> u64 {
    700
}

fn default_reply_mode() -> String {
    "mcp".to_string()
}

fn default_session_mode() -> String {
    "persistent".to_string()
}

fn default_mcp_server() -> String {
    "wechat-ai".to_string()
}

fn default_permission() -> String {
    "bypass_permissions".to_string()
}

fn default_timeout() -> u64 {
    180
}

fn default_stream_idle() -> u64 {
    900
}

fn default_true() -> bool {
    true
}

pub const DEFAULT_PROMPT_TEMPLATE: &str = "你是微信自动回复执行器。\n会话：{display_name}（wxid: {session}）\n新消息：\n{messages}\n\n规则：\n{prompt}\n\n说明：人名后的括号是 wxid；同一个显示名可能对应不同人，认人以内层 wxid 为准。消息里的 [图片]/[表情] 附有本地路径，可直接读取查看；[文件]/[视频] 附有本地路径。\n\n输出协议（每行一个动作，只有以协议前缀开头的行会被发送）：\n- REPLY: <文本>  发送一条文字\n- IMAGE: <本地绝对路径>  发送图片\n- FILE: <本地绝对路径>  发送文件\n- SKIP  表示不回复\n只输出需要执行的动作，不要解释过程；没有动作就只输出 SKIP。";

fn default_prompt_template() -> String {
    DEFAULT_PROMPT_TEMPLATE.to_string()
}

#[derive(Clone, Debug, Serialize)]
pub struct DetectedAgent {
    pub name: String,
    pub program: PathBuf,
    pub default_args: Vec<String>,
    pub new_session_args: Vec<String>,
    pub resume_args: Vec<String>,
    pub stream_args: Vec<String>,
    pub permission_values: Vec<String>,
    pub verified: bool,
}

fn stream_args_for(name: &str) -> Vec<String> {
    if name == "qoderclicn" || name == "claude" {
        vec![
            "-p".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--permission-mode".into(),
            "{permission}".into(),
        ]
    } else {
        Vec::new()
    }
}

pub fn detect_agents() -> Vec<DetectedAgent> {
    let mut found = Vec::new();
    if let Some(program) = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|home| home.join(".qodersec").join("bin").join("qoderclicn.exe"))
        .filter(|path| path.is_file())
    {
        found.push(DetectedAgent {
            name: "qoderclicn".to_string(),
            program,
            default_args: vec![
                "-p".into(),
                "--permission-mode".into(),
                "{permission}".into(),
                "{prompt}".into(),
            ],
            new_session_args: vec!["--session-id".into(), "{session_id}".into()],
            resume_args: vec!["-r".into(), "{session_id}".into()],
            stream_args: stream_args_for("qoderclicn"),
            permission_values: vec![
                "bypass_permissions".into(),
                "auto".into(),
                "accept_edits".into(),
                "default".into(),
            ],
            verified: true,
        });
    }
    let path_candidates = [
        (
            "claude",
            &["claude.exe", "claude.cmd", "claude.bat"][..],
            vec![
                "-p".into(),
                "--permission-mode".into(),
                "{permission}".into(),
                "{prompt}".into(),
            ],
            vec![
                "bypassPermissions".into(),
                "acceptEdits".into(),
                "default".into(),
                "plan".into(),
                "dontAsk".into(),
                "auto".into(),
            ],
        ),
        (
            "codex",
            &["codex.exe", "codex.cmd", "codex.bat"][..],
            vec![
                "exec".into(),
                "-s".into(),
                "danger-full-access".into(),
                "-a".into(),
                "never".into(),
                "{prompt}".into(),
            ],
            Vec::new(),
        ),
        (
            "gemini",
            &["gemini.exe", "gemini.cmd", "gemini.bat"][..],
            vec!["-p".into(), "{prompt}".into(), "--yolo".into()],
            Vec::new(),
        ),
        (
            "cursor-agent",
            &["cursor-agent.exe", "cursor-agent.cmd", "cursor-agent.bat"][..],
            vec!["-p".into(), "{prompt}".into(), "--force".into()],
            Vec::new(),
        ),
    ];
    for (name, files, default_args, permission_values) in path_candidates {
        if let Some(program) = find_program(files) {
            let (new_session_args, resume_args) = if name == "claude" {
                (
                    vec!["--session-id".into(), "{session_id}".into()],
                    vec!["-r".into(), "{session_id}".into()],
                )
            } else {
                (Vec::new(), Vec::new())
            };
            found.push(DetectedAgent {
                name: name.to_string(),
                program,
                default_args,
                new_session_args,
                resume_args,
                stream_args: stream_args_for(name),
                permission_values,
                verified: false,
            });
        }
    }
    found
}

fn find_program(files: &[&str]) -> Option<PathBuf> {
    for file in files {
        if let Some(path) = find_in_path(file) {
            return Some(path);
        }
    }
    let mut roots = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Programs"));
    }
    for root in roots {
        for file in files {
            let candidate = root.join(file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_in_path(file: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        let candidate = entry.join(file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn run(config_path: &Path, once: bool) -> Result<(), String> {
    let _lock = LockFile::acquire(&lock_path(config_path))?;
    let mut config = load_config(config_path)?;
    if config.account_root.as_os_str().is_empty() {
        return Err("account_root is required".to_string());
    }
    std::fs::create_dir_all(&config.log_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&config.cursor_dir).map_err(|error| error.to_string())?;
    let mut store = open_store(&config)?;
    let mut media = MediaContext::new(
        store.account_root().map(Path::to_path_buf),
        &config.account_root,
        &config.key_store,
        config.log_dir.join("media"),
    );
    let mut agents: HashMap<String, StreamAgent> = HashMap::new();
    log_event(
        &config,
        serde_json::json!({"event": "start", "pid": std::process::id()}),
    );
    loop {
        let latest = match load_config(config_path) {
            Ok(value) => value,
            Err(error) => {
                if once {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if latest.account_root != config.account_root
            || latest.key_store != config.key_store
            || latest.broker != config.broker
        {
            config = latest;
            store = open_store(&config)?;
            media = MediaContext::new(
                store.account_root().map(Path::to_path_buf),
                &config.account_root,
                &config.key_store,
                config.log_dir.join("media"),
            );
        } else {
            config = latest;
        }
        prune_agents(&mut agents, &config);
        for session in config.sessions.clone() {
            if !session.enabled || session.username.trim().is_empty() {
                continue;
            }
            if let Err(error) = poll_session(&config, &store, &session, &mut agents, &mut media) {
                log_event(
                    &config,
                    serde_json::json!({
                        "event": "error",
                        "session": session.username,
                        "message": error,
                    }),
                );
            }
        }
        write_status(&config, false);
        if once {
            break;
        }
        std::thread::sleep(Duration::from_millis(config.poll_ms.max(200)));
    }
    log_event(&config, serde_json::json!({"event": "stop"}));
    Ok(())
}

fn open_store(config: &WatchConfig) -> Result<RealtimeAccountStore, String> {
    let (key, raw_key, _) =
        find_key_for_db_storage_with_account(&config.key_store, &config.account_root)
            .map_err(|error| format!("read key store failed: {error}"))?;
    let wants_stdout = if config.sessions.iter().any(|session| session.enabled) {
        config
            .sessions
            .iter()
            .filter(|session| session.enabled)
            .any(|session| resolve_agent(config, session).reply_mode == "stdout")
    } else {
        config.reply_mode == "stdout"
    };
    let native_bridge = wants_stdout
        .then(|| config.bridge.clone())
        .filter(|path| path.is_file());
    RealtimeAccountStore::open_with_options(
        &config.broker,
        &config.account_root,
        &key,
        raw_key,
        config.account_hint.as_deref(),
        &BrokerOptions {
            native_bridge,
            native_work_dir: Some(config.log_dir.join("native-actions")),
            native_timeout_ms: None,
        },
    )
    .map_err(|error| format!("open account failed: {error}"))
}

fn prune_agents(agents: &mut HashMap<String, StreamAgent>, config: &WatchConfig) {
    let idle_limit = Duration::from_secs(config.agent.stream_idle_secs.max(30));
    let enabled: Vec<&str> = config
        .sessions
        .iter()
        .filter(|session| session.enabled)
        .map(|session| session.username.as_str())
        .collect();
    agents.retain(|username, agent| {
        if !config.agent.stream || !enabled.contains(&username.as_str()) {
            return false;
        }
        if !agent.alive() || agent.idle_for() > idle_limit {
            return false;
        }
        true
    });
}

fn poll_session(
    config: &WatchConfig,
    store: &RealtimeAccountStore,
    session: &SessionConfig,
    agents: &mut HashMap<String, StreamAgent>,
    media: &mut MediaContext,
) -> Result<(), String> {
    let cursor = load_cursor(config, &session.username);
    if cursor.is_none() {
        let latest = store
            .load_messages(
                &session.username,
                &MessageQuery {
                    limit: 1,
                    before: None,
                },
            )
            .map_err(|error| error.to_string())?;
        if let Some(message) = latest.messages.last() {
            save_cursor(
                config,
                &session.username,
                &MessageCursor {
                    create_time: message.create_time,
                    sort_seq: message.sort_seq,
                    local_id: message.local_id,
                    shard: message.shard.clone(),
                },
            );
        }
        return Ok(());
    }
    let page = store
        .load_messages_after(&session.username, cursor.as_ref(), 50)
        .map_err(|error| error.to_string())?;
    if page.messages.is_empty() {
        return Ok(());
    }
    let fresh: Vec<Message> = page
        .messages
        .iter()
        .filter(|message| config.include_self || !message.is_sent)
        .cloned()
        .collect();
    if !fresh.is_empty() {
        trigger(config, store, session, &fresh, agents, media)?;
    }
    if let Some(next) = page.next_after {
        save_cursor(config, &session.username, &next);
    }
    Ok(())
}

fn trigger(
    config: &WatchConfig,
    store: &RealtimeAccountStore,
    session: &SessionConfig,
    messages: &[Message],
    agents: &mut HashMap<String, StreamAgent>,
    media: &mut MediaContext,
) -> Result<(), String> {
    let spec = resolve_agent(config, session);
    if spec.program.as_os_str().is_empty() {
        return Err("agent program is not configured".to_string());
    }
    let messages_text = format_messages(store, session, messages, media);
    let permission = if session.permission.trim().is_empty() {
        config.agent.permission.as_str()
    } else {
        session.permission.trim()
    };
    let template = if config.prompt_template.trim().is_empty() {
        DEFAULT_PROMPT_TEMPLATE
    } else {
        config.prompt_template.as_str()
    };
    let prompt = fill(
        template,
        &[
            ("session", &session.username),
            ("display_name", &display_name(session)),
            ("messages", &messages_text),
            ("prompt", session.prompt.trim()),
            ("mcp_server", &config.agent.mcp_server),
            ("permission", permission),
        ],
    );
    let project_dir = session_project_dir(config, session);
    ensure_project_files(config, session, &project_dir);
    let persistent = session.session_mode != "fresh";
    let mut session_store = load_session_store(&project_dir);
    let entry = if persistent {
        effective_session(&session_store)
            .unwrap_or_else(|| add_session(&project_dir, &mut session_store))
    } else {
        add_session(&project_dir, &mut session_store)
    };
    let session_id = entry.id.clone();
    let continued = persistent && entry.started;
    let mut args: Vec<String> = spec
        .args
        .iter()
        .map(|arg| {
            fill(
                arg,
                &[
                    ("session", &session.username),
                    ("display_name", &display_name(session)),
                    ("messages", &messages_text),
                    ("prompt", &prompt),
                    ("mcp_server", &config.agent.mcp_server),
                    ("permission", permission),
                    ("session_id", &session_id),
                ],
            )
        })
        .collect();
    let session_args = if continued {
        &spec.resume_args
    } else {
        &spec.new_session_args
    };
    if !session_args.is_empty() {
        let insert_at = spec
            .args
            .iter()
            .position(|arg| arg.contains("{prompt}"))
            .unwrap_or(args.len());
        let insert_at = insert_at.min(args.len());
        let expanded: Vec<String> = session_args
            .iter()
            .map(|arg| {
                fill(
                    arg,
                    &[
                        ("session_id", &session_id),
                        ("session", &session.username),
                        ("mcp_server", &config.agent.mcp_server),
                        ("permission", permission),
                    ],
                )
            })
            .collect();
        args.splice(insert_at..insert_at, expanded);
    }
    let started = Instant::now();
    let turn = now_secs();
    transcript_push(
        config,
        &session.username,
        &session_id,
        turn,
        "user",
        &messages_text,
        "",
        false,
    );
    let stream = spec.stream && spec.reply_mode == "stdout";
    let timeout = Duration::from_secs(config.agent.timeout_secs.max(10));
    let (exit, stdout, stderr, args, stream_session) = if stream {
        let mut record = |event: StreamRecord| match event {
            StreamRecord::Thinking(text) => {
                transcript_push(
                    config,
                    &session.username,
                    &session_id,
                    turn,
                    "thinking",
                    &text,
                    "",
                    false,
                );
            }
            StreamRecord::Assistant(text) => {
                transcript_push(
                    config,
                    &session.username,
                    &session_id,
                    turn,
                    "assistant",
                    &text,
                    "",
                    false,
                );
            }
            StreamRecord::ToolUse { name, input } => {
                transcript_push(
                    config,
                    &session.username,
                    &session_id,
                    turn,
                    "tool",
                    &input,
                    &name,
                    false,
                );
            }
            StreamRecord::ToolResult { text, error } => {
                transcript_push(
                    config,
                    &session.username,
                    &session_id,
                    turn,
                    "tool_result",
                    &text,
                    "",
                    error,
                );
            }
        };
        match run_stream(
            config,
            session,
            &spec,
            permission,
            &prompt,
            &session_id,
            continued,
            &project_dir,
            agents,
            timeout,
            &mut record,
        ) {
            Ok(value) => value,
            Err(error) => {
                transcript_push(
                    config,
                    &session.username,
                    &session_id,
                    turn,
                    "error",
                    &error,
                    "",
                    true,
                );
                if let Some(found) = session_store
                    .sessions
                    .iter_mut()
                    .find(|item| item.id == session_id)
                {
                    if continued {
                        found.started = false;
                        save_session_store(&project_dir, &session_store);
                    }
                }
                log_event(
                    config,
                    serde_json::json!({
                        "event": "error",
                        "session": session.username,
                        "stream": true,
                        "message": error,
                    }),
                );
                return Err(error);
            }
        }
    } else {
        let (exit, stdout, stderr) = run_command(&spec.program, &args, &project_dir, timeout)?;
        (exit, stdout, stderr, args.clone(), None)
    };
    let effective_session = stream_session.unwrap_or_else(|| session_id.clone());
    if let Some(found) = session_store
        .sessions
        .iter_mut()
        .find(|item| item.id == session_id)
    {
        if exit == Some(0) {
            found.started = true;
            found.last_used = now_secs();
            session_store.current = Some(effective_session.clone());
        } else if continued {
            found.started = false;
        }
        save_session_store(&project_dir, &session_store);
    }
    let actions = if spec.reply_mode == "stdout" {
        parse_actions(&stdout)
    } else {
        Vec::new()
    };
    let mut sent_actions: Vec<String> = Vec::new();
    let mut action_errors: Vec<String> = Vec::new();
    let mut reply: Option<String> = None;
    let mut skipped = false;
    for action in actions {
        let result = match action {
            OutputAction::Text(text) => {
                reply = Some(text.clone());
                store
                    .action_client()
                    .send_text(&session.username, &text)
                    .map(|_| "text".to_string())
            }
            OutputAction::Image(path) => store
                .action_client()
                .send_image(&session.username, &path)
                .map(|_| format!("image:{path}")),
            OutputAction::File(path) => store
                .action_client()
                .send_file(&session.username, &path)
                .map(|_| format!("file:{path}")),
            OutputAction::Skip => {
                skipped = true;
                continue;
            }
        };
        match result {
            Ok(value) => sent_actions.push(value),
            Err(error) => action_errors.push(error.to_string()),
        }
    }
    let sent = !sent_actions.is_empty();
    if sent {
        transcript_push(
            config,
            &session.username,
            &effective_session,
            turn,
            "final",
            &stdout,
            "",
            exit != Some(0),
        );
    } else if skipped || stdout.trim().eq_ignore_ascii_case("SKIP") {
        transcript_push(
            config,
            &session.username,
            &effective_session,
            turn,
            "skip",
            "本轮不回复（SKIP）",
            "",
            false,
        );
    } else if spec.reply_mode == "stdout" {
        transcript_push(
            config,
            &session.username,
            &effective_session,
            turn,
            "skip",
            "本轮没有产生回复",
            "",
            false,
        );
    } else if spec.reply_mode == "none" {
        transcript_push(
            config,
            &session.username,
            &effective_session,
            turn,
            "note",
            "仅记录模式：未发送",
            "",
            false,
        );
    } else {
        transcript_push(
            config,
            &session.username,
            &effective_session,
            turn,
            "note",
            "agent 已直接发送（MCP）",
            "",
            false,
        );
    }
    log_event(
        config,
        serde_json::json!({
            "event": "agent",
            "session": session.username,
            "display_name": display_name(session),
            "messages": messages.len(),
            "prompt": prompt,
            "args": args,
            "exit": exit,
            "elapsed_ms": started.elapsed().as_millis(),
            "stdout": truncate(&stdout, 4000),
            "stderr": truncate(&stderr, 4000),
            "reply": reply,
            "reply_sent": sent,
            "actions": sent_actions,
            "action_errors": action_errors,
            "session_id": effective_session,
            "continued": continued,
            "stream": stream,
        }),
    );
    Ok(())
}

fn run_command(
    program: &Path,
    args: &[String],
    work_dir: &Path,
    timeout: Duration,
) -> Result<(Option<i32>, String, String), String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(work_dir)
        .env_remove("QODER_AGENT_SDK_ENTRYPOINT")
        .env_remove("QODER_SDK_AUTH_PAYLOAD_FILE")
        .env_remove("QODERCN_SESSION_TYPE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn agent failed: {error}"))?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (tx, rx) = mpsc::channel();
    if let Some(pipe) = stdout_pipe {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buffer = String::new();
            let mut file = pipe;
            let _ = file.read_to_string(&mut buffer);
            let _ = tx.send(("stdout", buffer));
        });
    }
    if let Some(pipe) = stderr_pipe {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buffer = String::new();
            let mut file = pipe;
            let _ = file.read_to_string(&mut buffer);
            let _ = tx.send(("stderr", buffer));
        });
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(format!("wait agent failed: {error}")),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    let mut received = 0;
    while received < 2 && Instant::now() < drain_deadline {
        if let Ok((kind, text)) = rx.recv_timeout(Duration::from_millis(200)) {
            received += 1;
            match kind {
                "stdout" => stdout.push_str(&text),
                _ => stderr.push_str(&text),
            }
        }
    }
    Ok((status.and_then(|status| status.code()), stdout, stderr))
}

type StreamRun = (Option<i32>, String, String, Vec<String>, Option<String>);

#[allow(clippy::too_many_arguments)]
fn run_stream(
    config: &WatchConfig,
    session: &SessionConfig,
    spec: &AgentSpec,
    permission: &str,
    prompt: &str,
    session_id: &str,
    continued: bool,
    project_dir: &Path,
    agents: &mut HashMap<String, StreamAgent>,
    timeout: Duration,
    record: &mut dyn FnMut(StreamRecord),
) -> Result<StreamRun, String> {
    if spec.stream_args.is_empty() {
        return Err("agent stream_args is empty (需要 --input-format stream-json)".to_string());
    }
    let placeholders = [
        ("session", session.username.as_str()),
        ("display_name", &display_name(session)),
        ("prompt", ""),
        ("mcp_server", config.agent.mcp_server.as_str()),
        ("permission", permission),
        ("session_id", session_id),
    ];
    let mut args: Vec<String> = spec
        .stream_args
        .iter()
        .map(|arg| fill(arg, &placeholders))
        .collect();
    let fingerprint = format!(
        "{}|{}|{}",
        spec.program.display(),
        args.join("\u{1f}"),
        project_dir.display()
    );
    let session_args = if continued {
        &spec.resume_args
    } else {
        &spec.new_session_args
    };
    args.extend(session_args.iter().map(|arg| fill(arg, &placeholders)));
    let reusable = session.session_mode != "fresh"
        && agents
            .get_mut(&session.username)
            .map(|agent| {
                agent.alive()
                    && agent.fingerprint() == fingerprint
                    && agent
                        .session_id()
                        .map(|id| id == session_id)
                        .unwrap_or(true)
            })
            .unwrap_or(false);
    if !reusable {
        agents.remove(&session.username);
        let agent = StreamAgent::spawn(&spec.program, &args, project_dir, fingerprint.clone())?;
        agents.insert(session.username.clone(), agent);
    }
    let agent = agents
        .get_mut(&session.username)
        .ok_or_else(|| "stream agent missing".to_string())?;
    let result = agent.send(prompt, timeout, record);
    match result {
        Ok((text, returned_id)) => Ok((Some(0), text, String::new(), args, returned_id)),
        Err(error) => {
            agents.remove(&session.username);
            Err(error)
        }
    }
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub fn ensure_session_project(config: &WatchConfig, session: &SessionConfig) {
    let directory = session_project_dir(config, session);
    ensure_project_files(config, session, &directory);
}

fn session_project_dir(config: &WatchConfig, session: &SessionConfig) -> PathBuf {
    session
        .project_dir
        .clone()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| {
            config
                .log_dir
                .join("projects")
                .join(safe_name(&session.username))
        })
}

fn ensure_project_files(config: &WatchConfig, session: &SessionConfig, project_dir: &Path) {
    if std::fs::create_dir_all(project_dir).is_err() {
        return;
    }
    let path = project_dir.join("AGENTS.md");
    let block = format!(
        "# 微信自动回复 · {}

<!-- wce:begin -->
- 会话：{}（{}）
- 回复方式：{}
- MCP 服务：{}
- 补充规则：
{}
- 每轮只处理给出的新消息，最多回复 1 条；不需要回复输出 SKIP；stdout 模式可输出 `REPLY: <文本>`、`IMAGE: <绝对路径>`、`FILE: <绝对路径>`。
- 人名后的括号是 wxid，认人以内层 wxid 为准；[图片]/[表情]/[文件]/[视频] 后的本地路径可直接读取。
<!-- wce:end -->
",
        display_name(session),
        display_name(session),
        session.username,
        resolve_agent(config, session).reply_mode,
        config.agent.mcp_server,
        if session.prompt.trim().is_empty() {
            "根据内容判断；不确定就不回复。"
        } else {
            session.prompt.trim()
        }
    );
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = replace_managed_block(&existing, &block);
    let _ = std::fs::write(&path, updated);
    let claude = project_dir.join("CLAUDE.md");
    if !claude.exists() {
        let _ = std::fs::write(
            claude,
            "Follow AGENTS.md in this directory for WeChat auto-reply rules.
",
        );
    }
}

fn replace_managed_block(existing: &str, block: &str) -> String {
    const BEGIN: &str = "<!-- wce:begin -->";
    const END: &str = "<!-- wce:end -->";
    if let (Some(start), Some(end)) = (existing.find(BEGIN), existing.find(END)) {
        if end > start {
            let mut output = String::new();
            output.push_str(&existing[..start]);
            output.push_str(block);
            output.push_str(&existing[end + END.len()..]);
            return output;
        }
    }
    if existing.trim().is_empty() {
        block.to_string()
    } else {
        format!(
            "{existing}

{block}"
        )
    }
}

fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mix = |mut value: u64| {
        value ^= value >> 30;
        value = value.wrapping_mul(0xbf58476d1ce4e5b9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    };
    let a = mix(nanos ^ pid.rotate_left(17));
    let b = mix(nanos.rotate_left(29) ^ pid);
    format!(
        "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a & 0x0fff,
        (b >> 52) & 0x0fff,
        b & 0xffffffffffff
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputAction {
    Text(String),
    Image(String),
    File(String),
    Skip,
}

pub fn parse_actions(stdout: &str) -> Vec<OutputAction> {
    let mut actions = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim().trim_matches('"').trim();
        if trimmed.eq_ignore_ascii_case("SKIP") {
            return vec![OutputAction::Skip];
        }
        let lower = trimmed.to_ascii_lowercase();
        let capture = |prefixes: &[&str]| -> Option<String> {
            prefixes.iter().find_map(|prefix| {
                lower
                    .strip_prefix(prefix)
                    .map(|_| trimmed[prefix.len()..].trim().to_string())
            })
        };
        if let Some(text) = capture(&["reply:"]) {
            if !text.is_empty() {
                actions.push(OutputAction::Text(text));
            }
        } else if let Some(path) = capture(&["send_image:", "image:"]) {
            if !path.is_empty() {
                actions.push(OutputAction::Image(path));
            }
        } else if let Some(path) = capture(&["send_file:", "file:"]) {
            if !path.is_empty() {
                actions.push(OutputAction::File(path));
            }
        }
        if actions.len() >= 4 {
            break;
        }
    }
    actions
}

fn format_messages(
    store: &RealtimeAccountStore,
    session: &SessionConfig,
    messages: &[Message],
    media: &mut MediaContext,
) -> String {
    let mut senders: Vec<String> = messages
        .iter()
        .map(|message| message.sender_username.clone())
        .filter(|value| !value.is_empty())
        .collect();
    senders.push(session.username.clone());
    senders.sort();
    senders.dedup();
    let contacts = store.load_contacts_for(&senders).unwrap_or_default();
    let group_names = if session.username.ends_with("@chatroom") {
        store
            .group_nicknames(&session.username, &senders)
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut output = String::new();
    for message in messages {
        let username = message.sender_username.as_str();
        let display = if message.is_sent {
            "我".to_string()
        } else {
            group_names
                .get(username)
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .or_else(|| {
                    let value = message.sender_display_name.trim();
                    (!value.is_empty()).then(|| value.to_string())
                })
                .or_else(|| {
                    contacts.get(username).and_then(|contact| {
                        [
                            contact.remark.as_str(),
                            contact.display_name.as_str(),
                            contact.nick_name.as_str(),
                        ]
                        .into_iter()
                        .find(|value| !value.trim().is_empty())
                        .map(|value| value.to_string())
                    })
                })
                .unwrap_or_else(|| username.to_string())
        };
        let label = if display == username {
            username.to_string()
        } else {
            format!("{display} ({username})")
        };
        let kind = format!("{:?}", message.kind);
        let mut content = summarize_content(message);
        if let Some(annotation) = media.annotate(&session.username, message) {
            if matches!(content.as_str(), "[图片]" | "[表情]" | "[语音]" | "[视频]") {
                content.clear();
            }
            let separator = if content.is_empty() { "" } else { "  " };
            output.push_str(&format!(
                "[{}] {} [{}]: {}{}{}\n",
                message.create_time, label, kind, content, separator, annotation
            ));
        } else {
            output.push_str(&format!(
                "[{}] {} [{}]: {}\n",
                message.create_time, label, kind, content
            ));
        }
    }
    output
}

fn summarize_content(message: &Message) -> String {
    if matches!(
        message.kind,
        MessageKind::Image | MessageKind::Emoji | MessageKind::Voice | MessageKind::Video
    ) {
        return summarize_kind(&message.kind);
    }
    let body = message
        .body
        .clone()
        .or_else(|| parse_message_body(message.local_type, &message.content));
    if let Some(body) = body.as_ref() {
        let value = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
        if let Some(text) = value.get("content").and_then(serde_json::Value::as_str) {
            return truncate(text, 300);
        }
    }
    truncate(&message.content, 300)
}

fn summarize_kind(kind: &MessageKind) -> String {
    match kind {
        MessageKind::Image => "[图片]".to_string(),
        MessageKind::Emoji => "[表情]".to_string(),
        MessageKind::Voice => "[语音]".to_string(),
        MessageKind::Video => "[视频]".to_string(),
        _ => String::new(),
    }
}

fn display_name(session: &SessionConfig) -> String {
    if session.display_name.trim().is_empty() {
        session.username.clone()
    } else {
        session.display_name.clone()
    }
}

fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut output = template.to_string();
    for (key, value) in values {
        output = output.replace(&format!("{{{key}}}"), value);
    }
    output
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect::<String>() + "…"
}

fn transcript_path(config: &WatchConfig, username: &str) -> PathBuf {
    config
        .log_dir
        .join("transcripts")
        .join(format!("{}.jsonl", safe_name(username)))
}

#[allow(clippy::too_many_arguments)]
fn transcript_push(
    config: &WatchConfig,
    username: &str,
    agent_session: &str,
    turn: u64,
    kind: &str,
    text: &str,
    name: &str,
    error: bool,
) {
    if text.trim().is_empty() && kind != "user" {
        return;
    }
    let path = transcript_path(config, username);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(metadata) = std::fs::metadata(&path) {
        if metadata.len() > 4_000_000 {
            let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
        }
    }
    let entry = serde_json::json!({
        "ts": now_secs(),
        "turn": turn,
        "kind": kind,
        "agent_session": agent_session,
        "name": name,
        "error": error,
        "text": truncate(text, 8000),
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{entry}");
    }
}

fn cursor_path(config: &WatchConfig, username: &str) -> PathBuf {
    let safe: String = username
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || value == '_' || value == '-' {
                value
            } else {
                '_'
            }
        })
        .collect();
    config.cursor_dir.join(format!("{safe}.json"))
}

fn load_cursor(config: &WatchConfig, username: &str) -> Option<MessageCursor> {
    let text = std::fs::read_to_string(cursor_path(config, username)).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_cursor(config: &WatchConfig, username: &str, cursor: &MessageCursor) {
    let _ = std::fs::create_dir_all(&config.cursor_dir);
    if let Ok(text) = serde_json::to_string(cursor) {
        let _ = std::fs::write(cursor_path(config, username), text);
    }
}

fn log_event(config: &WatchConfig, mut value: serde_json::Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "ts".to_string(),
            serde_json::json!(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0)),
        );
    }
    let _ = std::fs::create_dir_all(&config.log_dir);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.log_dir.join("events.jsonl"))
    {
        let _ = writeln!(file, "{value}");
    }
}

fn write_status(config: &WatchConfig, busy: bool) {
    let _ = std::fs::create_dir_all(&config.log_dir);
    let status = serde_json::json!({
        "pid": std::process::id(),
        "busy": busy,
        "updated": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
    });
    let _ = std::fs::write(
        config.log_dir.join("status.json"),
        serde_json::to_string_pretty(&status).unwrap_or_default(),
    );
}

fn load_config(path: &Path) -> Result<WatchConfig, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("read config failed: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("parse config failed: {error}"))
}

fn lock_path(config_path: &Path) -> PathBuf {
    config_path.with_extension("lock")
}

struct LockFile {
    path: PathBuf,
}

impl LockFile {
    fn acquire(path: &Path) -> Result<Self, String> {
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) => {
                    if attempt == 0 && error.kind() == std::io::ErrorKind::AlreadyExists {
                        let holder = std::fs::read_to_string(path)
                            .ok()
                            .and_then(|text| text.trim().parse::<u32>().ok());
                        if holder.map(|pid| !process_alive(pid)).unwrap_or(true) {
                            let _ = std::fs::remove_file(path);
                            continue;
                        }
                    }
                    return Err(format!("watcher already running: {}", path.display()));
                }
            }
        }
        Err(format!("watcher already running: {}", path.display()))
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_agent_overrides_global() {
        let config: WatchConfig = serde_json::from_value(serde_json::json!({
            "reply_mode": "mcp",
            "agent": {
                "program": "global.exe",
                "args": ["global"],
                "stream": true,
                "stream_args": ["global-stream"]
            }
        }))
        .expect("config");
        let empty: SessionConfig = serde_json::from_value(serde_json::json!({
            "username": "a"
        }))
        .expect("session");
        let inherited = resolve_agent(&config, &empty);
        assert_eq!(inherited.program.to_string_lossy(), "global.exe");
        assert!(inherited.stream);
        let own: SessionConfig = serde_json::from_value(serde_json::json!({
            "username": "b",
            "agent_program": "own.exe",
            "agent_args": ["own"],
            "reply_mode": "stdout",
            "stream": true,
            "stream_args": ["own-stream"]
        }))
        .expect("session");
        let spec = resolve_agent(&config, &own);
        assert_eq!(spec.program.to_string_lossy(), "own.exe");
        assert_eq!(spec.args, vec!["own".to_string()]);
        assert_eq!(spec.reply_mode, "stdout");
        assert_eq!(spec.stream_args, vec!["own-stream".to_string()]);
    }

    #[test]
    fn fills_placeholders() {
        let value = fill(
            "a {session} b {prompt}",
            &[("session", "x"), ("prompt", "y")],
        );
        assert_eq!(value, "a x b y");
    }

    #[test]
    fn truncates_utf8() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn parses_stdout_actions() {
        let output = "thinking\nREPLY: hello\nIMAGE: D:\\a.png\nFILE: D:\\b.txt\n";
        assert_eq!(
            parse_actions(output),
            vec![
                OutputAction::Text("hello".into()),
                OutputAction::Image(r"D:\a.png".into()),
                OutputAction::File(r"D:\b.txt".into())
            ]
        );
        assert_eq!(parse_actions("SKIP"), vec![OutputAction::Skip]);
        assert!(parse_actions("nothing").is_empty());
    }

    #[test]
    fn parses_quoted_and_prefixed_actions() {
        assert_eq!(
            parse_actions("\"REPLY: 你好\"\nIMAGE: x.png\nSEND_FILE: y.txt"),
            vec![
                OutputAction::Text("你好".into()),
                OutputAction::Image("x.png".into()),
                OutputAction::File("y.txt".into())
            ]
        );
        assert!(parse_actions("nothing").is_empty());
    }

    #[test]
    fn session_store_tracks_and_switches() {
        let dir = std::env::temp_dir().join(format!("wce-watch-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = SessionStore::default();
        let first = add_session(&dir, &mut store);
        let second = add_session(&dir, &mut store);
        assert_eq!(store.current.as_deref(), Some(second.id.as_str()));
        assert!(set_current_session(&dir, &mut store, &first.id));
        let loaded = load_session_store(&dir);
        assert_eq!(loaded.current.as_deref(), Some(first.id.as_str()));
        assert_eq!(
            effective_session(&loaded).map(|item| item.id),
            Some(first.id.clone())
        );
        std::fs::write(
            dir.join(".wce-session"),
            "legacy-id
",
        )
        .unwrap();
        let migrated = load_session_store(&dir);
        assert!(migrated.sessions.iter().any(|item| item.id == "legacy-id"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_ids_are_uuid_shaped() {
        let value = new_session_id();
        assert_eq!(value.len(), 36);
        assert_eq!(
            value.split('-').map(str::len).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(value.contains("-4"));
    }

    #[test]
    fn keeps_managed_block_edits() {
        let text = "# t
<!-- wce:begin -->
old
<!-- wce:end -->
user
";
        let updated = replace_managed_block(
            text,
            "<!-- wce:begin -->
new
<!-- wce:end -->
",
        );
        assert!(updated.contains("new"));
        assert!(updated.contains("user"));
        assert!(!updated.contains("old"));
    }

    #[test]
    fn detects_at_least_something_or_nothing() {
        let _ = detect_agents();
    }
}
