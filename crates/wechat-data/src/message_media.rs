use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use md5::{Digest, Md5};
use regex::Regex;
use wechat_core::media::{decrypt_media_bytes, MediaKeys};

use crate::key_store::ImageKeys;

static MEDIA_ROOT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

pub fn set_media_root(path: Option<PathBuf>) {
    let lock = MEDIA_ROOT.get_or_init(|| RwLock::new(None));
    if let Ok(mut guard) = lock.write() {
        *guard = path;
    }
}

fn media_root_override() -> Option<PathBuf> {
    MEDIA_ROOT
        .get()
        .and_then(|lock| lock.read().ok().and_then(|guard| guard.clone()))
}

pub fn cache_media_bytes(md5: &str, data: &[u8], media_type: &str) -> Option<PathBuf> {
    let md5 = md5.trim().to_ascii_lowercase();
    if md5.len() != 32 || data.is_empty() {
        return None;
    }
    let ext = match media_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        _ => return None,
    };
    let root = media_root_override()?.join("resource").join(&md5[..2]);
    std::fs::create_dir_all(&root).ok()?;
    let path = root.join(format!("{md5}.{ext}"));
    if !path.is_file() {
        std::fs::write(&path, data).ok()?;
    }
    Some(path)
}

#[derive(Debug, Clone)]
pub struct MessageImage {
    pub data: Vec<u8>,
    pub media_type: String,
}

pub fn extract_image_md5(content: &str) -> Option<String> {
    if content.trim().is_empty() {
        return None;
    }
    for attr in ["md5", "cdnthumbmd5", "thumbmd5", "rawmd5"] {
        let pattern = format!(r#"(?i)\b{}\s*=\s*["']([0-9a-f]{{32}})["']"#, attr);
        let Ok(regex) = Regex::new(&pattern) else {
            continue;
        };
        if let Some(value) = regex.captures(content).and_then(|c| c.get(1)) {
            return Some(value.as_str().to_ascii_lowercase());
        }
    }
    md5_from_imagesuffix(content)
}

fn md5_from_imagesuffix(content: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(r"(?i)\b([0-9a-f]{32})(?:_t|_h|_b)?\.dat\b").expect("md5 dat regex")
    });
    pattern
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|value| value.as_str().to_ascii_lowercase())
}

pub fn load_message_image(
    account_root: &Path,
    chat_username: &str,
    image_md5: &str,
    local_id: i64,
    create_time: i64,
    keys: &ImageKeys,
) -> Option<MessageImage> {
    if let Some(image) = find_cache_thumb(account_root, chat_username, local_id, create_time) {
        return Some(image);
    }

    let image_md5 = image_md5.trim().to_ascii_lowercase();
    if image_md5.len() != 32 || !image_md5.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if let Some(image) = find_video_thumb(account_root, &image_md5) {
        return Some(image);
    }
    if let Some(image) = find_resource_image(account_root, &image_md5) {
        return Some(image);
    }

    let attach = account_root.join("msg").join("attach");
    let chat_dir = attach.join(hex_md5(chat_username));
    let path = find_attach_image(&chat_dir, &image_md5)?;
    decrypt_dat(&path, keys)
}

pub fn load_message_image_cached(
    account_root: &Path,
    chat_username: &str,
    image_md5: &str,
    local_id: i64,
    create_time: i64,
) -> Option<MessageImage> {
    if let Some(image) = find_cache_thumb(account_root, chat_username, local_id, create_time) {
        return Some(image);
    }
    let md5 = image_md5.trim().to_ascii_lowercase();
    if md5.len() == 32 && md5.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Some(image) = find_video_thumb(account_root, &md5) {
            return Some(image);
        }
        if let Some(image) = find_resource_image(account_root, &md5) {
            return Some(image);
        }
    }
    None
}

