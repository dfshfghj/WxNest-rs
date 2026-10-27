use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::sqlite::SqlValue;

const MD5_LENGTH: usize = 32;
const ALLOWED_EXTENSIONS: &[&str] = &[
    ".dat", ".gif", ".heic", ".heif", ".jpeg", ".jpg", ".m4v", ".mov", ".mp4", ".png", ".webp",
];
const VIDEO_EXTENSIONS: &[&str] = &[".m4v", ".mov", ".mp4"];

fn hex32_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)[0-9a-f]{32}").expect("valid md5 regex"))
}

fn dat_name_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)([0-9a-f]{32})(?:[._][thbc])?\.dat").expect("valid dat regex")
    })
}

fn strip_suffix_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)(?:_h|_t|_thumb)$").expect("valid suffix regex"))
}

fn is_md5(value: &str) -> bool {
    value.len() == MD5_LENGTH && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn most_common(values: impl Iterator<Item = String>) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for value in values {
        match counts.iter_mut().find(|(key, _)| *key == value) {
            Some((_, count)) => *count += 1,
            None => counts.push((value, 1)),
        }
    }
    let mut best = String::new();
    let mut best_count = 0;
    for (value, count) in counts {
        if count > best_count {
            best = value;
            best_count = count;
        }
    }
    best
}

pub fn extract_md5_from_blob(blob: &[u8]) -> String {
    if blob.is_empty() {
        return String::new();
    }
    let text = String::from_utf8_lossy(blob);
    let matches: Vec<String> = dat_name_pattern()
        .captures_iter(&text)
        .filter_map(|captures| captures.get(1))
        .map(|value| value.as_str().to_ascii_lowercase())
        .collect();
    if !matches.is_empty() {
        return most_common(matches.into_iter());
    }
    let matches: Vec<String> = hex32_pattern()
        .find_iter(&text)
        .map(|value| value.as_str().to_ascii_lowercase())
        .collect();
    most_common(matches.into_iter())
}

fn packed_info_bytes(value: &SqlValue) -> Option<Vec<u8>> {
    match value {
        SqlValue::Null | SqlValue::Integer(_) | SqlValue::Real(_) => None,
        SqlValue::Blob(bytes) => Some(bytes.clone()),
        SqlValue::Text(text) => {
            let text = text.trim();
            let text = text
                .strip_prefix("0x")
                .or_else(|| text.strip_prefix("0X"))
                .unwrap_or(text);
            if !text.is_empty()
                && text.len() % 2 == 0
                && text.chars().all(|ch| ch.is_ascii_hexdigit())
            {
                hex::decode(text).ok()
            } else {
                Some(text.as_bytes().to_vec())
            }
        }
    }
}

pub fn extract_md5_from_packed_info(value: &SqlValue) -> String {
    let Some(bytes) = packed_info_bytes(value) else {
        return String::new();
    };
    if bytes.len() == 16 {
        return hex::encode(bytes);
    }
    let md5 = extract_md5_from_blob(&bytes).to_ascii_lowercase();
    if md5.len() == MD5_LENGTH && md5.chars().all(|ch| ch.is_ascii_hexdigit()) {
        md5
    } else {
        String::new()
    }
}

#[derive(Debug)]
enum ProtoField<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn read_varint(raw: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    for _ in 0..10 {
        if pos >= raw.len() {
            return None;
        }
        let byte = raw[pos];
        pos += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((value, pos));
        }
        shift += 7;
    }
    None
}

fn parse_fields(raw: &[u8]) -> Option<Vec<(u64, ProtoField<'_>)>> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < raw.len() {
        let (tag, next) = read_varint(raw, pos)?;
        if next <= pos {
            return None;
        }
        pos = next;
        let field_no = tag >> 3;
        let wire_type = tag & 0x07;
        if field_no == 0 {
            return None;
        }
        match wire_type {
            0 => {
                let (value, next) = read_varint(raw, pos)?;
                if next <= pos {
                    return None;
                }
                fields.push((field_no, ProtoField::Varint(value)));
                pos = next;
            }
            1 => {
                if pos + 8 > raw.len() {
                    return None;
                }
                fields.push((field_no, ProtoField::Fixed));
                pos += 8;
            }
            2 => {
                let (size, next) = read_varint(raw, pos)?;
                if next <= pos {
                    return None;
                }
                pos = next;
                let end = pos.checked_add(size as usize)?;
                if end > raw.len() {
                    return None;
                }
                fields.push((field_no, ProtoField::Bytes(&raw[pos..end])));
                pos = end;
            }
            5 => {
                if pos + 4 > raw.len() {
                    return None;
                }
                fields.push((field_no, ProtoField::Fixed));
                pos += 4;
            }
            _ => return None,
        }
    }
    Some(fields)
}

