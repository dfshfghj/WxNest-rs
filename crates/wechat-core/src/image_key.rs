use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use md5::{Digest, Md5};
use regex::Regex;
use serde::Serialize;

use crate::crypto::aes::aes_ecb_decrypt_128;
use crate::error::{CoreError, Result};

pub const V2_MAGIC: &[u8; 6] = b"\x07\x08V2\x08\x07";
pub const V2_CIPHERTEXT_START: usize = 0x0F;
pub const AES_BLOCK_SIZE: usize = 16;
const MAX_CODE: u64 = 0xFFFF_FFFF;
const MAX_PREFERRED_DIRS: usize = 2_000;
const SKIPPED_FALLBACK_DIR_PARTS: &[&str] = &["thumb", "emoticon"];

#[derive(Debug, Clone, Serialize)]
pub struct DerivedImageKeys {
    pub xor_key: u8,
    pub aes_key: String,
}

#[derive(Debug, Clone)]
pub struct V2Template {
    pub path: PathBuf,
    pub ciphertext: Vec<u8>,
    pub mtime_ns: u64,
    pub tail_xor_key: Option<u8>,
    pub tail_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct TemplateScanResult {
    pub templates: Vec<V2Template>,
    pub inferred_xor_key: Option<u8>,
    pub used_fallback: bool,
    pub files_scanned: usize,
    pub xor_support: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageKeyResolution {
    pub code: u32,
    pub wxid: String,
    pub xor_key: u8,
    pub xor_key_hex: String,
    pub aes_key: String,
    pub verified: bool,
    pub template_path: String,
    pub inferred_xor_key: Option<u8>,
}

pub fn enumerate_kvcomm_codes(kvcomm_dir: &Path) -> Vec<u32> {
    let pattern = kvcomm_pattern();
    let mut codes: Vec<u32> = Vec::new();
    let entries = match fs::read_dir(kvcomm_dir) {
        Ok(value) => value,
        Err(_) => return codes,
    };
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(value) => value,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        let Some(captures) = pattern.captures(&file_name) else {
            continue;
        };
        let Ok(code) = captures[1].parse::<u64>() else {
            continue;
        };
        if code > 0 && code <= MAX_CODE {
            let code = code as u32;
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
    }
    codes.sort_unstable();
    codes
}

pub fn clean_wxid(value: Option<&str>) -> String {
    let candidate = value.unwrap_or_default().trim();
    if candidate.is_empty() {
        return String::new();
    }
    let pattern = wxid_suffix_pattern();
    match pattern.captures(candidate) {
        Some(captures) => captures[1].to_string(),
        None => candidate.to_string(),
    }
}

pub fn collect_wxid_candidates(
    target_wxid: Option<&str>,
    account: Option<&str>,
    local_native_wxids: &[String],
) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for raw in [target_wxid, account]
        .into_iter()
        .chain(local_native_wxids.iter().map(|value| Some(value.as_str())))
        .chain(std::iter::once(Some("unknown")))
    {
        let value = clean_wxid(raw);
        if value.is_empty() || !seen.insert(value.clone()) {
            continue;
        }
        candidates.push(value);
    }
    candidates
}

pub fn derive_image_keys(code: u32, wxid: &str) -> Result<DerivedImageKeys> {
    if code == 0 {
        return Err(CoreError::ImageKey(
            "code must be an integer in the range 1..0xffffffff".into(),
        ));
    }
    let cleaned_wxid = clean_wxid(Some(wxid));
    if cleaned_wxid.is_empty() {
        return Err(CoreError::ImageKey("wxid must not be empty".into()));
    }
    let mut digest = Md5::new();
    digest.update(format!("{code}{cleaned_wxid}").as_bytes());
    let hex_digest = hex::encode(digest.finalize());
    Ok(DerivedImageKeys {
        xor_key: (code & 0xFF) as u8,
        aes_key: hex_digest[..16].to_string(),
    })
}

pub fn decrypt_aes_block(aes_key: &str, ciphertext: &[u8]) -> Option<[u8; AES_BLOCK_SIZE]> {
    let key_bytes = aes_key.as_bytes();
    if key_bytes.len() < AES_BLOCK_SIZE || ciphertext.len() != AES_BLOCK_SIZE {
        return None;
    }
    if !key_bytes.iter().all(u8::is_ascii) {
        return None;
    }
    let plain = aes_ecb_decrypt_128(&key_bytes[..AES_BLOCK_SIZE], ciphertext).ok()?;
    plain.try_into().ok()
}

pub fn detect_image_format(plaintext: &[u8]) -> Option<&'static str> {
    if plaintext.is_empty() {
        return None;
    }
    if plaintext.starts_with(b"\xff\xd8\xff") {
        return Some("jpeg");
    }
    if plaintext.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("png");
    }
    if plaintext.starts_with(b"RIFF") && plaintext.len() >= 12 && &plaintext[8..12] == b"WEBP" {
        return Some("webp");
    }
    if plaintext.starts_with(b"wxgf") || plaintext.starts_with(b"WXGF") {
        return Some("wxgf");
    }
    if plaintext.starts_with(b"GIF87a") || plaintext.starts_with(b"GIF89a") {
        return Some("gif");
    }
    None
}

