use std::io::{BufRead, Write};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wechat_data::BrokerOptions;
use wechat_data::{
    extract_image_md5, fetch_emoticon_media, load_emoji_image, load_message_image,
    load_message_image_cached, parse_emoji_reference, ActionClient, ImageKeys, KeyStore,
    MediaIndex, Message, MessageBody, MessageCursor, MessageKind, MessageQuery,
    RealtimeAccountStore,
};

struct McpState {
    account: RealtimeAccountStore,
    media: Mutex<Option<MediaIndex>>,
    files: Mutex<HashMap<String, Option<String>>>,
    image_keys: Option<ImageKeys>,
    media_dir: PathBuf,
    cursor_dir: PathBuf,
    decrypt: bool,
    remote_emoji: bool,
    decrypted: Mutex<HashMap<String, Option<String>>>,
}

pub fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    let options = BrokerOptions {
        native_bridge: config.bridge.clone(),
        native_work_dir: config.work_dir.clone(),
        native_timeout_ms: Some(15_000),
    };
    let account = RealtimeAccountStore::open_with_options(
        &config.broker,
        &config.root,
        &config.key,
        config.raw_key,
        config.account.as_deref(),
        &options,
    )
    .map_err(|error| format!("open realtime store failed: {error}"))?;
    let stored_image_keys = config
        .key_store
        .as_ref()
        .and_then(|path| KeyStore::load(path).ok())
        .and_then(|store| {
            store
                .image_keys_for_account(account.account())
                .or_else(|| store.image_keys_for_account(account.self_username()))
        });
    let image_keys = stored_image_keys.or_else(|| {
        let root = account.account_root()?;
        wechat_key_scan::recover_image_key_for_account(root, None)
            .ok()
            .map(|found| ImageKeys {
                aes_key: found.aes_key,
                xor_key: Some(found.xor_key),
            })
    });
    let state = McpState {
        account,
        media: Mutex::new(None),
        files: Mutex::new(HashMap::new()),
        image_keys,
        cursor_dir: config.media_dir.join(".cursors"),
        media_dir: config.media_dir,
        decrypt: config.decrypt,
        remote_emoji: config.remote_emoji,
        decrypted: Mutex::new(HashMap::new()),
    };
    eprintln!(
        "wechat-mcp ready account={} native_actions={}",
        state.account.account(),
        state.account.native_actions()
    );
    serve(state)
}

fn serve(state: McpState) -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": error.to_string() },
                });
                writeln!(stdout, "{response}").map_err(|error| error.to_string())?;
                stdout.flush().map_err(|error| error.to_string())?;
                continue;
            }
        };
        if let Some(response) = handle_request(&state, request) {
            writeln!(stdout, "{response}").map_err(|error| error.to_string())?;
            stdout.flush().map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn handle_request(state: &McpState, request: Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let id = id?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "wechat-mcp", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(state, &params),
        "notifications/initialized" => return None,
        _ => Err((-32601, format!("unknown method: {method}"))),
    };
    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    })
}

fn call_tool(state: &McpState, params: &Value) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "tools/call requires name".to_string()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match run_tool(state, name, &arguments) {
        Ok(value) => Ok(json!({
            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
        })),
        Err(message) => Ok(json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        })),
    }
}

