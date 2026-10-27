use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use subtle::ConstantTimeEq;

use crate::crypto::kdf::{derive_enc_key, derive_mac_key, hmac_sha512_parts};
use crate::error::{CoreError, Result};
use crate::model::KeyValidationResult;

use super::{HMAC_SIZE, IV_SIZE, KEY_SIZE, PAGE_SIZE, RESERVE_SIZE, SALT_SIZE, SQLITE_HEADER};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    RawEncKey,
    SqlcipherPassphrase,
}

impl KeyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RawEncKey => "raw_enc_key",
            Self::SqlcipherPassphrase => "sqlcipher_passphrase",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedKey {
    pub enc_key: [u8; KEY_SIZE],
    pub mac_key: [u8; KEY_SIZE],
    pub mode: KeyMode,
}

pub fn parse_key_hex(key_hex: &str) -> Result<[u8; KEY_SIZE]> {
    let encoded = key_hex.trim();
    if encoded.len() != KEY_SIZE * 2 {
        return Err(CoreError::InvalidKey("密钥必须是64位十六进制字符串".into()));
    }
    let bytes = hex::decode(encoded)
        .map_err(|_| CoreError::InvalidKey("密钥必须是有效的十六进制字符串".into()))?;
    bytes
        .try_into()
        .map_err(|_| CoreError::InvalidKey("密钥长度必须为32字节".into()))
}

pub fn compute_page_hmac(mac_key: &[u8], page: &[u8], page_num: u32) -> Option<[u8; 64]> {
    if page.len() < PAGE_SIZE {
        return None;
    }
    let offset = if page_num == 1 { SALT_SIZE } else { 0 };
    let data_end = PAGE_SIZE - RESERVE_SIZE + IV_SIZE;
    Some(hmac_sha512_parts(
        mac_key,
        &[&page[offset..data_end], &page_num.to_le_bytes()],
    ))
}

pub fn resolve_page1_key_material(
    key_material: &[u8; KEY_SIZE],
    page1: &[u8],
) -> Option<ResolvedKey> {
    if page1.len() < PAGE_SIZE {
        return None;
    }
    let salt = &page1[..SALT_SIZE];
    let stored = &page1[PAGE_SIZE - HMAC_SIZE..PAGE_SIZE];

    let raw_mac = derive_mac_key(key_material, salt).ok()?;
    if let Some(expected) = compute_page_hmac(&raw_mac, page1, 1) {
        if bool::from(stored.ct_eq(&expected)) {
            return Some(ResolvedKey {
                enc_key: *key_material,
                mac_key: raw_mac,
                mode: KeyMode::RawEncKey,
            });
        }
    }

    let derived = derive_enc_key(key_material, salt).ok()?;
    let derived_mac = derive_mac_key(&derived, salt).ok()?;
    if let Some(expected) = compute_page_hmac(&derived_mac, page1, 1) {
        if bool::from(stored.ct_eq(&expected)) {
            return Some(ResolvedKey {
                enc_key: derived,
                mac_key: derived_mac,
                mode: KeyMode::SqlcipherPassphrase,
            });
        }
    }
    None
}

pub fn validate_realtime_database_key(
    db_storage_path: &Path,
    key_hex: &str,
) -> KeyValidationResult {
    let key_material = match parse_key_hex(key_hex) {
        Ok(value) => value,
        Err(_) => return KeyValidationResult::invalid("invalid_key_format"),
    };

    let root = match std::fs::canonicalize(db_storage_path) {
        Ok(value) => value,
        Err(_) => return KeyValidationResult::invalid("invalid_db_storage_path"),
    };
    if !root.is_dir() {
        return KeyValidationResult::invalid("invalid_db_storage_path");
    }

    let combined = first_file(&[root.join("MicroMsg.db"), root.join("micromsg.db")]);
    let session = first_file(&[
        root.join("session").join("session.db"),
        root.join("session.db"),
        root.join("Session.db"),
    ])
    .or_else(|| combined.clone());

    let mut message_candidates: Vec<PathBuf> = Vec::new();
    message_candidates.extend(list_matching(&root.join("message"), |name| {
        name.starts_with("message_") && name.ends_with(".db")
    }));
    message_candidates.extend(list_matching(&root, |name| {
        name.starts_with("message_") && name.ends_with(".db")
    }));
    message_candidates.extend(list_matching(&root, |name| {
        name.starts_with("MSG") && name.ends_with(".db")
    }));
    message_candidates.extend(list_matching(&root, |name| {
        name.starts_with("msg") && name.ends_with(".db")
    }));
    let message = first_file(&message_candidates).or_else(|| combined.clone());

    let mut role_paths: Vec<(&str, PathBuf)> = Vec::new();
    if let Some(path) = message {
        role_paths.push(("message", path));
    }
    if let Some(path) = session {
        role_paths.push(("session", path));
    }

    let mut verified_roles: Vec<String> = Vec::new();
    let mut modes: BTreeMap<String, String> = BTreeMap::new();
    let mut path_results: HashMap<PathBuf, String> = HashMap::new();

    for (role, database_path) in role_paths {
        let mode = match path_results.get(&database_path) {
            Some(mode) => mode.clone(),
            None => {
                let page1 = read_first_page(&database_path).unwrap_or_default();
                let mode = if page1.starts_with(SQLITE_HEADER) {
                    String::new()
                } else {
                    resolve_page1_key_material(&key_material, &page1)
                        .map(|resolved| resolved.mode.as_str().to_string())
                        .unwrap_or_default()
                };
                path_results.insert(database_path, mode.clone());
                mode
            }
        };
        if !mode.is_empty() {
            verified_roles.push(role.to_string());
            modes.insert(role.to_string(), mode);
        }
    }

    verified_roles.sort();
    let valid = verified_roles == ["message", "session"];
    let missing: Vec<&str> = ["message", "session"]
        .into_iter()
        .filter(|role| !verified_roles.iter().any(|value| value == role))
        .collect();
    KeyValidationResult {
        valid,
        required_roles: vec!["message".into(), "session".into()],
        verified_roles,
        modes,
        reason: if valid {
            String::new()
        } else {
            format!("unverified_roles:{}", missing.join(","))
        },
    }
}

fn first_file(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|path| path.is_file()).cloned()
}

fn list_matching<F>(directory: &Path, predicate: F) -> Vec<PathBuf>
where
    F: Fn(&str) -> bool,
{
    let mut paths: Vec<PathBuf> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if path.is_file() && predicate(&name) {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    paths.sort();
    paths
}

fn read_first_page(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; PAGE_SIZE];
    let mut read = 0;
    while read < PAGE_SIZE {
        let count = file.read(&mut buffer[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    buffer.truncate(read);
    Ok(buffer)
}

pub fn read_page1(path: &Path) -> Result<Vec<u8>> {
    read_first_page(path)
}