pub fn verify_aes_key(aes_key: &str, ciphertext: &[u8]) -> bool {
    decrypt_aes_block(aes_key, ciphertext)
        .map(|plain| detect_image_format(&plain).is_some())
        .unwrap_or(false)
}

pub fn infer_xor_key_with_support(tails: &[Vec<u8>]) -> (Option<u8>, usize) {
    let mut counts: HashMap<[u8; 2], (usize, usize)> = HashMap::new();
    for (index, tail) in tails.iter().enumerate() {
        if tail.len() != 2 {
            continue;
        }
        let pair = [tail[0], tail[1]];
        let entry = counts.entry(pair).or_insert((0, index));
        entry.0 += 1;
    }
    let Some((pair, support)) = counts
        .into_iter()
        .max_by(|left, right| {
            left.1
                 .0
                .cmp(&right.1 .0)
                .then_with(|| right.1 .1.cmp(&left.1 .1))
        })
        .map(|(pair, value)| (pair, value.0))
    else {
        return (None, 0);
    };
    let first = pair[0] ^ 0xFF;
    let second = pair[1] ^ 0xD9;
    (if first == second { Some(first) } else { None }, support)
}

pub fn infer_xor_key_from_v2_tails(tails: &[Vec<u8>]) -> Option<u8> {
    infer_xor_key_with_support(tails).0
}

pub fn scan_v2_templates(
    account_dir: &Path,
    limit: usize,
    max_fallback_dirs: usize,
) -> TemplateScanResult {
    if limit == 0 {
        return TemplateScanResult::default();
    }
    let discovery_capacity = (limit * 4).max(64);
    let preferred_paths = collect_preferred_paths(account_dir, discovery_capacity);
    let (mut templates, mut files_scanned) = read_v2_templates(&preferred_paths, limit);
    let mut used_fallback = false;

    if templates.is_empty() && max_fallback_dirs > 0 {
        used_fallback = true;
        let excluded: HashSet<String> = preferred_paths
            .iter()
            .map(|path| normcase_abs(path))
            .collect();
        let fallback_paths = collect_fallback_paths(
            account_dir,
            discovery_capacity,
            max_fallback_dirs,
            &excluded,
        );
        let (fallback_templates, fallback_scanned) = read_v2_templates(&fallback_paths, limit);
        templates = fallback_templates;
        files_scanned += fallback_scanned;
    }

    let tails: Vec<Vec<u8>> = templates
        .iter()
        .map(|template| template.tail_bytes.clone())
        .collect();
    let (inferred_xor_key, xor_support) = infer_xor_key_with_support(&tails);
    TemplateScanResult {
        templates,
        inferred_xor_key,
        used_fallback,
        files_scanned,
        xor_support,
    }
}

pub fn current_v2_template(templates: &[V2Template]) -> Option<&V2Template> {
    templates.first()
}

pub fn trusted_xor_for_verified_aes_key(aes_key: &str, templates: &[V2Template]) -> Option<u8> {
    let current = current_v2_template(templates)?;
    let plain = decrypt_aes_block(aes_key, &current.ciphertext)?;
    detect_image_format(&plain)?;

    let mut matching: Vec<u8> = Vec::new();
    for template in templates {
        let Some(plain) = decrypt_aes_block(aes_key, &template.ciphertext) else {
            continue;
        };
        if detect_image_format(&plain) == Some("jpeg") {
            if let Some(xor_key) = template.tail_xor_key {
                matching.push(xor_key);
            }
        }
    }
    if matching.is_empty() {
        return None;
    }
    most_common_u8(&matching)
}