pub fn extract_voice_transcript_from_packed_info(value: &SqlValue) -> String {
    let Some(bytes) = packed_info_bytes(value) else {
        return String::new();
    };
    if bytes.is_empty() || bytes.len() > 1024 * 1024 {
        return String::new();
    }
    let Some(top_fields) = parse_fields(&bytes) else {
        return String::new();
    };
    for (field_no, field) in top_fields {
        let ProtoField::Bytes(nested_blob) = field else {
            continue;
        };
        if field_no != 5 {
            continue;
        }
        let Some(nested_fields) = parse_fields(nested_blob) else {
            continue;
        };
        let mut status: Option<u64> = None;
        let mut text_blob: Option<&[u8]> = None;
        for (nested_no, nested_field) in nested_fields {
            match (nested_no, nested_field) {
                (1, ProtoField::Varint(value)) => status = Some(value),
                (2, ProtoField::Bytes(bytes)) => text_blob = Some(bytes),
                _ => {}
            }
        }
        if status != Some(2) {
            continue;
        }
        let Some(text_blob) = text_blob else {
            continue;
        };
        let Ok(transcript) = std::str::from_utf8(text_blob) else {
            continue;
        };
        let transcript = transcript.trim();
        if !transcript.is_empty() {
            return transcript.to_string();
        }
    }
    String::new()
}

fn iter_media_lookup_keys(name: &str) -> Vec<String> {
    let lower = name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return Vec::new();
    }
    let stem = Path::new(&lower)
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut keys: Vec<String> = Vec::new();
    for value in [lower, stem.clone()] {
        if !value.is_empty() && !keys.contains(&value) {
            keys.push(value);
        }
    }
    let stripped = strip_suffix_pattern().replace(&stem, "").to_string();
    if !stripped.is_empty() && !keys.contains(&stripped) {
        keys.push(stripped);
    }
    keys
}

pub fn iter_md5_candidates_from_name(name: &str) -> Vec<String> {
    iter_media_lookup_keys(name)
        .into_iter()
        .filter(|key| is_md5(key))
        .fold(Vec::new(), |mut candidates, key| {
            if !candidates.contains(&key) {
                candidates.push(key);
            }
            candidates
        })
}

fn iter_files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }
    files
}

fn suffix(path: &Path) -> String {
    path.extension()
        .map(|value| format!(".{}", value.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default()
}

#[derive(Debug, Default, Clone)]
pub struct MediaIndex {
    md5_hits: HashMap<String, HashMap<String, PathBuf>>,
}

impl MediaIndex {
    const KINDS: [&'static str; 5] = ["image", "emoji", "video", "video_thumb", "file"];

    pub fn build(account_dir: &Path) -> Self {
        Self::build_with_roots(&[account_dir.to_path_buf()])
    }

    pub fn build_with_roots(roots: &[PathBuf]) -> Self {
        let mut index = Self::default();
        for root in roots {
            index.index_decrypted_resources(root);
        }
        for root in roots {
            index.scan_media_roots(root);
        }
        index
    }

    pub fn resolve(&self, kind: &str, md5: &str) -> Option<&Path> {
        let kind = kind.trim().to_ascii_lowercase();
        let md5 = md5.trim().to_ascii_lowercase();
        if !is_md5(&md5) {
            return None;
        }
        let preferred: &[&str] = match kind.as_str() {
            "emoji" => &["emoji", "image"],
            "video_thumb" => &["video_thumb", "image"],
            other => {
                if !Self::KINDS.contains(&other) {
                    return None;
                }
                return self
                    .md5_hits
                    .get(other)
                    .and_then(|hits| hits.get(&md5))
                    .filter(|path| path.is_file())
                    .map(PathBuf::as_path);
            }
        };
        for candidate in preferred {
            if let Some(path) = self
                .md5_hits
                .get(*candidate)
                .and_then(|hits| hits.get(&md5))
            {
                if path.is_file() {
                    return Some(path.as_path());
                }
            }
        }
        None
    }

    fn put_md5(&mut self, kind: &str, md5: String, path: &Path) {
        if md5.is_empty() {
            return;
        }
        self.md5_hits
            .entry(kind.to_string())
            .or_default()
            .entry(md5)
            .or_insert_with(|| path.to_path_buf());
    }

    fn register_kind_path(&mut self, kind: &str, path: &Path) {
        let Some(name) = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
        else {
            return;
        };
        for md5 in iter_md5_candidates_from_name(&name) {
            self.put_md5(kind, md5, path);
        }
    }

    fn index_decrypted_resources(&mut self, root: &Path) {
        let resource_dir = root.join("resource");
        if !resource_dir.is_dir() {
            return;
        }
        for path in iter_files_under(&resource_dir) {
            if iter_md5_candidates_from_name(&file_name_string(&path)).is_empty() {
                continue;
            }
            if VIDEO_EXTENSIONS.contains(&suffix(&path).as_str()) {
                self.register_kind_path("video", &path);
            } else {
                self.register_kind_path("image", &path);
                self.register_kind_path("emoji", &path);
                self.register_kind_path("video_thumb", &path);
            }
        }
    }

    fn scan_media_roots(&mut self, root: &Path) {
        let attach_root = root.join("msg").join("attach");
        if let Ok(entries) = std::fs::read_dir(&attach_root) {
            for entry in entries.flatten() {
                if entry
                    .metadata()
                    .map(|metadata| metadata.is_dir())
                    .unwrap_or(false)
                {
                    self.scan_attach_dir(&entry.path());
                }
            }
        }

        for directory in directory_candidates(root, "video") {
            self.scan_video_dir(&directory);
        }
        for directory in directory_candidates(root, "file") {
            self.scan_file_dir(&directory);
        }
        let cache_dir = root.join("cache");
        if cache_dir.is_dir() {
            self.scan_cache_dir(&cache_dir);
        }
    }

    fn scan_attach_dir(&mut self, directory: &Path) {
        for path in iter_files_under(directory) {
            let extension = suffix(&path);
            if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
                continue;
            }
            if VIDEO_EXTENSIONS.contains(&extension.as_str()) {
                self.register_kind_path("video", &path);
            } else {
                self.register_kind_path("image", &path);
            }
        }
    }

    fn scan_video_dir(&mut self, directory: &Path) {
        for path in iter_files_under(directory) {
            let extension = suffix(&path);
            if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
                continue;
            }
            if VIDEO_EXTENSIONS.contains(&extension.as_str()) {
                self.register_kind_path("video", &path);
            } else if extension == ".dat" {
                self.register_kind_path("video", &path);
                self.register_kind_path("video_thumb", &path);
            } else {
                self.register_kind_path("video_thumb", &path);
            }
        }
    }

    fn scan_file_dir(&mut self, directory: &Path) {
        for path in iter_files_under(directory) {
            self.register_kind_path("file", &path);
            if VIDEO_EXTENSIONS.contains(&suffix(&path).as_str()) {
                self.register_kind_path("video", &path);
            }
        }
    }

    fn scan_cache_dir(&mut self, directory: &Path) {
        for path in iter_files_under(directory) {
            let extension = suffix(&path);
            if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
                continue;
            }
            let emoji_component = path.components().any(|component| {
                let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
                value == "emoji" || value == "emoticon"
            });
            if emoji_component {
                self.register_kind_path("emoji", &path);
            } else if VIDEO_EXTENSIONS.contains(&extension.as_str()) {
                self.register_kind_path("video", &path);
            } else {
                self.register_kind_path("video_thumb", &path);
            }
        }
    }
}