fn run_tool(state: &McpState, name: &str, args: &Value) -> Result<Value, String> {
    let account = &state.account;
    match name {
        "wechat_status" => Ok(json!({
            "account": account.account(),
            "account_dir": account.account_dir().display().to_string(),
            "self_username": account.self_username(),
            "native_actions": account.native_actions(),
            "info": account.account_info(),
        })),
        "list_sessions" => {
            let hidden = args
                .get("include_hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let official = args
                .get("include_official")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let sessions = account
                .list_sessions(hidden, official)
                .map_err(|error| error.to_string())?;
            Ok(json!({ "sessions": sessions }))
        }
        "load_messages" => {
            let username = required_str(args, "username")?;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
            let before = args
                .get("before")
                .filter(|value| !value.is_null())
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| format!("before cursor is invalid: {error}"))?;
            let page = account
                .load_messages(&username, &MessageQuery { limit, before })
                .map_err(|error| error.to_string())?;
            Ok(json!({
                "username": username,
                "messages": messages_json(state, &username, &page.messages),
                "has_more": page.has_more,
                "next_before": page.next_before,
            }))
        }
        "list_contacts" => {
            let keyword = args.get("keyword").and_then(Value::as_str).unwrap_or("");
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
            let contacts = account
                .list_contact_directory(keyword, limit)
                .map_err(|error| error.to_string())?;
            Ok(json!({ "contacts": contacts }))
        }
        "search_messages" => {
            let query = required_str(args, "query")?;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
            let page = account
                .search_messages(&query, limit)
                .map_err(|error| error.to_string())?;
            Ok(json!({ "query": query, "hits": page.hits, "has_more": page.has_more }))
        }
        "list_moments" => {
            let before = args.get("before_tid").and_then(Value::as_i64);
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            let posts = account
                .list_sns_posts(before, limit)
                .map_err(|error| error.to_string())?;
            Ok(json!({ "posts": posts }))
        }
        "get_cursor" => {
            let username = required_str(args, "username")?;
            let cursor = latest_cursor(&state.account, &username)?;
            Ok(json!({ "username": username, "cursor": cursor }))
        }
        "watch_messages" => {
            let username = required_str(args, "username")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 200) as usize;
            let timeout = args
                .get("timeout_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(25)
                .clamp(1, 60);
            let poll = args
                .get("poll_ms")
                .and_then(Value::as_u64)
                .unwrap_or(400)
                .clamp(100, 5000);
            let after = args
                .get("after")
                .filter(|value| !value.is_null())
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| format!("after cursor is invalid: {error}"))?;
            let mut cursor = match after {
                Some(cursor) => Some(cursor),
                None => load_saved_cursor(state, &username)
                    .or(latest_cursor(&state.account, &username)?),
            };
            if let Some(value) = cursor.as_ref() {
                save_cursor(state, &username, value);
            }
            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                let page = state
                    .account
                    .load_messages_after(&username, cursor.as_ref(), limit)
                    .map_err(|error| error.to_string())?;
                if !page.messages.is_empty() {
                    let next = page.next_after.clone();
                    if let Some(value) = next.as_ref() {
                        save_cursor(state, &username, value);
                    }
                    return Ok(json!({
                        "username": username,
                        "messages": messages_json(state, &username, &page.messages),
                        "cursor": next,
                        "has_more": page.has_more,
                        "timed_out": false,
                    }));
                }
                if Instant::now() >= deadline {
                    if let Some(value) = page.next_after.as_ref().or(cursor.as_ref()) {
                        save_cursor(state, &username, value);
                        cursor = Some(value.clone());
                    }
                    return Ok(json!({
                        "username": username,
                        "messages": [],
                        "cursor": cursor,
                        "has_more": false,
                        "timed_out": true,
                    }));
                }
                std::thread::sleep(Duration::from_millis(poll));
            }
        }
        "export_media" => {
            let username = required_str(args, "username")?;
            let md5 = required_str(args, "md5")?.to_ascii_lowercase();
            let kind = args
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("image")
                .to_ascii_lowercase();
            let local_id = args.get("local_id").and_then(Value::as_i64).unwrap_or(0);
            let create_time = args.get("create_time").and_then(Value::as_i64).unwrap_or(0);
            if let Some(path) = decrypt_media(state, &username, &kind, &md5, local_id, create_time)
            {
                return Ok(json!({ "path": path, "md5": md5, "kind": kind, "source": "local" }));
            }
            if kind == "emoji" {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                let aes_key = args.get("aes_key").and_then(Value::as_str).unwrap_or("");
                if !url.is_empty() {
                    let media = fetch_emoticon_media(
                        url,
                        aes_key,
                        state.image_keys.as_ref(),
                        16 * 1024 * 1024,
                    )
                    .map_err(|error| format!("fetch emoticon failed: {error}"))?;
                    if let Some(path) =
                        write_media_bytes(state, &md5, &media.data, &media.media_type)
                    {
                        return Ok(json!({
                            "path": path,
                            "md5": md5,
                            "kind": kind,
                            "source": "remote",
                        }));
                    }
                }
            }
            Err("media not decrypted: 本地无缓存、图片密钥不可用或类型不支持".to_string())
        }
        "send_text" => {
            let recipient = required_str(args, "recipient")?;
            let text = required_str(args, "text")?;
            action(state)?
                .send_text(&recipient, &text)
                .map_err(|error| error.to_string())
        }
        "send_image" => {
            let recipient = required_str(args, "recipient")?;
            let path = required_str(args, "path")?;
            action(state)?
                .send_image(&recipient, &path)
                .map_err(|error| error.to_string())
        }
        "send_file" => {
            let recipient = required_str(args, "recipient")?;
            let path = required_str(args, "path")?;
            action(state)?
                .send_file(&recipient, &path)
                .map_err(|error| error.to_string())
        }
        "send_emoticon" => {
            let recipient = required_str(args, "recipient")?;
            let path = required_str(args, "path")?;
            action(state)?
                .send_emoticon(&recipient, &path)
                .map_err(|error| error.to_string())
        }
        "send_voice" => {
            let recipient = required_str(args, "recipient")?;
            let path = required_str(args, "path")?;
            let duration = args
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(3000) as u32;
            action(state)?
                .send_voice(&recipient, &path, duration)
                .map_err(|error| error.to_string())
        }
        "send_at_text" => {
            let recipient = required_str(args, "recipient")?;
            let text = required_str(args, "text")?;
            let members = required_str(args, "members")?;
            action(state)?
                .send_at_text(&recipient, &text, &members)
                .map_err(|error| error.to_string())
        }
        "moments_like" => {
            let feed = required_u64(args, "feed_id")?;
            let user = required_str(args, "user")?;
            action(state)?
                .like_moments(feed, &user)
                .map_err(|error| error.to_string())
        }
        "moments_publish" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            let visibility = args.get("visibility").and_then(Value::as_u64).unwrap_or(1) as u32;
            let images = args
                .get("images")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            action(state)?
                .publish_moments(text, &images, visibility)
                .map_err(|error| error.to_string())
        }
        "moments_comment_image" => {
            let feed = required_u64(args, "feed_id")?;
            let user = required_str(args, "user")?;
            let image = required_str(args, "image")?;
            action(state)?
                .comment_image_moments(feed, &user, &image)
                .map_err(|error| error.to_string())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn action(state: &McpState) -> Result<ActionClient, String> {
    if !state.account.native_actions() {
        return Err("native actions are not configured (WCE_NATIVE_BRIDGE)".to_string());
    }
    Ok(state.account.action_client())
}

fn latest_cursor(
    account: &RealtimeAccountStore,
    username: &str,
) -> Result<Option<MessageCursor>, String> {
    let page = account
        .load_messages(
            username,
            &MessageQuery {
                limit: 1,
                before: None,
            },
        )
        .map_err(|error| error.to_string())?;
    Ok(page.messages.last().map(|message| MessageCursor {
        create_time: message.create_time,
        sort_seq: message.sort_seq,
        local_id: message.local_id,
        shard: message.shard.clone(),
    }))
}

fn cursor_file(state: &McpState, username: &str) -> PathBuf {
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
    state.cursor_dir.join(format!("{safe}.json"))
}

fn load_saved_cursor(state: &McpState, username: &str) -> Option<MessageCursor> {
    let text = std::fs::read_to_string(cursor_file(state, username)).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_cursor(state: &McpState, username: &str, cursor: &MessageCursor) {
    if std::fs::create_dir_all(&state.cursor_dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string(cursor) {
        let _ = std::fs::write(cursor_file(state, username), text);
    }
}

fn messages_json(state: &McpState, username: &str, messages: &[Message]) -> Vec<Value> {
    let mut budget = 32usize;
    messages
        .iter()
        .map(|message| {
            let mut value = serde_json::to_value(message).unwrap_or(Value::Null);
            if let Some(media) = media_info(state, username, message, &mut budget) {
                value["media"] = media;
            }
            value
        })
        .collect()
}

fn export_emoji_remote(state: &McpState, content: &str, md5: &str) -> Option<String> {
    let cache_key = format!("remote:{md5}");
    if let Ok(guard) = state.decrypted.lock() {
        if let Some(value) = guard.get(&cache_key) {
            return value.clone();
        }
    }
    let reference = parse_emoji_reference(content)?;
    let (url, aes_key) = reference.best_url();
    if url.is_empty() {
        return None;
    }
    let media =
        fetch_emoticon_media(url, aes_key, state.image_keys.as_ref(), 16 * 1024 * 1024).ok()?;
    let value = write_media_bytes(state, md5, &media.data, &media.media_type);
    if let Ok(mut guard) = state.decrypted.lock() {
        guard.insert(cache_key, value.clone());
    }
    value
}

fn decrypt_media(
    state: &McpState,
    username: &str,
    kind: &str,
    md5: &str,
    local_id: i64,
    create_time: i64,
) -> Option<String> {
    if !state.decrypt || md5.len() != 32 {
        return None;
    }
    let cache_key = format!("decrypt:{kind}:{md5}");
    if let Ok(guard) = state.decrypted.lock() {
        if let Some(value) = guard.get(&cache_key) {
            return value.clone();
        }
    }
    let root = state.account.account_root()?;
    let media = match kind {
        "emoji" => state
            .image_keys
            .as_ref()
            .and_then(|keys| load_emoji_image(root, md5, keys)),
        _ => match state.image_keys.as_ref() {
            Some(keys) => load_message_image(root, username, md5, local_id, create_time, keys),
            None => load_message_image_cached(root, username, md5, local_id, create_time),
        },
    };
    let value =
        media.and_then(|image| write_media_bytes(state, md5, &image.data, &image.media_type));
    if let Ok(mut guard) = state.decrypted.lock() {
        guard.insert(cache_key, value.clone());
    }
    value
}

fn media_kind(message: &Message) -> Option<&'static str> {
    match message.kind {
        MessageKind::Image => Some("image"),
        MessageKind::Video => Some("video"),
        MessageKind::Emoji => Some("emoji"),
        MessageKind::File => Some("file"),
        MessageKind::Voice => Some("voice"),
        MessageKind::App => match &message.body {
            Some(MessageBody::File { .. }) => Some("file"),
            _ => None,
        },
        _ => None,
    }
}

fn media_md5(message: &Message) -> String {
    if let Some(value) = message.media_md5.as_ref().filter(|value| !value.is_empty()) {
        return value.to_ascii_lowercase();
    }
    if let Some(MessageBody::File { file_md5, .. }) = &message.body {
        if !file_md5.is_empty() {
            return file_md5.to_ascii_lowercase();
        }
    }
    if let Some(value) = extract_image_md5(&message.content) {
        return value;
    }
    if let Some(emoji) = parse_emoji_reference(&message.content) {
        if !emoji.md5.is_empty() {
            return emoji.md5;
        }
    }
    String::new()
}

fn media_info(
    state: &McpState,
    username: &str,
    message: &Message,
    budget: &mut usize,
) -> Option<Value> {
    let kind = media_kind(message)?;
    let md5 = media_md5(message);
    let root = state.account.account_root();
    let mut roots = Vec::new();
    if let Some(root) = root {
        for relative in ["msg/attach", "msg/file", "msg/video", "cache", "resource"] {
            let path = root.join(relative);
            if path.is_dir() {
                roots.push(display_path(&path));
            }
        }
    }
    let mut path: Option<String> = None;
    let mut thumbnail = false;
    if !md5.is_empty() {
        let mut guard = state.media.lock().ok()?;
        if guard.is_none() {
            if let Some(root) = root {
                *guard = Some(MediaIndex::build(root));
            }
        }
        if let Some(index) = guard.as_ref() {
            let primary = index.resolve(kind, &md5).map(Path::to_path_buf);
            let fallback = match kind {
                "image" => index.resolve("video_thumb", &md5).map(Path::to_path_buf),
                "video" => index.resolve("video_thumb", &md5).map(Path::to_path_buf),
                "emoji" => index.resolve("image", &md5).map(Path::to_path_buf),
                _ => None,
            };
            if let Some(value) = primary.or(fallback) {
                thumbnail = kind == "video" && index.resolve("video", &md5).is_none();
                path = Some(display_path(&value));
            }
        }
    }
    if path.is_none() && kind == "file" {
        if let Some(title) = message
            .body
            .as_ref()
            .and_then(|body| match body {
                MessageBody::File { title, .. } => Some(title.clone()),
                _ => None,
            })
            .filter(|title| !title.trim().is_empty())
        {
            path = find_file_by_name(state, root, &title);
        }
    }
    let source_path = path.clone();
    let mut decrypted = false;
    if matches!(kind, "image" | "emoji") && !md5.is_empty() {
        if let Some(value) = decrypt_media(
            state,
            username,
            kind,
            &md5,
            message.local_id,
            message.create_time,
        ) {
            path = Some(value);
            decrypted = true;
        } else if kind == "emoji" && state.remote_emoji && *budget > 0 {
            if let Some(value) = export_emoji_remote(state, &message.content, &md5) {
                path = Some(value);
                decrypted = true;
                *budget -= 1;
            }
        }
    }
    let encrypted = source_path
        .as_deref()
        .map(|value| value.to_ascii_lowercase().ends_with(".dat"))
        .unwrap_or(false);
    let filename = message
        .body
        .as_ref()
        .and_then(|body| match body {
            MessageBody::File { title, .. } => Some(title.clone()),
            _ => None,
        })
        .filter(|title| !title.trim().is_empty());
    Some(json!({
        "kind": kind,
        "md5": md5,
        "path": path,
        "sourcePath": source_path,
        "decrypted": decrypted,
        "thumbnail": thumbnail,
        "encrypted": encrypted,
        "filename": filename,
        "roots": roots,
    }))
}

fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy().to_string();
    if let Some(rest) = value.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{rest}");
    }
    value
        .strip_prefix("\\\\?\\")
        .map(str::to_string)
        .unwrap_or(value)
}