pub fn verify_key_pair(
    xor_key: u8,
    aes_key: &str,
    templates: &[V2Template],
    require_xor_match: bool,
) -> bool {
    let Some(current) = current_v2_template(templates) else {
        return false;
    };
    if require_xor_match {
        let Some(current_xor) = trusted_xor_for_verified_aes_key(aes_key, templates) else {
            return false;
        };
        if current_xor != xor_key {
            return false;
        }
    }
    verify_aes_key(aes_key, &current.ciphertext)
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_local_image_key(
    kvcomm_dir: &Path,
    account_dir: &Path,
    target_wxid: Option<&str>,
    account: Option<&str>,
    local_native_wxids: &[String],
    template_scan: Option<TemplateScanResult>,
    template_limit: usize,
    max_fallback_dirs: usize,
) -> Option<ImageKeyResolution> {
    let codes = enumerate_kvcomm_codes(kvcomm_dir);
    if codes.is_empty() {
        return None;
    }

    let mut native_values: Vec<String> = local_native_wxids.to_vec();
    if let Some(name) = account_dir.file_name() {
        native_values.push(name.to_string_lossy().to_string());
    }
    let wxids = collect_wxid_candidates(target_wxid, account, &native_values);
    if wxids.is_empty() {
        return None;
    }

    let template_data = template_scan
        .unwrap_or_else(|| scan_v2_templates(account_dir, template_limit, max_fallback_dirs));
    let current = current_v2_template(&template_data.templates)?;
    let current_xor_key =
        if template_data.inferred_xor_key.is_some() && template_data.xor_support >= 2 {
            template_data.inferred_xor_key
        } else {
            current.tail_xor_key
        };

    let mut ordered_codes = codes;
    if let Some(current_xor) = current_xor_key {
        ordered_codes.sort_by_key(|code| ((code & 0xFF) as u8 != current_xor, *code));
    }

    for wxid in wxids {
        for code in &ordered_codes {
            let keys = derive_image_keys(*code, &wxid).ok()?;
            if !verify_aes_key(&keys.aes_key, &current.ciphertext) {
                continue;
            }
            return Some(ImageKeyResolution {
                code: *code,
                wxid: clean_wxid(Some(&wxid)),
                xor_key: keys.xor_key,
                xor_key_hex: format!("0x{:02X}", keys.xor_key),
                aes_key: keys.aes_key,
                verified: true,
                template_path: current.path.display().to_string(),
                inferred_xor_key: current_xor_key,
            });
        }
    }
    None
}

fn collect_preferred_paths(account_dir: &Path, capacity: usize) -> Vec<PathBuf> {
    let attach_root = account_dir.join("msg").join("attach");
    let mut attach_dirs = list_child_dirs(&attach_root);
    attach_dirs.sort_by(|left, right| {
        let left_mtime = safe_mtime_ns(left);
        let right_mtime = safe_mtime_ns(right);
        right_mtime.cmp(&left_mtime).then_with(|| {
            left.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
                .cmp(
                    &right
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase(),
                )
        })
    });

    let month_pattern = month_pattern();
    let mut heap: Vec<(u64, String, String)> = Vec::new();
    let mut visited_dirs = 0usize;
    for attach_dir in attach_dirs {
        if visited_dirs >= MAX_PREFERRED_DIRS {
            break;
        }
        visited_dirs += 1;
        let mut month_dirs: Vec<PathBuf> = list_child_dirs(&attach_dir)
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .map(|value| month_pattern.is_match(&value.to_string_lossy()))
                    .unwrap_or(false)
            })
            .collect();
        month_dirs.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
        for month_dir in month_dirs {
            if visited_dirs >= MAX_PREFERRED_DIRS {
                break;
            }
            visited_dirs += 1;
            for img_dir in list_child_dirs(&month_dir).into_iter().filter(|path| {
                path.file_name()
                    .map(|value| value.to_string_lossy().to_lowercase() == "img")
                    .unwrap_or(false)
            }) {
                if visited_dirs >= MAX_PREFERRED_DIRS {
                    break;
                }
                visited_dirs += 1;
                offer_recent_template_files(&img_dir, &mut heap, capacity, &HashSet::new());
            }
        }
    }
    recent_paths(heap)
}

fn collect_fallback_paths(
    account_dir: &Path,
    capacity: usize,
    max_dirs: usize,
    excluded_paths: &HashSet<String>,
) -> Vec<PathBuf> {
    let mut queue: Vec<PathBuf> = ["msg", "cache", "resource"]
        .iter()
        .map(|name| account_dir.join(name))
        .filter(|path| path.is_dir())
        .collect();
    let mut heap: Vec<(u64, String, String)> = Vec::new();
    let mut visited_dirs = 0usize;
    let mut queue_index = 0usize;

    while queue_index < queue.len() && visited_dirs < max_dirs {
        let directory = queue[queue_index].clone();
        queue_index += 1;
        visited_dirs += 1;
        offer_recent_template_files(&directory, &mut heap, capacity, excluded_paths);

        let mut child_dirs = list_child_dirs(&directory);
        child_dirs.sort_by_key(|path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
        });
        for child in child_dirs {
            let lower_name = child
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            if SKIPPED_FALLBACK_DIR_PARTS
                .iter()
                .any(|part| lower_name.contains(part))
            {
                continue;
            }
            if queue.len() >= max_dirs {
                break;
            }
            queue.push(child);
        }
    }
    recent_paths(heap)
}

