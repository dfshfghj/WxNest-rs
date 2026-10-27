use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::filters::should_skip_source_database;

#[derive(Debug, Clone, Serialize)]
pub struct AccountDatabase {
    pub path: String,
    pub name: String,
    pub account: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountSource {
    pub db_storage_path: String,
    pub wxid_dir: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DetectedAccount {
    pub account: String,
    pub db_storage_path: String,
    pub wxid_dir: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub status: String,
    pub message: String,
    pub account_databases: BTreeMap<String, Vec<AccountDatabase>>,
    pub account_sources: BTreeMap<String, AccountSource>,
    pub detected_accounts: Vec<DetectedAccount>,
}

impl ScanResult {
    fn error(message: impl Into<String>) -> Self {
        Self {
            status: "error".into(),
            message: message.into(),
            account_databases: BTreeMap::new(),
            account_sources: BTreeMap::new(),
            detected_accounts: Vec::new(),
        }
    }
}

pub fn scan_account_databases_from_path(db_storage_path: &Path) -> ScanResult {
    if !db_storage_path.exists() {
        return ScanResult::error(format!(
            "指定的数据库路径不存在: {}",
            db_storage_path.display()
        ));
    }

    let roots = resolve_db_storage_roots(db_storage_path);
    if roots.is_empty() {
        return ScanResult::error(
            "未找到微信数据库文件！请确保路径指向具体账号的 db_storage 目录。",
        );
    }

    let detected_accounts: Vec<DetectedAccount> = roots
        .iter()
        .map(|root| DetectedAccount {
            account: derive_account_name_from_path(root),
            db_storage_path: root.display().to_string(),
            wxid_dir: root
                .parent()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        })
        .collect();

    if roots.len() > 1 {
        let names = detected_accounts
            .iter()
            .map(|item| {
                if item.account.is_empty() {
                    item.db_storage_path.clone()
                } else {
                    item.account.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut result = ScanResult::error(format!(
            "检测到多个账号目录，请选择具体账号的 db_storage 目录后再解密，不要直接选择上级目录。当前检测到: {names}"
        ));
        result.detected_accounts = detected_accounts;
        return result;
    }

    let db_root = &roots[0];
    let account_name = derive_account_name_from_path(db_root);
    let mut databases: Vec<AccountDatabase> = Vec::new();
    for path in walk_files(db_root) {
        if !path
            .extension()
            .map(|value| value.to_string_lossy().eq_ignore_ascii_case("db"))
            .unwrap_or(false)
        {
            continue;
        }
        if should_skip_source_database(&path) {
            continue;
        }
        databases.push(AccountDatabase {
            path: path.display().to_string(),
            name: path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_default(),
            account: account_name.clone(),
        });
    }
    databases.sort_by(|left, right| left.path.cmp(&right.path));

    if databases.is_empty() {
        let mut result = ScanResult::error("未找到微信数据库文件！请检查 db_storage_path 是否正确");
        result.detected_accounts = detected_accounts;
        return result;
    }

    let mut account_databases = BTreeMap::new();
    account_databases.insert(account_name.clone(), databases);
    let mut account_sources = BTreeMap::new();
    account_sources.insert(
        account_name,
        AccountSource {
            db_storage_path: db_root.display().to_string(),
            wxid_dir: db_root
                .parent()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        },
    );

    ScanResult {
        status: "success".into(),
        message: String::new(),
        account_databases,
        account_sources,
        detected_accounts,
    }
}

fn resolve_db_storage_roots(storage_path: &Path) -> Vec<PathBuf> {
    let target = std::fs::canonicalize(storage_path).unwrap_or_else(|_| storage_path.to_path_buf());
    if !target.exists() {
        return Vec::new();
    }
    let current = if target.is_dir() {
        target.clone()
    } else {
        target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| target.clone())
    };

    let mut probe = current.clone();
    loop {
        if probe
            .file_name()
            .map(|value| value.to_string_lossy().eq_ignore_ascii_case("db_storage"))
            .unwrap_or(false)
        {
            return vec![probe];
        }
        let Some(parent) = probe.parent() else {
            break;
        };
        if parent == probe {
            break;
        }
        probe = parent.to_path_buf();
    }

    let mut roots = Vec::new();
    collect_db_storage_dirs(&current, &mut roots);
    roots.sort_by(|left, right| {
        left.to_string_lossy()
            .to_lowercase()
            .cmp(&right.to_string_lossy().to_lowercase())
    });
    roots.dedup();
    roots
}

fn collect_db_storage_dirs(directory: &Path, roots: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(directory) {
        Ok(value) => value,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_db_storage = path
            .file_name()
            .map(|value| value.to_string_lossy().eq_ignore_ascii_case("db_storage"))
            .unwrap_or(false);
        if is_db_storage {
            roots.push(path);
        } else {
            collect_db_storage_dirs(&path, roots);
        }
    }
}

fn walk_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(value) => value,
        Err(_) => return files,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else if path.is_file() {
            files.push(path);
        }
    }
    files
}

pub fn normalize_account_name(name: &str) -> String {
    let value = name.trim();
    if value.is_empty() {
        return "unknown_account".into();
    }
    if value.starts_with("wxid_") {
        let parts: Vec<&str> = value.split('_').collect();
        if parts.len() >= 3 {
            let trimmed = parts[..parts.len() - 1].join("_");
            if !trimmed.trim().is_empty() {
                return trimmed;
            }
        }
    }
    value.to_string()
}

pub fn derive_account_name_from_path(path: &Path) -> String {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for part in target.iter() {
        let value = part.to_string_lossy();
        if value.starts_with("wxid_") {
            return normalize_account_name(&value);
        }
    }
    for part in target.iter().rev() {
        let value = part.to_string_lossy().trim().to_string();
        if value.is_empty() || value.eq_ignore_ascii_case("db_storage") || value.len() <= 3 {
            continue;
        }
        return normalize_account_name(&value);
    }
    "unknown_account".into()
}