fn find_file_by_name(state: &McpState, root: Option<&Path>, title: &str) -> Option<String> {
    let name = Path::new(title).file_name()?.to_string_lossy().to_string();
    let mut guard = state.files.lock().ok()?;
    const INDEX: &str = "__wce_file_index__";
    if !guard.contains_key(INDEX) {
        let mut index: HashMap<String, String> = HashMap::new();
        if let Some(directory) = root
            .map(|root| root.join("msg").join("file"))
            .filter(|path| path.is_dir())
        {
            let mut stack = vec![(directory, 0usize)];
            let mut visited = 0usize;
            while let Some((path, depth)) = stack.pop() {
                if depth > 5 || visited > 20000 {
                    break;
                }
                let Ok(entries) = std::fs::read_dir(&path) else {
                    continue;
                };
                for entry in entries.flatten() {
                    visited += 1;
                    let Ok(file_type) = entry.file_type() else {
                        continue;
                    };
                    if file_type.is_dir() {
                        stack.push((entry.path(), depth + 1));
                    } else {
                        index
                            .entry(entry.file_name().to_string_lossy().to_string())
                            .or_insert_with(|| display_path(&entry.path()));
                    }
                }
            }
        }
        for (file, path) in index {
            guard.entry(file).or_insert(Some(path));
        }
        guard.insert(INDEX.to_string(), None);
    }
    guard.get(&name).cloned().flatten()
}