pub fn load_message_image_original(
    account_root: &Path,
    chat_username: &str,
    image_md5: &str,
    local_id: i64,
    create_time: i64,
    keys: &ImageKeys,
) -> Option<MessageImage> {
    let image_md5 = image_md5.trim().to_ascii_lowercase();
    if image_md5.len() == 32 && image_md5.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Some(image) = find_resource_image(account_root, &image_md5) {
            return Some(image);
        }
        let attach = account_root.join("msg").join("attach");
        let chat_dir = attach.join(hex_md5(chat_username));
        if let Some(path) = find_attach_image_original(&chat_dir, &image_md5) {
            if let Some(image) = read_plain_image(&path) {
                return Some(image);
            }
            if let Some(image) = decrypt_dat(&path, keys) {
                return Some(image);
            }
        }
    }
    load_message_image(
        account_root,
        chat_username,
        &image_md5,
        local_id,
        create_time,
        keys,
    )
}

pub fn find_resource_image(account_root: &Path, md5: &str) -> Option<MessageImage> {
    if md5.len() != 32 {
        return None;
    }
    let mut roots = vec![account_root.join("resource")];
    if let Some(extra) =
        media_root_override().or_else(|| std::env::var_os("WCE_MEDIA_ROOT").map(PathBuf::from))
    {
        roots.push(extra.join("resource"));
        roots.push(extra);
    }
    for root in roots {
        let dir = root.join(&md5[..2]);
        for ext in ["jpg", "jpeg", "png", "gif", "webp", "bmp"] {
            for candidate in [
                dir.join(format!("{md5}.{ext}")),
                root.join(format!("{md5}.{ext}")),
            ] {
                if candidate.is_file() {
                    if let Some(image) = read_plain_image(&candidate) {
                        return Some(image);
                    }
                }
            }
        }
    }
    None
}

pub fn load_emoji_image(account_root: &Path, md5: &str, keys: &ImageKeys) -> Option<MessageImage> {
    let md5 = md5.trim().to_ascii_lowercase();
    if md5.len() != 32 || !md5.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if let Some(image) = find_resource_image(account_root, &md5) {
        return Some(image);
    }
    let prefix = &md5[..2];
    let cache = account_root.join("cache");
    let mut months: Vec<PathBuf> = std::fs::read_dir(&cache)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    months.sort();
    months.reverse();
    for month in months {
        for bucket in ["Emoticon", "emoticon", "Emoji", "emoji"] {
            let dir = month.join(bucket).join(prefix);
            if !dir.is_dir() {
                continue;
            }
            if let Some(image) = load_emoji_file(&dir, &md5, keys) {
                return Some(image);
            }
        }
    }
    for dir in [
        account_root.join("msg").join("emoji"),
        account_root.join("msg").join("emoticon"),
    ] {
        if dir.is_dir() {
            if let Some(image) = load_emoji_file(&dir, &md5, keys) {
                return Some(image);
            }
        }
    }
    None
}

fn load_emoji_file(dir: &Path, md5: &str, keys: &ImageKeys) -> Option<MessageImage> {
    for suffix in ["", ".dat", ".gif", ".png", ".webp", ".jpg", "_h", "_t"] {
        let path = dir.join(format!("{md5}{suffix}"));
        if !path.is_file() {
            continue;
        }
        if let Some(image) = read_plain_image(&path) {
            return Some(image);
        }
        if let Some(image) = decrypt_dat(&path, keys) {
            return Some(image);
        }
    }
    None
}

pub fn load_sns_image(
    account_root: &Path,
    tid: i64,
    media_id: &str,
    media_type: i64,
    media_md5: &str,
    keys: &ImageKeys,
) -> Option<MessageImage> {
    let media_id = media_id.trim();
    if media_id.is_empty() {
        return None;
    }
    let mut months = sns_cache_months(account_root);
    months.reverse();
    let tid_values = [tid.to_string(), (tid as u64).to_string()];
    for month in &months {
        for tid_value in &tid_values {
            for kind in [media_type, 2, 4, 1, 3] {
                let cache_key = hex_md5(&format!("{tid_value}_{media_id}_{kind}"));
                if let Some(image) = decrypt_sns_cache(month, &cache_key, keys) {
                    return Some(image);
                }
            }
        }
        let md5 = media_md5.trim().to_ascii_lowercase();
        if md5.len() == 32 && md5.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Some(image) = decrypt_sns_cache(month, &md5, keys) {
                return Some(image);
            }
        }
    }
    let md5 = media_md5.trim().to_ascii_lowercase();
    if md5.len() == 32 && md5.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Some(image) = find_resource_image(account_root, &md5) {
            return Some(image);
        }
    }
    None
}

