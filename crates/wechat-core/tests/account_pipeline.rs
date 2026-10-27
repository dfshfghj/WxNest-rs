use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use wechat_core::account::{
    decrypt_account, decrypt_account_with_options, AccountDecryptOptions, SOURCE_INFO_FILE,
};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sqlcipher")
}

fn key_hex() -> String {
    hex::encode((1u8..=32).collect::<Vec<_>>())
}

fn copy(source: &Path, destination: &Path) {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::copy(source, destination).expect("copy fixture");
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).expect("read file")
}

#[test]
fn decrypts_account_directory_into_flat_layout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input = temp.path().join("wxid_pipeline");
    copy(
        &fixtures().join("encrypted_raw.db"),
        &input.join("message_0.db"),
    );
    copy(
        &fixtures().join("encrypted_passphrase.db"),
        &input.join("session.db"),
    );

    let output_root = temp.path().join("out");
    let report = decrypt_account(&input, &output_root, &key_hex());
    assert_eq!(report.status, "success", "{}", report.message);
    assert_eq!(report.account, "wxid_pipeline");
    assert_eq!(report.total_databases, 2);
    assert_eq!(report.successful_count, 2);
    assert_eq!(report.failed_count, 0);
    assert_eq!(report.skipped_count, 0);
    assert_eq!(report.processed_files.len(), 2);
    assert!(report.error.is_empty());

    let output = output_root.join("wxid_pipeline");
    assert_eq!(
        read(&output.join("message_0.db")),
        read(&fixtures().join("expected_raw.db"))
    );
    assert_eq!(
        read(&output.join("session.db")),
        read(&fixtures().join("expected_passphrase.db"))
    );

    let source_info: Value =
        serde_json::from_slice(&read(&output.join(SOURCE_INFO_FILE))).expect("source info");
    assert!(source_info["db_storage_path"]
        .as_str()
        .unwrap()
        .ends_with("wxid_pipeline"));

    let modes = report
        .databases
        .iter()
        .map(|item| {
            item.detail
                .as_ref()
                .map(|detail| detail.key_mode.clone())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    assert!(modes.contains(&"raw_enc_key".to_string()));
    assert!(modes.contains(&"sqlcipher_passphrase".to_string()));
}

#[test]
fn merges_wal_and_can_skip_existing_outputs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input = temp.path().join("wxid_wal");
    copy(
        &fixtures().join("encrypted_wal.db"),
        &input.join("message_0.db"),
    );
    copy(
        &fixtures().join("encrypted_wal.db-wal"),
        &input.join("message_0.db-wal"),
    );
    copy(
        &fixtures().join("encrypted_raw.db"),
        &input.join("session.db"),
    );

    let output_root = temp.path().join("out");
    let output = output_root.join("wxid_wal");
    fs::create_dir_all(&output).expect("output dir");
    fs::write(output.join("session.db"), b"sentinel").expect("sentinel");

    let report = decrypt_account_with_options(
        &input,
        &output_root,
        &key_hex(),
        &AccountDecryptOptions {
            skip_existing: true,
        },
    );
    assert_eq!(report.status, "success", "{}", report.message);
    assert_eq!(report.successful_count, 1);
    assert_eq!(report.skipped_count, 1);
    assert_eq!(report.failed_count, 0);
    assert_eq!(read(&output.join("session.db")), b"sentinel");
    assert_eq!(
        read(&output.join("message_0.db")),
        read(&fixtures().join("expected_wal.db"))
    );

    let wal = report
        .databases
        .iter()
        .find(|item| item.db_name == "message_0.db")
        .and_then(|item| item.detail.as_ref())
        .expect("wal detail");
    assert_eq!(wal.wal.committed_frames, 1);
    assert!(wal.success);
}

#[test]
fn supports_nested_db_storage_layout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input = temp.path().join("wxid_nested").join("db_storage");
    copy(
        &fixtures().join("encrypted_raw.db"),
        &input.join("message/message_0.db"),
    );
    copy(
        &fixtures().join("encrypted_passphrase.db"),
        &input.join("session/session.db"),
    );

    let output_root = temp.path().join("out");
    let report = decrypt_account(
        temp.path().join("wxid_nested").as_path(),
        &output_root,
        &key_hex(),
    );
    assert_eq!(report.status, "success", "{}", report.message);
    assert_eq!(report.account, "wxid_nested");
    assert_eq!(report.total_databases, 2);

    let output = output_root.join("wxid_nested");
    assert_eq!(
        read(&output.join("message_0.db")),
        read(&fixtures().join("expected_raw.db"))
    );
    assert_eq!(
        read(&output.join("session.db")),
        read(&fixtures().join("expected_passphrase.db"))
    );
}

#[test]
fn multi_account_root_is_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("wechat_files");
    copy(
        &fixtures().join("encrypted_raw.db"),
        &root.join("wxid_one/db_storage/message_0.db"),
    );
    copy(
        &fixtures().join("encrypted_raw.db"),
        &root.join("wxid_two/db_storage/message_0.db"),
    );

    let report = decrypt_account(&root, &temp.path().join("out"), &key_hex());
    assert_eq!(report.status, "error");
    assert_eq!(report.detected_accounts.len(), 2);
    assert!(report.message.contains("多个账号"), "{}", report.message);
}

#[test]
fn wrong_key_reports_per_database_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input = temp.path().join("wxid_bad");
    copy(
        &fixtures().join("encrypted_raw.db"),
        &input.join("message_0.db"),
    );

    let report = decrypt_account(&input, &temp.path().join("out"), &"ff".repeat(32));
    assert_eq!(report.status, "error");
    assert_eq!(report.failed_count, 1);
    assert_eq!(report.successful_count, 0);
    let outcome = &report.databases[0];
    assert!(!outcome.success);
    assert!(outcome.error.contains("key_mismatch"), "{}", outcome.error);
}

#[test]
fn plain_sqlite_is_copied() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input = temp.path().join("wxid_plain");
    copy(&fixtures().join("plain.db"), &input.join("message_0.db"));

    let report = decrypt_account(&input, &temp.path().join("out"), &key_hex());
    assert_eq!(report.status, "success", "{}", report.message);
    let detail = report.databases[0].detail.as_ref().expect("detail");
    assert!(detail.copied_as_sqlite);
    assert_eq!(
        read(&temp.path().join("out/wxid_plain/message_0.db")),
        read(&fixtures().join("plain.db"))
    );
}

#[test]
fn duplicate_names_use_nested_output_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input = temp.path().join("wxid_dupes");
    copy(
        &fixtures().join("encrypted_raw.db"),
        &input.join("a/message_0.db"),
    );
    copy(
        &fixtures().join("encrypted_raw.db"),
        &input.join("b/message_0.db"),
    );

    let report = decrypt_account(&input, &temp.path().join("out"), &key_hex());
    assert_eq!(report.status, "success", "{}", report.message);
    assert_eq!(report.successful_count, 2);
    let output = temp.path().join("out/wxid_dupes");
    assert!(output.join("_nested/a/message_0.db").is_file());
    assert!(output.join("_nested/b/message_0.db").is_file());
}