fn media_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        _ => "bin",
    }
}

fn write_media_bytes(state: &McpState, md5: &str, data: &[u8], media_type: &str) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    let key = if md5.len() == 32 {
        md5.to_string()
    } else {
        format!("remote-{}", std::process::id())
    };
    let directory = state.media_dir.join(&key[..2]);
    std::fs::create_dir_all(&directory).ok()?;
    let path = directory.join(format!("{key}.{}", media_extension(media_type)));
    if !path.is_file() {
        std::fs::write(&path, data).ok()?;
    }
    Some(path.to_string_lossy().to_string())
}

fn required_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing argument: {key}"))
}

fn required_u64(args: &Value, key: &str) -> Result<u64, String> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing argument: {key}"))
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool("wechat_status", "当前账号、目录和原生动作状态", obj(vec![])),
        tool(
            "list_sessions",
            "列出会话",
            obj(vec![
                ("include_hidden", boolean("包含隐藏会话")),
                ("include_official", boolean("包含公众号")),
            ]),
        ),
        tool(
            "load_messages",
            "按会话读取聊天消息",
            obj_with_required(
                vec![
                    ("username", string("会话 wxid 或 @chatroom")),
                    ("limit", integer("最多条数，默认 50")),
                    (
                        "before",
                        json!({ "type": "object", "description": "上一页的 next_before 游标" }),
                    ),
                ],
                vec!["username"],
            ),
        ),
        tool(
            "get_cursor",
            "获取会话当前最新消息游标，用于 watch_messages",
            obj_with_required(
                vec![("username", string("会话 wxid 或 @chatroom"))],
                vec!["username"],
            ),
        ),
        tool(
            "watch_messages",
            "长轮询等待会话的新消息（默认从现在开始等 25 秒）",
            obj_with_required(
                vec![
                    ("username", string("会话 wxid 或 @chatroom")),
                    (
                        "after",
                        json!({ "type": "object", "description": "上次返回的 cursor；省略则从现在开始等" }),
                    ),
                    ("timeout_seconds", integer("最长等待秒数，默认 25，最大 60")),
                    ("poll_ms", integer("轮询间隔毫秒，默认 400")),
                    ("limit", integer("单次最多返回条数，默认 50")),
                ],
                vec!["username"],
            ),
        ),
        tool(
            "list_contacts",
            "读取联系人",
            obj(vec![
                ("keyword", string("可选关键词")),
                ("limit", integer("最多条数，默认 200")),
            ]),
        ),
        tool(
            "search_messages",
            "全文搜索聊天记录",
            obj_with_required(
                vec![
                    ("query", string("搜索关键词")),
                    ("limit", integer("最多条数，默认 50")),
                ],
                vec!["query"],
            ),
        ),
        tool(
            "list_moments",
            "读取朋友圈",
            obj(vec![
                ("before_tid", integer("上一页的 tid")),
                ("limit", integer("最多条数，默认 20")),
            ]),
        ),
        tool(
            "export_media",
            "把图片/表情解密到本地并返回路径",
            obj_with_required(
                vec![
                    ("username", string("会话 wxid 或 @chatroom")),
                    ("md5", string("消息 media.md5")),
                    ("kind", string("image 或 emoji，默认 image")),
                    ("local_id", integer("消息 local_id，可选")),
                    ("create_time", integer("消息 create_time，可选")),
                    ("url", string("表情 CDN 地址，本地未缓存时使用")),
                    ("aes_key", string("表情 CDN aes_key")),
                ],
                vec!["username", "md5"],
            ),
        ),
        tool(
            "send_text",
            "发送文本消息",
            obj_with_required(
                vec![
                    ("recipient", string("wxid、群 id 或 filehelper")),
                    ("text", string("正文")),
                ],
                vec!["recipient", "text"],
            ),
        ),
        tool(
            "send_image",
            "发送图片",
            obj_with_required(
                vec![
                    ("recipient", string("接收方")),
                    ("path", string("绝对图片路径")),
                ],
                vec!["recipient", "path"],
            ),
        ),
        tool(
            "send_file",
            "发送文件",
            obj_with_required(
                vec![
                    ("recipient", string("接收方")),
                    ("path", string("绝对文件路径")),
                ],
                vec!["recipient", "path"],
            ),
        ),
        tool(
            "send_emoticon",
            "发送表情图片",
            obj_with_required(
                vec![
                    ("recipient", string("接收方")),
                    ("path", string("绝对图片路径")),
                ],
                vec!["recipient", "path"],
            ),
        ),
        tool(
            "send_voice",
            "发送语音",
            obj_with_required(
                vec![
                    ("recipient", string("接收方")),
                    ("path", string("绝对音频路径")),
                    ("duration_ms", integer("时长毫秒，默认 3000")),
                ],
                vec!["recipient", "path"],
            ),
        ),
        tool(
            "send_at_text",
            "群聊发送 @ 消息",
            obj_with_required(
                vec![
                    ("recipient", string("群 id")),
                    ("text", string("正文")),
                    ("members", string("逗号分隔的 wxid")),
                ],
                vec!["recipient", "text", "members"],
            ),
        ),
        tool(
            "moments_like",
            "朋友圈点赞",
            obj_with_required(
                vec![
                    ("feed_id", integer("朋友圈 tid")),
                    ("user", string("发布者 wxid")),
                ],
                vec!["feed_id", "user"],
            ),
        ),
        tool(
            "moments_publish",
            "发布朋友圈",
            obj(vec![
                ("text", string("正文")),
                (
                    "images",
                    json!({ "type": "array", "items": { "type": "string" }, "description": "绝对图片路径" }),
                ),
                ("visibility", integer("1 公开，2 仅自己")),
            ]),
        ),
        tool(
            "moments_comment_image",
            "朋友圈图片评论",
            obj_with_required(
                vec![
                    ("feed_id", integer("朋友圈 tid")),
                    ("user", string("发布者 wxid")),
                    ("image", string("绝对图片路径")),
                ],
                vec!["feed_id", "user", "image"],
            ),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

fn obj(properties: Vec<(&str, Value)>) -> Value {
    obj_with_required(properties, vec![])
}

fn obj_with_required(properties: Vec<(&str, Value)>, required: Vec<&str>) -> Value {
    let properties = properties
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<serde_json::Map<String, Value>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn integer(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

fn boolean(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

struct Config {
    root: PathBuf,
    broker: PathBuf,
    bridge: Option<PathBuf>,
    work_dir: Option<PathBuf>,
    key: String,
    raw_key: bool,
    account: Option<String>,
    key_store: Option<PathBuf>,
    media_dir: PathBuf,
    decrypt: bool,
    remote_emoji: bool,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let root = std::env::var_os("WECHAT_REALTIME_ROOT")
            .map(PathBuf::from)
            .ok_or("WECHAT_REALTIME_ROOT is required")?;
        let broker = std::env::var_os("WCE_SELF_BROKER")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe().ok().and_then(|exe| {
                    let dir = exe.parent()?;
                    let agent = dir.join("wechat-agent.exe");
                    if agent.is_file() {
                        return Some(agent);
                    }
                    let broker = dir.join("wce-self-broker.exe");
                    broker.is_file().then_some(broker)
                })
            })
            .ok_or("WCE_SELF_BROKER is required")?;
        let bridge = std::env::var_os("WCE_NATIVE_BRIDGE")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .or_else(|| {
                broker
                    .parent()
                    .map(|dir| dir.join("text_bridge.dll"))
                    .filter(|path| path.is_file())
            });
        let work_dir = std::env::var_os("WCE_NATIVE_WORK_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::temp_dir().join("wce-mcp-actions").into());
        let account = std::env::var("WCE_MCP_ACCOUNT")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let key_store = std::env::var_os("WECHAT_KEY_STORE").map(PathBuf::from);
        let media_dir = std::env::var_os("WCE_MEDIA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("wechat-mcp-media"));
        let decrypt = std::env::var("WCE_MCP_DECRYPT").ok().as_deref() != Some("0");
        let remote_emoji = std::env::var("WCE_MCP_REMOTE_EMOJI").ok().as_deref() != Some("0");
        let (key, raw_key) = if let Ok(value) = std::env::var("WCE_DB_KEY") {
            if value.trim().is_empty() {
                return Err("WCE_DB_KEY is empty".to_string());
            }
            (
                value.trim().to_string(),
                std::env::var("WCE_DB_RAW").ok().as_deref() == Some("1"),
            )
        } else if let Ok(store) = std::env::var("WECHAT_KEY_STORE") {
            wechat_data::find_key_for_db_storage_with_account(Path::new(&store), &root)
                .map_err(|error| format!("read key store failed: {error}"))
                .map(|(key, raw, _)| (key, raw))?
        } else {
            return Err("WCE_DB_KEY or WECHAT_KEY_STORE is required".to_string());
        };
        Ok(Self {
            root,
            broker,
            bridge,
            work_dir,
            key,
            raw_key,
            account,
            key_store,
            media_dir,
            decrypt,
            remote_emoji,
        })
    }
}
