use std::collections::HashMap;
use std::path::{Path, PathBuf};

use wechat_data::{
    extract_image_md5, fetch_emoticon_media, find_image_keys_for_db_storage, load_emoji_image,
    load_message_image, load_message_image_cached, parse_emoji_reference, parse_message_body,
    ImageKeys, MediaIndex, Message, MessageBody, MessageKind,
};

pub struct MediaContext {
    root: PathBuf,
    out_dir: PathBuf,
    keys: Option<ImageKeys>,
    index: Option<MediaIndex>,
    saved: HashMap<String, Option<String>>,
    remote_budget: usize,
}

impl MediaContext {
    pub fn new(
        account_root: Option<PathBuf>,
        db_storage: &Path,
        key_store: &Path,
        out_dir: PathBuf,
    ) -> Self {
        let empty = PathBuf::new();
        let root = account_root.unwrap_or(empty);
        let keys = find_image_keys_for_db_storage(key_store, db_storage)
            .ok()
            .flatten();
        Self {
            root,
            out_dir,
            keys,
            index: None,
            saved: HashMap::new(),
            remote_budget: 8,
        }
    }

    pub fn annotate(&mut self, chat: &str, message: &Message) -> Option<String> {
        if self.root.as_os_str().is_empty() {
            return None;
        }
        match message.kind {
            MessageKind::Image => self.annotate_image(chat, message),
            MessageKind::Emoji => self.annotate_emoji(message),
            MessageKind::File => self.annotate_file(message),
            MessageKind::Video => self.annotate_video(message),
            MessageKind::Voice => self.annotate_voice(message),
            _ => None,
        }
    }

    fn md5_of(&self, message: &Message) -> String {
        if let Some(value) = message.media_md5.as_ref().filter(|value| !value.is_empty()) {
            return value.to_ascii_lowercase();
        }
        if let Some(value) = extract_image_md5(&message.content) {
            return value.to_ascii_lowercase();
        }
        if let Some(reference) = parse_emoji_reference(&message.content) {
            if !reference.md5.is_empty() {
                return reference.md5.to_ascii_lowercase();
            }
        }
        String::new()
    }

    fn save(&mut self, md5: &str, data: &[u8], media_type: &str) -> Option<String> {
        if md5.len() != 32 {
            return None;
        }
        if let Some(value) = self.saved.get(md5) {
            return value.clone();
        }
        let extension = match media_type {
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "jpg",
        };
        let directory = self.out_dir.join(&md5[..2]);
        let path = directory.join(format!("{md5}.{extension}"));
        let value = if path.is_file() {
            Some(display(&path))
        } else {
            std::fs::create_dir_all(&directory).ok()?;
            std::fs::write(&path, data).ok()?;
            Some(display(&path))
        };
        self.saved.insert(md5.to_string(), value.clone());
        value
    }

    fn annotate_image(&mut self, chat: &str, message: &Message) -> Option<String> {
        let md5 = self.md5_of(message);
        if md5.is_empty() {
            return Some("[图片] 未找到 md5".to_string());
        }
        let image = match self.keys.as_ref() {
            Some(keys) => load_message_image(
                &self.root,
                chat,
                &md5,
                message.local_id,
                message.create_time,
                keys,
            ),
            None => load_message_image_cached(&self.root, chat, &md5, message.local_id, message.create_time),
        };
        match image.and_then(|image| self.save(&md5, &image.data, &image.media_type)) {
            Some(path) => Some(format!("[图片] md5={md5} 本地={path}")),
            None => Some(format!(
                "[图片] md5={md5} 未解密（可先请求微信客户端打开该图，或 export_media）"
            )),
        }
    }