fn sns_cache_months(account_root: &Path) -> Vec<PathBuf> {
    let cache = account_root.join("cache");
    let Ok(entries) = std::fs::read_dir(cache) else {
        return Vec::new();
    };
    let mut months: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("Sns").is_dir())
        .collect();
    months.sort();
    months
}

fn decrypt_sns_cache(month: &Path, cache_key: &str, keys: &ImageKeys) -> Option<MessageImage> {
    if cache_key.len() != 32 {
        return None;
    }
    let path = month
        .join("Sns")
        .join("Img")
        .join(&cache_key[..2])
        .join(&cache_key[2..]);
    if !path.is_file() {
        return None;
    }
    decrypt_dat(&path, keys)
}

fn decrypt_dat(path: &Path, keys: &ImageKeys) -> Option<MessageImage> {
    let data = std::fs::read(path).ok()?;
    let aes_key = keys.aes_key.as_bytes().to_vec();
    let result = decrypt_media_bytes(
        &data,
        &MediaKeys {
            xor_key: keys.xor_key,
            aes_key: Some(aes_key),
        },
        None,
        None,
    );
    if result.data.is_empty() || !result.media_type.starts_with("image/") {
        return None;
    }
    Some(MessageImage {
        data: result.data,
        media_type: result.media_type,
    })
}

fn find_video_thumb(account_root: &Path, md5: &str) -> Option<MessageImage> {
    let root = account_root.join("msg").join("video");
    let entries = std::fs::read_dir(root).ok()?;
    let mut months: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    months.sort();
    months.reverse();
    for month in months {
        for suffix in ["_thumb.jpg", ".jpg", "_thumb.png", ".png"] {
            let path = month.join(format!("{md5}{suffix}"));
            if path.is_file() {
                if let Some(image) = read_plain_image(&path) {
                    return Some(image);
                }
            }
        }
    }
    None
}

fn find_cache_thumb(
    account_root: &Path,
    chat_username: &str,
    local_id: i64,
    create_time: i64,
) -> Option<MessageImage> {
    if local_id <= 0 || create_time <= 0 {
        return None;
    }
    let cache = account_root.join("cache");
    let chat_hash = hex_md5(chat_username);
    let file_id = format!("{local_id}_{create_time}");
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&cache) {
        let mut periods: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        periods.sort();
        periods.reverse();
        for period in periods {
            for message_dir in ["Message", "message"] {
                roots.push(period.join(message_dir).join(&chat_hash));
            }
        }
    }
    for message_dir in ["Message", "message"] {
        roots.push(cache.join(message_dir).join(&chat_hash));
    }
    for root in roots {
        for subdir in ["Thumb", "thumb", "ImageTemp", "Image", "Img"] {
            let dir = root.join(subdir);
            for suffix in [
                "_thumb.jpg",
                "_thumb.jpeg",
                "_thumb.png",
                "_thumb.webp",
                "_t.dat",
                "_h.dat",
                ".dat",
                ".jpg",
                ".png",
            ] {
                let path = dir.join(format!("{file_id}{suffix}"));
                if !path.is_file() {
                    continue;
                }
                if let Some(image) = read_plain_image(&path) {
                    return Some(image);
                }
            }
        }
    }
    None
}

fn read_plain_image(path: &Path) -> Option<MessageImage> {
    let data = std::fs::read(path).ok()?;
    let media_type = wechat_core::media::detect_image_media_type(&data);
    if media_type == "application/octet-stream" {
        return None;
    }
    Some(MessageImage {
        data,
        media_type: media_type.to_string(),
    })
}

