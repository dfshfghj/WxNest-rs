use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::DecryptResult;
use crate::sqlcipher::decryptor::decrypt_database;
use crate::sqlcipher::filters::should_skip_source_database;
use crate::sqlcipher::key::parse_key_hex;
use crate::sqlcipher::scan::{
    derive_account_name_from_path, scan_account_databases_from_path, DetectedAccount, ScanResult,
};

pub const SOURCE_INFO_FILE: &str = "_source.json";

#[derive(Debug, Clone, Default)]
pub struct AccountDecryptOptions {
    pub skip_existing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountDatabaseOutcome {
    pub db_name: String,
    pub source_path: String,
    pub output_path: String,
    pub success: bool,
    pub skipped: bool,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<DecryptResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountDecryptReport {
    pub status: String,
    pub message: String,
    pub account: String,
    pub source_db_storage_path: String,
    pub source_wxid_dir: String,
    pub output_directory: String,
    pub total_databases: usize,
    pub successful_count: usize,
    pub skipped_count: usize,
    pub failed_count: usize,
    pub diagnostic_warning_count: usize,
    pub processed_files: Vec<String>,
    pub failed_files: Vec<String>,
    pub warnings: Vec<String>,
    pub databases: Vec<AccountDatabaseOutcome>,
    pub detected_accounts: Vec<DetectedAccount>,
    pub error: String,
}

impl AccountDecryptReport {
    fn error(message: impl Into<String>, detected_accounts: Vec<DetectedAccount>) -> Self {
        let message = message.into();
        Self {
            status: "error".into(),
            message: message.clone(),
            account: String::new(),
            source_db_storage_path: String::new(),
            source_wxid_dir: String::new(),
            output_directory: String::new(),
            total_databases: 0,
            successful_count: 0,
            skipped_count: 0,
            failed_count: 0,
            diagnostic_warning_count: 0,
            processed_files: Vec::new(),
            failed_files: Vec::new(),
            warnings: Vec::new(),
            databases: Vec::new(),
            detected_accounts,
            error: message,
        }
    }

    fn fatal_account(
        message: impl Into<String>,
        account: &LocatedAccount,
        output_directory: &Path,
    ) -> Self {
        let message = message.into();
        Self {
            status: "error".into(),
            message: message.clone(),
            account: account.account.clone(),
            source_db_storage_path: account.root.display().to_string(),
            source_wxid_dir: account.wxid_dir.clone(),
            output_directory: output_directory.display().to_string(),
            total_databases: account.databases.len(),
            successful_count: 0,
            skipped_count: 0,
            failed_count: account.databases.len(),
            diagnostic_warning_count: 0,
            processed_files: Vec::new(),
            failed_files: account
                .databases
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            warnings: Vec::new(),
            databases: Vec::new(),
            detected_accounts: account.detected_accounts.clone(),
            error: message,
        }
    }
}

struct LocatedAccount {
    account: String,
    root: PathBuf,
    databases: Vec<PathBuf>,
    wxid_dir: String,
    detected_accounts: Vec<DetectedAccount>,
}

pub fn decrypt_account(input: &Path, output_root: &Path, key_hex: &str) -> AccountDecryptReport {
    decrypt_account_with_options(
        input,
        output_root,
        key_hex,
        &AccountDecryptOptions::default(),
    )
}

pub fn decrypt_account_with_options(
    input: &Path,
    output_root: &Path,
    key_hex: &str,
    options: &AccountDecryptOptions,
) -> AccountDecryptReport {
    let located = match locate_account(input) {
        Ok(value) => value,
        Err(scan) => return AccountDecryptReport::error(scan.message, scan.detected_accounts),
    };
    let output_directory = output_root.join(&located.account);

    if let Err(error) = parse_key_hex(key_hex) {
        return AccountDecryptReport::fatal_account(error.to_string(), &located, &output_directory);
    }
    if same_directory(&located.root, &output_directory) {
        return AccountDecryptReport::fatal_account(
            "输出目录不能与源账号目录相同",
            &located,
            &output_directory,
        );
    }
    if let Err(error) = fs::create_dir_all(&output_directory) {
        return AccountDecryptReport::fatal_account(
            format!("创建输出目录失败: {error}"),
            &located,
            &output_directory,
        );
    }

    let mut warnings = Vec::new();
    if let Err(error) = write_source_info(&located, &output_directory) {
        warnings.push(format!("写入 {SOURCE_INFO_FILE} 失败: {error}"));
    }

    let duplicates = duplicate_file_names(&located.databases);
    let mut outcomes = Vec::with_capacity(located.databases.len());
    let mut processed_files = Vec::new();
    let mut failed_files = Vec::new();
    let mut successful_count = 0;
    let mut skipped_count = 0;
    let mut failed_count = 0;
    let mut diagnostic_warning_count = 0;

    for source in &located.databases {
        let db_name = file_name(source);
        let output_path = output_path_for(source, &located, &output_directory, &duplicates);
        if let Some(parent) = output_path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                failed_count += 1;
                failed_files.push(source.display().to_string());
                outcomes.push(AccountDatabaseOutcome {
                    db_name,
                    source_path: source.display().to_string(),
                    output_path: output_path.display().to_string(),
                    success: false,
                    skipped: false,
                    error: format!("创建输出目录失败: {error}"),
                    detail: None,
                });
                continue;
            }
        }

        if options.skip_existing && output_path.is_file() {
            skipped_count += 1;
            outcomes.push(AccountDatabaseOutcome {
                db_name,
                source_path: source.display().to_string(),
                output_path: output_path.display().to_string(),
                success: true,
                skipped: true,
                error: String::new(),
                detail: None,
            });
            continue;
        }

        let result = decrypt_database(source, &output_path, key_hex);
        if result.failed_pages > 0
            || result.hmac_warning_pages > 0
            || result.diagnostic_status != "ok"
        {
            diagnostic_warning_count += 1;
        }
        if result.success {
            successful_count += 1;
            processed_files.push(result.output_path.clone());
        } else {
            failed_count += 1;
            failed_files.push(source.display().to_string());
        }
        outcomes.push(AccountDatabaseOutcome {
            db_name,
            source_path: source.display().to_string(),
            output_path: result.output_path.clone(),
            success: result.success,
            skipped: false,
            error: result.error.clone(),
            detail: Some(result),
        });
    }