    fn annotate_emoji(&mut self, message: &Message) -> Option<String> {
        let reference = parse_emoji_reference(&message.content);
        let md5 = self.md5_of(message);
        if !md5.is_empty() {
            if let Some(keys) = self.keys.as_ref() {
                if let Some(image) = load_emoji_image(&self.root, &md5, keys) {
                    if let Some(path) = self.save(&md5, &image.data, &image.media_type) {
                        return Some(format!("[表情] md5={md5} 本地={path}"));
                    }
                }
            } else if let Some(path) = find_resource(&self.root, &md5) {
                return Some(format!("[表情] md5={md5} 本地={path}"));
            }
        }
        if let Some(reference) = reference.as_ref() {
            let (url, aes_key) = reference.best_url();
            if self.remote_budget > 0 && !url.is_empty() {
                self.remote_budget -= 1;
                let keys = self.keys.as_ref();
                if let Ok(media) = fetch_emoticon_media(url, aes_key, keys, 16 * 1024 * 1024) {
                    if let Some(path) = self.save(&md5, &media.data, &media.media_type) {
                        return Some(format!("[表情] md5={md5} 本地={path}（CDN 解密）"));
                    }
                }
            }
        }
        Some(format!("[表情] md5={md5} 未缓存"))
    }

    fn annotate_file(&mut self, message: &Message) -> Option<String> {
        let body = message
            .body
            .clone()
            .or_else(|| parse_message_body(message.local_type, &message.content));
        let (title, size, file_md5) = match body {
            Some(MessageBody::File {
                title,
                size,
                file_md5,
                ..
            }) => (title, size, file_md5),
            _ => (String::new(), String::new(), self.md5_of(message)),
        };
        let mut path = None;
        if !file_md5.is_empty() {
            let index = self.index.get_or_insert_with(|| MediaIndex::build(&self.root));
            path = index.resolve("file", &file_md5).map(display);
        }
        if path.is_none() && !title.is_empty() {
            path = find_file(&self.root, &title);
        }
        let detail = match (title.is_empty(), size.is_empty()) {
            (false, false) => format!("{title} ({size})"),
            (false, true) => title,
            (true, false) => size,
            (true, true) => "未知文件".to_string(),
        };
        Some(match path {
            Some(path) => format!("[文件] {detail} 本地={path}"),
            None => format!("[文件] {detail} 未找到本地文件"),
        })
    }

    fn annotate_video(&mut self, message: &Message) -> Option<String> {
        let md5 = self.md5_of(message);
        if md5.is_empty() {
            return Some("[视频] 未找到 md5".to_string());
        }
        let index = self.index.get_or_insert_with(|| MediaIndex::build(&self.root));
        let path = index.resolve("video", &md5).map(display);
        let thumb = index.resolve("video_thumb", &md5).map(display);
        match (path, thumb) {
            (Some(path), _) => Some(format!("[视频] md5={md5} 本地={path}")),
            (None, Some(path)) => Some(format!("[视频] md5={md5} 仅缩略图={path}")),
            (None, None) => Some(format!("[视频] md5={md5} 未找到本地文件")),
        }
    }

    fn annotate_voice(&mut self, message: &Message) -> Option<String> {
        let md5 = self.md5_of(message);
        if md5.is_empty() {
            None
        } else {
            Some(format!("[语音] md5={md5} 无本地文件"))
        }
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn find_resource(root: &Path, md5: &str) -> Option<String> {
    if md5.len() != 32 {
        return None;
    }
    let directory = root.join("resource").join(&md5[..2]);
    let entries = std::fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.starts_with(md5) {
            return Some(display(&entry.path()));
        }
    }
    None
}

fn find_file(root: &Path, title: &str) -> Option<String> {
    let target = title.trim();
    if target.is_empty() {
        return None;
    }
    let mut stack = vec![root.join("msg").join("file")];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if stack.len() < 64 {
                    stack.push(path);
                }
                continue;
            }
            if path
                .file_name()
                .map(|name| name.to_string_lossy() == target)
                .unwrap_or(false)
            {
                return Some(display(&path));
            }
        }
    }
    None
}