fn hex_md5(value: &str) -> String {
    hex::encode(Md5::digest(value.as_bytes()))
}

fn find_attach_image(chat_dir: &Path, image_md5: &str) -> Option<PathBuf> {
    let mut candidates = attach_candidates(chat_dir, image_md5);
    candidates.sort_by_key(|path| match attachment_rank(path) {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 3,
    });
    candidates.into_iter().next()
}

fn find_attach_image_original(chat_dir: &Path, image_md5: &str) -> Option<PathBuf> {
    let mut candidates = attach_candidates(chat_dir, image_md5);
    candidates.sort_by_key(|path| original_attachment_rank(path));
    candidates.into_iter().next()
}

fn attach_candidates(chat_dir: &Path, image_md5: &str) -> Vec<PathBuf> {
    let Ok(months) = std::fs::read_dir(chat_dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    for month in months.flatten() {
        if !month.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let month_path = month.path();
        for name in ["Img", "img", "Image", "image"] {
            let dir = month_path.join(name);
            if !dir.is_dir() {
                continue;
            }
            collect_named(&dir, image_md5, &mut candidates);
            if !candidates.is_empty() {
                break;
            }
        }
        if candidates.is_empty() {
            collect_named(&month_path, image_md5, &mut candidates);
        }
        if !candidates.is_empty() {
            break;
        }
    }
    candidates
}

fn original_attachment_rank(path: &Path) -> u8 {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with("_h.dat") {
        0
    } else if name.ends_with("_t.dat") {
        3
    } else if name.ends_with("_b.dat") {
        2
    } else {
        1
    }
}

fn collect_named(dir: &Path, image_md5: &str, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let Some(stem) = name.strip_suffix(".dat") else {
            continue;
        };
        let stem = stem
            .strip_suffix("_t")
            .or_else(|| stem.strip_suffix("_h"))
            .or_else(|| stem.strip_suffix("_b"))
            .unwrap_or(stem);
        if stem == image_md5 {
            output.push(entry.path());
        }
    }
}

fn attachment_rank(path: &Path) -> u8 {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with("_t.dat") {
        0
    } else if name.ends_with("_h.dat") {
        1
    } else if name.ends_with("_b.dat") {
        2
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_md5_from_image_xml() {
        let xml = r#"<msg><img aeskey="x" md5="0123456789ABCDEF0123456789abcdef" /></msg>"#;
        assert_eq!(
            extract_image_md5(xml).as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(extract_image_md5("<msg><img/></msg>").is_none());
    }

    #[test]
    fn finds_thumbnail_variant() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("2026-09").join("Img");
        std::fs::create_dir_all(&dir).expect("create dir");
        for suffix in ["_h", "", "_t"] {
            std::fs::write(
                dir.join(format!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa{suffix}.dat")),
                b"data",
            )
            .expect("write");
        }
        let found =
            find_attach_image(temp.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("thumbnail");
        assert!(found.to_string_lossy().ends_with("_t.dat"));
        let original = find_attach_image_original(temp.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("original");
        assert!(original.to_string_lossy().ends_with("_h.dat"));
    }

    #[test]
    fn finds_exported_resource_image() {
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let temp = tempfile::tempdir().expect("tempdir");
        let md5 = "0123456789abcdef0123456789abcdef";
        let dir = temp.path().join("resource").join(&md5[..2]);
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join(format!("{md5}.png")), PNG).expect("write");
        let image = find_resource_image(temp.path(), md5).expect("exported resource");
        assert_eq!(image.media_type, "image/png");

        let keys = ImageKeys {
            aes_key: "0123456789abcdef".to_string(),
            xor_key: None,
        };
        let listed = load_message_image(temp.path(), "chat", md5, 1, 1, &keys)
            .expect("list image from exported resource");
        assert_eq!(listed.media_type, "image/png");
    }
}