    let total_databases = located.databases.len();
    let status = if failed_count == 0 && successful_count + skipped_count > 0 {
        "success"
    } else if successful_count + skipped_count > 0 {
        "partial"
    } else {
        "error"
    };
    let message = summary_message(
        &located.account,
        total_databases,
        successful_count,
        skipped_count,
        failed_count,
        diagnostic_warning_count,
    );
    AccountDecryptReport {
        status: status.to_string(),
        message,
        account: located.account,
        source_db_storage_path: located.root.display().to_string(),
        source_wxid_dir: located.wxid_dir,
        output_directory: output_directory.display().to_string(),
        total_databases,
        successful_count,
        skipped_count,
        failed_count,
        diagnostic_warning_count,
        processed_files,
        failed_files,
        warnings,
        databases: outcomes,
        detected_accounts: located.detected_accounts,
        error: String::new(),
    }
}

fn locate_account(input: &Path) -> std::result::Result<LocatedAccount, ScanResult> {
    let scan = scan_account_databases_from_path(input);
    if scan.status == "success" {
        if let Some((account, databases)) = scan.account_databases.iter().next() {
            let source = scan.account_sources.get(account);
            let root = source
                .map(|value| PathBuf::from(&value.db_storage_path))
                .or_else(|| {
                    databases
                        .first()
                        .and_then(|db| Path::new(&db.path).parent().map(Path::to_path_buf))
                })
                .unwrap_or_else(|| input.to_path_buf());
            return Ok(LocatedAccount {
                account: account.clone(),
                root,
                databases: databases.iter().map(|db| PathBuf::from(&db.path)).collect(),
                wxid_dir: source
                    .map(|value| value.wxid_dir.clone())
                    .unwrap_or_default(),
                detected_accounts: scan.detected_accounts.clone(),
            });
        }
    }

    if input.is_dir() && scan.detected_accounts.len() <= 1 {
        let root = fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf());
        let mut databases = Vec::new();
        collect_database_files(&root, &mut databases);
        databases.sort();
        if !databases.is_empty() {
            let account = root
                .file_name()
                .map(|value| value.to_string_lossy().trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| derive_account_name_from_path(&root));
            return Ok(LocatedAccount {
                account,
                root: root.clone(),
                databases,
                wxid_dir: root
                    .parent()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                detected_accounts: scan.detected_accounts,
            });
        }
    }
    Err(scan)
}