fn offer_recent_template_files(
    directory: &Path,
    heap: &mut Vec<(u64, String, String)>,
    capacity: usize,
    excluded_paths: &HashSet<String>,
) {
    let entries = match fs::read_dir(directory) {
        Ok(value) => value,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_lowercase().ends_with("_t.dat") {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(value) => value,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let path_key = normcase_abs(&path);
        if excluded_paths.contains(&path_key) {
            continue;
        }
        let mtime_ns = safe_mtime_ns(&path);
        let path_text = path.display().to_string();
        heap.push((mtime_ns, path_text.to_lowercase(), path_text));
    }
    heap.sort();
    if heap.len() > capacity {
        heap.drain(..heap.len() - capacity);
    }
}

fn recent_paths(mut heap: Vec<(u64, String, String)>) -> Vec<PathBuf> {
    heap.sort_by(|left, right| right.cmp(left));
    heap.into_iter().map(|item| PathBuf::from(item.2)).collect()
}

fn read_v2_templates(paths: &[PathBuf], limit: usize) -> (Vec<V2Template>, usize) {
    let mut templates = Vec::new();
    let mut files_scanned = 0usize;
    for path in paths {
        if templates.len() >= limit {
            break;
        }
        files_scanned += 1;
        let mut file = match File::open(path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let mut header = vec![0u8; V2_CIPHERTEXT_START + AES_BLOCK_SIZE];
        if file.read_exact(&mut header).is_err()
            || header.len() < V2_CIPHERTEXT_START + AES_BLOCK_SIZE
            || !header.starts_with(V2_MAGIC)
        {
            continue;
        }
        if file.seek(SeekFrom::End(-2)).is_err() {
            continue;
        }
        let mut tail = [0u8; 2];
        if file.read_exact(&mut tail).is_err() {
            continue;
        }
        let tail_bytes = tail.to_vec();
        let tail_xor_key = infer_xor_key_from_v2_tails(std::slice::from_ref(&tail_bytes));
        templates.push(V2Template {
            path: path.clone(),
            ciphertext: header[V2_CIPHERTEXT_START..V2_CIPHERTEXT_START + AES_BLOCK_SIZE].to_vec(),
            mtime_ns: safe_mtime_ns(path),
            tail_xor_key,
            tail_bytes,
        });
    }
    (templates, files_scanned)
}

fn list_child_dirs(path: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(path) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    entries
        .flatten()
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|file_type| file_type.is_dir())
                .map(|_| entry.path())
        })
        .collect()
}

fn safe_mtime_ns(path: &Path) -> u64 {
    let Ok(metadata) = fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    modified
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn normcase_abs(path: &Path) -> String {
    let absolute = fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|directory| directory.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    });
    #[cfg(windows)]
    {
        absolute.to_string_lossy().replace('/', "\\").to_lowercase()
    }
    #[cfg(not(windows))]
    {
        absolute.to_string_lossy().to_string()
    }
}

fn most_common_u8(values: &[u8]) -> Option<u8> {
    let mut counts: HashMap<u8, (usize, usize)> = HashMap::new();
    for (index, value) in values.iter().enumerate() {
        let entry = counts.entry(*value).or_insert((0, index));
        entry.0 += 1;
    }
    counts
        .into_iter()
        .max_by(|left, right| {
            left.1
                 .0
                .cmp(&right.1 .0)
                .then_with(|| right.1 .1.cmp(&left.1 .1))
        })
        .map(|(value, _)| value)
}

fn kvcomm_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"(?i)^key_(\d+)_.+\.statistic$").expect("valid kvcomm regex"))
}

fn wxid_suffix_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)^(wxid_[^_]+)(?:_.+)$").expect("valid wxid regex"))
}

fn month_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^\d{4}-\d{2}$").expect("valid month regex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_weflow_keys() {
        let keys = derive_image_keys(0x12_34_56_78, "wxid_abc_1234").unwrap();
        assert_eq!(keys.xor_key, 0x78);
        assert_eq!(keys.aes_key.len(), 16);
        assert_eq!(clean_wxid(Some("wxid_abc_1234")), "wxid_abc");
    }

    #[test]
    fn tail_inference() {
        let xor = 0x5Au8;
        let tail = vec![0xFF ^ xor, 0xD9 ^ xor];
        assert_eq!(infer_xor_key_from_v2_tails(&[tail]), Some(xor));
    }

    #[test]
    fn collects_wxid_candidates_in_order() {
        let values = collect_wxid_candidates(
            Some("wxid_target_1"),
            Some("account"),
            &["wxid_native_2".into()],
        );
        assert_eq!(
            values,
            vec!["wxid_target", "account", "wxid_native", "unknown"]
        );
    }
}
