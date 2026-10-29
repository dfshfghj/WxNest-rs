use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

enum Event {
    Stdout(String),
    Stderr(String),
    Eof,
}

pub enum StreamRecord {
    Thinking(String),
    Assistant(String),
    ToolUse { name: String, input: String },
    ToolResult { text: String, error: bool },
}

pub struct StreamAgent {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Event>,
    session_id: Option<String>,
    fingerprint: String,
    last_used: Instant,
}

impl StreamAgent {
    pub fn spawn(
        program: &Path,
        args: &[String],
        work_dir: &Path,
        fingerprint: String,
    ) -> Result<Self, String> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(work_dir)
            .env_remove("QODER_AGENT_SDK_ENTRYPOINT")
            .env_remove("QODER_SDK_AUTH_PAYLOAD_FILE")
            .env_remove("QODERCN_SESSION_TYPE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn agent failed: {error}"))?;
        #[cfg(windows)]
        attach_kill_job(&child);
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "agent stdout unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "agent stderr unavailable".to_string())?;
        let (sender, events) = mpsc::channel();
        let out_sender = sender.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if out_sender.send(Event::Stdout(line)).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = out_sender.send(Event::Eof);
        });
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(Event::Stderr(line)).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            events,
            session_id: None,
            fingerprint,
            last_used: Instant::now(),
        })
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    pub fn send(
        &mut self,
        prompt: &str,
        timeout: Duration,
        record: &mut dyn FnMut(StreamRecord),
    ) -> Result<(String, Option<String>), String> {
        let message = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt },
        });
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "agent stdin unavailable".to_string())?;
        writeln!(stdin, "{message}")
            .map_err(|error| format!("write agent stdin failed: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("flush agent stdin failed: {error}"))?;
        self.last_used = Instant::now();
        let deadline = Instant::now() + timeout;
        let mut stderr_tail = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.kill();
                return Err(format!(
                    "agent timed out after {}s{}",
                    timeout.as_secs(),
                    tail_suffix(&stderr_tail)
                ));
            }
            let wait = remaining.min(Duration::from_millis(500));
            let event = match self.events.recv_timeout(wait) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    self.kill();
                    return Err(format!("agent pipe closed{}", tail_suffix(&stderr_tail)));
                }
            };
            match event {
                Event::Stderr(line) => {
                    stderr_tail.push_str(&line);
                    stderr_tail.push('\n');
                    if stderr_tail.len() > 8000 {
                        let split = stderr_tail.len() - 4000;
                        stderr_tail = stderr_tail.split_off(split);
                    }
                }
                Event::Eof => {
                    self.kill();
                    return Err(format!("agent exited{}", tail_suffix(&stderr_tail)));
                }
                Event::Stdout(line) => {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                        continue;
                    };
                    if let Some(id) = value.get("session_id").and_then(|item| item.as_str()) {
                        self.session_id = Some(id.to_string());
                    }
                    match value.get("type").and_then(|item| item.as_str()) {
                        Some("assistant") => record_assistant(&value, record),
                        Some("user") => record_tool_result(&value, record),
                        _ => {}
                    }
                    if value.get("type").and_then(|item| item.as_str()) == Some("result") {
                        let text = value
                            .get("result")
                            .and_then(|item| item.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let failed = value
                            .get("is_error")
                            .and_then(|item| item.as_bool())
                            .unwrap_or(false)
                            || value.get("subtype").and_then(|item| item.as_str()) == Some("error");
                        self.last_used = Instant::now();
                        if failed {
                            return Err(format!("agent error: {}", text.trim()));
                        }
                        return Ok((text, self.session_id.clone()));
                    }
                }
            }
        }
    }

    pub fn kill(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StreamAgent {
    fn drop(&mut self) {
        self.stdin.take();
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn record_assistant(value: &serde_json::Value, record: &mut dyn FnMut(StreamRecord)) {
    let Some(blocks) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
    else {
        return;
    };
    for block in blocks {
        let kind = block
            .get("type")
            .and_then(|item| item.as_str())
            .unwrap_or("");
        match kind {
            "thinking" => {
                if let Some(text) = block.get("thinking").and_then(|item| item.as_str()) {
                    if !text.trim().is_empty() {
                        record(StreamRecord::Thinking(text.to_string()));
                    }
                }
            }
            "text" => {
                if let Some(text) = block.get("text").and_then(|item| item.as_str()) {
                    if !text.trim().is_empty() {
                        record(StreamRecord::Assistant(text.to_string()));
                    }
                }
            }
            "tool_use" => {
                let name = block
                    .get("name")
                    .and_then(|item| item.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let input = block
                    .get("input")
                    .map(|item| item.to_string())
                    .unwrap_or_default();
                record(StreamRecord::ToolUse { name, input });
            }
            _ => {}
        }
    }
}

fn record_tool_result(value: &serde_json::Value, record: &mut dyn FnMut(StreamRecord)) {
    let Some(blocks) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
    else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(|item| item.as_str()) != Some("tool_result") {
            continue;
        }
        let text = match block.get("content") {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(|value| value.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let error = block
            .get("is_error")
            .and_then(|item| item.as_bool())
            .unwrap_or(false);
        record(StreamRecord::ToolResult { text, error });
    }
}

#[cfg(windows)]
fn attach_kill_job(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    let Some(job) = kill_on_close_job() else {
        return;
    };
    unsafe {
        windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
            job,
            child.as_raw_handle() as _,
        );
    }
}

#[cfg(windows)]
fn kill_on_close_job() -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    static JOB: OnceLock<usize> = OnceLock::new();
    let handle = JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return 0;
        }
        job as usize
    });
    (*handle != 0).then_some(*handle as _)
}

fn tail_suffix(tail: &str) -> String {
    let trimmed = tail.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {}", truncate_chars(trimmed, 400))
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect::<String>() + "…"
}