fn collect_database_files(directory: &Path, databases: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_database_files(&path, databases);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|value| value.eq_ignore_ascii_case("db"))
            && !should_skip_source_database(&path)
        {
            databases.push(path);
        }
    }
}

fn duplicate_file_names(databases: &[PathBuf]) -> HashSet<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in databases {
        *counts.entry(file_name(path).to_lowercase()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect()
}

fn output_path_for(
    source: &Path,
    located: &LocatedAccount,
    output_directory: &Path,
    duplicates: &HashSet<String>,
) -> PathBuf {
    let name = file_name(source);
    if duplicates.contains(&name.to_lowercase()) {
        let relative = source
            .strip_prefix(&located.root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| PathBuf::from(&name));
        output_directory.join("_nested").join(relative)
    } else {
        output_directory.join(name)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn same_directory(left: &Path, right: &Path) -> bool {
    let Ok(left) = fs::canonicalize(left) else {
        return false;
    };
    let Ok(right) = fs::canonicalize(right) else {
        return false;
    };
    left == right
}

fn write_source_info(located: &LocatedAccount, output_directory: &Path) -> std::io::Result<()> {
    let payload = serde_json::json!({
        "db_storage_path": located.root.display().to_string(),
        "wxid_dir": located.wxid_dir,
    });
    let data = serde_json::to_vec_pretty(&payload).unwrap_or_default();
    fs::write(output_directory.join(SOURCE_INFO_FILE), data)
}

fn summary_message(
    account: &str,
    total: usize,
    successful: usize,
    skipped: usize,
    failed: usize,
    warnings: usize,
) -> String {
    let mut message = format!("账号 {account} 共 {total} 个数据库，成功 {successful} 个");
    if skipped > 0 {
        message.push_str(&format!("，跳过 {skipped} 个"));
    }
    if failed > 0 {
        message.push_str(&format!("，失败 {failed} 个"));
    }
    if warnings > 0 {
        message.push_str(&format!("，{warnings} 个数据库存在诊断警告"));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_includes_counts() {
        assert_eq!(
            summary_message("wxid_test", 3, 2, 1, 0, 0),
            "账号 wxid_test 共 3 个数据库，成功 2 个，跳过 1 个"
        );
        assert_eq!(
            summary_message("wxid_test", 2, 0, 0, 2, 1),
            "账号 wxid_test 共 2 个数据库，成功 0 个，失败 2 个，1 个数据库存在诊断警告"
        );
    }

    #[test]
    fn duplicate_names_are_routed_to_nested_paths() {
        let databases = vec![
            PathBuf::from("/root/a/message_0.db"),
            PathBuf::from("/root/b/message_0.db"),
            PathBuf::from("/root/session.db"),
        ];
        let duplicates = duplicate_file_names(&databases);
        assert!(duplicates.contains("message_0.db"));
        assert!(!duplicates.contains("session.db"));
        let located = LocatedAccount {
            account: "wxid_test".into(),
            root: PathBuf::from("/root"),
            databases: databases.clone(),
            wxid_dir: String::new(),
            detected_accounts: Vec::new(),
        };
        let output = Path::new("/out");
        assert_eq!(
            output_path_for(&databases[0], &located, output, &duplicates),
            PathBuf::from("/out/_nested/a/message_0.db")
        );
        assert_eq!(
            output_path_for(&databases[2], &located, output, &duplicates),
            PathBuf::from("/out/session.db")
        );
    }
}
