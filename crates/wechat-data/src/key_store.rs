use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;
use wechat_core::sqlcipher::{read_page1, resolve_page1_key_material, KeyMode};

use crate::error::{DataError, Result};

pub struct KeyStore {
    entries: Vec<(String, String)>,
    image_entries: Vec<(String, ImageKeys)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageKeys {
    pub aes_key: String,
    pub xor_key: Option<u8>,
}

impl KeyStore {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| DataError::Decode(format!("解析 key store 失败: {error}")))?;
        let map = value
            .get("accounts")
            .and_then(Value::as_object)
            .or_else(|| value.as_object())
            .ok_or_else(|| DataError::Decode("key store 结构不是对象".into()))?;

        let mut entries = Vec::new();
        let mut image_entries = Vec::new();
        for (name, entry) in map {
            let mut names = vec![name.clone()];
            if let Some(wxid_dir) = entry.get("db_key_source_wxid_dir").and_then(Value::as_str) {
                if let Some(base) = Path::new(wxid_dir).file_name() {
                    names.push(base.to_string_lossy().to_string());
                }
            }
            if let Some(wxid_dir) = entry
                .get("image_key_source_wxid_dir")
                .and_then(Value::as_str)
            {
                if let Some(base) = Path::new(wxid_dir).file_name() {
                    names.push(base.to_string_lossy().to_string());
                }
            }
            for alias in entry
                .get("aliases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                names.push(alias.to_string());
            }
            names.sort();
            names.dedup();

            if let Some(keys) = image_keys_from_entry(entry) {
                for name in &names {
                    image_entries.push((name.clone(), keys.clone()));
                }
            }

            let Some(key) = entry.get("db_key").and_then(Value::as_str) else {
                continue;
            };
            if !is_valid_key(key) {
                continue;
            }
            let key = key.to_ascii_lowercase();
            for name in names {
                entries.push((name, key.clone()));
            }
        }
        Ok(Self {
            entries,
            image_entries,
        })
    }

    pub fn key_for_account(&self, account: &str) -> Option<String> {
        if account.trim().is_empty() {
            return None;
        }
        for candidate in account_candidates(account) {
            if let Some((_, key)) = self
                .entries
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&candidate))
            {
                return Some(key.clone());
            }
        }
        None
    }

    pub fn image_keys_for_account(&self, account: &str) -> Option<ImageKeys> {
        if account.trim().is_empty() {
            return None;
        }
        for candidate in account_candidates(account) {
            if let Some((_, keys)) = self
                .image_entries
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&candidate))
            {
                return Some(keys.clone());
            }
        }
        None
    }

    pub fn unique_key_accounts(&self) -> Vec<(String, String)> {
        let mut seen = HashSet::new();
        let mut output = Vec::new();
        for (name, key) in &self.entries {
            if seen.insert(key.clone()) {
                output.push((name.clone(), key.clone()));
            }
        }
        output
    }

    pub fn all_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.entries.iter().map(|(_, key)| key.clone()).collect();
        keys.sort();
        keys.dedup();
        keys
    }
}

pub fn find_image_keys_for_db_storage(
    store_path: &Path,
    db_storage: &Path,
) -> Result<Option<ImageKeys>> {
    let store = KeyStore::load(store_path)?;
    for account in account_from_db_storage(db_storage) {
        if let Some(keys) = store.image_keys_for_account(&account) {
            return Ok(Some(keys));
        }
    }
    Ok(None)
}

fn image_keys_from_entry(entry: &Value) -> Option<ImageKeys> {
    let aes_key = entry.get("image_aes_key").and_then(Value::as_str)?;
    let aes_key = aes_key.trim();
    let aes_key = if aes_key.len() == 32 && aes_key.chars().all(|c| c.is_ascii_hexdigit()) {
        let raw = hex::decode(aes_key).ok()?;
        String::from_utf8(raw).ok()?
    } else if aes_key.len() == 16 {
        aes_key.to_string()
    } else {
        return None;
    };
    let xor_key = entry.get("image_xor_key").and_then(parse_xor_key);
    Some(ImageKeys { aes_key, xor_key })
}

fn parse_xor_key(value: &Value) -> Option<u8> {
    if let Some(number) = value.as_u64() {
        return u8::try_from(number).ok();
    }
    let text = value.as_str()?.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u8::from_str_radix(hex, 16).ok();
    }
    if text.chars().any(|c| c.is_ascii_alphabetic()) {
        u8::from_str_radix(text, 16).ok()
    } else {
        text.parse().ok()
    }
}

pub fn account_from_db_storage(db_storage: &Path) -> Vec<String> {
    let account = db_storage
        .parent()
        .and_then(|path| path.file_name())
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            db_storage
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
        })
        .unwrap_or_default();
    account_candidates(&account)
}

pub fn load_key_for_db_storage(store_path: &Path, db_storage: &Path) -> Result<String> {
    find_key_for_db_storage(store_path, db_storage).map(|(key, _)| key)
}

pub fn find_key_for_db_storage(store_path: &Path, db_storage: &Path) -> Result<(String, bool)> {
    find_key_for_db_storage_with_account(store_path, db_storage).map(|(key, raw, _)| (key, raw))
}

pub fn find_key_for_db_storage_with_account(
    store_path: &Path,
    db_storage: &Path,
) -> Result<(String, bool, Option<String>)> {
    let store = KeyStore::load(store_path)?;
    let nested = db_storage.join("session").join("session.db");
    let session = if nested.is_file() {
        nested
    } else {
        db_storage.join("session.db")
    };
    if !session.is_file() {
        return Err(DataError::NotFound(format!(
            "找不到 session.db: {}",
            db_storage.display()
        )));
    }
    let page1 = read_page1(&session)
        .map_err(|error| DataError::Decode(format!("读取 session.db 失败: {error}")))?;

    for account in account_from_db_storage(db_storage) {
        if let Some(key) = store.key_for_account(&account) {
            let raw_key = hex::decode(&key)
                .ok()
                .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                .and_then(|key_bytes| resolve_page1_key_material(&key_bytes, &page1))
                .is_some_and(|resolved| matches!(resolved.mode, KeyMode::RawEncKey));
            return Ok((key, raw_key, Some(account)));
        }
    }

    for (account, key) in store.unique_key_accounts() {
        let Ok(bytes) = hex::decode(&key) else {
            continue;
        };
        let Ok(key_bytes) = <[u8; 32]>::try_from(bytes) else {
            continue;
        };
        if let Some(resolved) = resolve_page1_key_material(&key_bytes, &page1) {
            return Ok((
                key,
                matches!(resolved.mode, KeyMode::RawEncKey),
                Some(account),
            ));
        }
    }
    Err(DataError::NotFound(format!(
        "key store 中没有匹配 {} 的密钥",
        db_storage.display()
    )))
}

fn account_candidates(account: &str) -> Vec<String> {
    let mut output = vec![account.to_string()];
    if let Some((prefix, suffix)) = account.rsplit_once('_') {
        if !prefix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit()) {
            output.push(prefix.to_string());
        }
    }
    output
}

fn is_valid_key(key: &str) -> bool {
    key.len() == 64 && key.chars().all(|character| character.is_ascii_hexdigit())
}