fn file_name_string(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn directory_candidates(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let nested = root.join("msg").join(name);
    if nested.is_dir() {
        candidates.push(nested);
    }
    let direct = root.join(name);
    if direct.is_dir() && !candidates.contains(&direct) {
        candidates.push(direct);
    }
    if root
        .file_name()
        .map(|value| value.to_string_lossy().eq_ignore_ascii_case(name))
        .unwrap_or(false)
        && !candidates.contains(&root.to_path_buf())
    {
        candidates.push(root.to_path_buf());
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_dat_referenced_md5() {
        let blob =
            b"path=0123456789abcdef0123456789abcdef_t.dat other=ffffffffffffffffffffffffffffffff";
        assert_eq!(
            extract_md5_from_blob(blob),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn extracts_md5_candidates_from_names() {
        assert_eq!(
            iter_md5_candidates_from_name("0123456789abcdef0123456789abcdef_t.dat"),
            vec!["0123456789abcdef0123456789abcdef".to_string()]
        );
        assert!(iter_md5_candidates_from_name("not-a-md5.dat").is_empty());
    }

    #[test]
    fn extract_packed_16_bytes() {
        let bytes = (0u8..16).collect::<Vec<_>>();
        assert_eq!(
            extract_md5_from_packed_info(&SqlValue::Blob(bytes)),
            "000102030405060708090a0b0c0d0e0f"
        );
    }

    #[test]
    fn extracts_completed_voice_transcript() {
        fn varint(mut value: u64) -> Vec<u8> {
            let mut output = Vec::new();
            loop {
                let byte = (value & 0x7F) as u8;
                value >>= 7;
                if value == 0 {
                    output.push(byte);
                    return output;
                }
                output.push(byte | 0x80);
            }
        }
        let transcript = "语音转写";
        let mut nested = vec![1 << 3, 2];
        nested.extend(varint(((2 << 3) | 2) as u64));
        nested.extend(varint(transcript.len() as u64));
        nested.extend_from_slice(transcript.as_bytes());
        let mut payload = varint(((5 << 3) | 2) as u64);
        payload.extend(varint(nested.len() as u64));
        payload.extend(nested);
        assert_eq!(
            extract_voice_transcript_from_packed_info(&SqlValue::Blob(payload)),
            transcript
        );
    }
}
