use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use wechat_core::crypto::isaac64::{Isaac64, WordFormat};
use wechat_core::image_key::{resolve_local_image_key, scan_v2_templates};
use wechat_core::media::{
    decrypt_dat_v3, decrypt_dat_v4, decrypt_media_bytes, try_xor_decrypt_by_magic, MediaKeys,
    DAT_V1_AES_KEY,
};
use wechat_core::sqlcipher::{decrypt_database, merge_wal_snapshot, validate_realtime_database_key};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn load_json(path: &Path) -> Value {
    serde_json::from_slice(&read(path)).expect("valid fixture json")
}

fn copy_fixture(source: &Path, destination: &Path) {
    fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "copy {} -> {}: {error}",
            source.display(),
            destination.display()
        )
    });
}

#[test]
fn sqlcipher_raw_parity() {
    let root = fixtures().join("sqlcipher");
    let expected = load_json(&root.join("expected.json"));
    let temp = tempfile::tempdir().unwrap();
    let encrypted = temp.path().join("encrypted.db");
    let output = temp.path().join("output.db");
    copy_fixture(&root.join("encrypted_raw.db"), &encrypted);

    let key_hex = hex::encode((1u8..=32).collect::<Vec<_>>());
    let result = decrypt_database(&encrypted, &output, &key_hex);
    let fixture = &expected["raw"];

    assert!(result.success, "error: {}", result.error);
    assert_eq!(result.key_mode, fixture["key_mode"].as_str().unwrap());
    assert_eq!(result.total_pages, fixture["total_pages"].as_u64().unwrap());
    assert_eq!(
        result.successful_pages,
        fixture["successful_pages"].as_u64().unwrap()
    );
    assert_eq!(
        result.failed_pages,
        fixture["failed_pages"].as_u64().unwrap()
    );
    assert_eq!(
        result.hmac_warning_pages,
        fixture["hmac_warning_pages"].as_u64().unwrap()
    );
    assert_eq!(result.diagnostic_status, "ok");
    assert!(result.key_authenticated);
    assert!(!result.copied_as_sqlite);

    let actual = read(&output);
    let expected_bytes = read(&root.join("expected_raw.db"));
    assert_eq!(actual, expected_bytes);
    assert_eq!(
        sha256_hex(&actual),
        fixture["expected_sha256"].as_str().unwrap()
    );
    assert_eq!(result.output_size, actual.len() as u64);
    assert_eq!(
        result.output_header_debug["reserved_space"].as_u64(),
        Some(80)
    );
}

#[test]
fn sqlcipher_passphrase_parity() {
    let root = fixtures().join("sqlcipher");
    let expected = load_json(&root.join("expected.json"));
    let temp = tempfile::tempdir().unwrap();
    let encrypted = temp.path().join("encrypted.db");
    let output = temp.path().join("output.db");
    copy_fixture(&root.join("encrypted_passphrase.db"), &encrypted);

    let key_hex = hex::encode((1u8..=32).collect::<Vec<_>>());
    let result = decrypt_database(&encrypted, &output, &key_hex);
    let fixture = &expected["passphrase"];

    assert!(result.success, "error: {}", result.error);
    assert_eq!(result.key_mode, "sqlcipher_passphrase");
    assert_eq!(result.total_pages, fixture["total_pages"].as_u64().unwrap());
    assert_eq!(result.diagnostic_status, "ok");

    let actual = read(&output);
    assert_eq!(actual, read(&root.join("expected_passphrase.db")));
    assert_eq!(
        sha256_hex(&actual),
        fixture["expected_sha256"].as_str().unwrap()
    );
}

#[test]
fn sqlcipher_wrong_key_parity() {
    let root = fixtures().join("sqlcipher");
    let expected = load_json(&root.join("expected.json"));
    let temp = tempfile::tempdir().unwrap();
    let encrypted = temp.path().join("encrypted.db");
    let output = temp.path().join("output.db");
    copy_fixture(&root.join("encrypted_raw.db"), &encrypted);

    let result = decrypt_database(&encrypted, &output, &("ff".repeat(32)));
    let fixture = &expected["wrong_key"];

    assert!(!result.success);
    assert_eq!(result.error, fixture["error"].as_str().unwrap());
    assert_eq!(
        result.failed_pages,
        fixture["failed_pages"].as_u64().unwrap()
    );
    assert_eq!(result.key_mode, "");
    assert!(!output.exists());
}

#[test]
fn sqlcipher_encrypted_wal_parity() {
    let root = fixtures().join("sqlcipher");
    let expected = load_json(&root.join("expected.json"));
    let temp = tempfile::tempdir().unwrap();
    let encrypted = temp.path().join("encrypted.db");
    let output = temp.path().join("output.db");
    copy_fixture(&root.join("encrypted_wal.db"), &encrypted);
    copy_fixture(
        &root.join("encrypted_wal.db-wal"),
        &temp.path().join("encrypted.db-wal"),
    );

    let key_hex = hex::encode((1u8..=32).collect::<Vec<_>>());
    let result = decrypt_database(&encrypted, &output, &key_hex);
    let fixture = &expected["wal"];

    assert!(result.success, "error: {}", result.error);
    assert_eq!(result.wal.committed_frames, 1);
    assert_eq!(
        result.wal.applied_pages as u64,
        fixture["wal"]["applied_pages"].as_u64().unwrap()
    );
    assert_eq!(
        result.wal.database_pages as u64,
        fixture["wal"]["database_pages"].as_u64().unwrap()
    );
    assert_eq!(
        result.wal.ignored_tail_bytes as u64,
        fixture["wal"]["ignored_tail_bytes"].as_u64().unwrap()
    );
    let actual = read(&output);
    assert_eq!(actual, read(&root.join("expected_wal.db")));
    assert_eq!(
        sha256_hex(&actual),
        fixture["expected_sha256"].as_str().unwrap()
    );
}

#[test]
fn sqlcipher_plain_copy_parity() {
    let root = fixtures().join("sqlcipher");
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("plain.db");
    let output = temp.path().join("output.db");
    copy_fixture(&root.join("plain.db"), &source);

    let result = decrypt_database(&source, &output, &hex::encode([0u8; 32]));
    assert!(result.success, "error: {}", result.error);
    assert!(result.copied_as_sqlite);
    assert!(!result.key_authenticated);
    assert_eq!(result.diagnostic_status, "ok");
    assert_eq!(read(&source), read(&output));
}

#[test]
fn wal_parity() {
    let root = fixtures().join("wal");
    let expected = load_json(&root.join("expected.json"));
    let base = read(&root.join("base.db"));
    let wal = read(&root.join("wal.bin"));
    let (merged, info) = merge_wal_snapshot(&base, &wal, 4096, None).unwrap();

    assert_eq!(merged, read(&root.join("expected.db")));
    assert_eq!(
        sha256_hex(&merged),
        expected["expected_sha256"].as_str().unwrap()
    );
    assert_eq!(
        info.database_pages as u64,
        expected["database_pages"].as_u64().unwrap()
    );
    assert_eq!(
        info.committed_frames as u64,
        expected["committed_frames"].as_u64().unwrap()
    );
    assert_eq!(
        info.applied_pages as u64,
        expected["applied_pages"].as_u64().unwrap()
    );
    assert_eq!(
        info.ignored_tail_bytes as u64,
        expected["ignored_tail_bytes"].as_u64().unwrap()
    );
}

#[test]
fn media_dat_parity() {
    let root = fixtures().join("media");
    let expected = load_json(&root.join("expected.json"));
    let xor_key = expected["dat_v3"]["xor_key"].as_u64().unwrap() as u8;

    let v3 = decrypt_dat_v3(&read(&root.join("dat_v3.bin")), xor_key);
    let v3_expected = read(&root.join("dat_v3_expected.bin"));
    assert_eq!(v3, v3_expected);
    assert_eq!(
        sha256_hex(&v3),
        expected["dat_v3"]["expected_sha256"].as_str().unwrap()
    );

    let v1 = decrypt_dat_v4(&read(&root.join("dat_v4_v1.bin")), xor_key, DAT_V1_AES_KEY).unwrap();
    assert_eq!(v1, read(&root.join("dat_v4_v1_expected.bin")));

    let v2_key = expected["dat_v4_v2"]["aes_key"]
        .as_str()
        .unwrap()
        .as_bytes();
    let v2 = decrypt_dat_v4(&read(&root.join("dat_v4_v2.bin")), xor_key, v2_key).unwrap();
    assert_eq!(v2, read(&root.join("dat_v4_v2_expected.bin")));

    let keys = MediaKeys {
        xor_key: Some(xor_key),
        aes_key: Some(v2_key.to_vec()),
    };
    let high = decrypt_media_bytes(&read(&root.join("dat_v4_v2.bin")), &keys, Some(2), None);
    assert_eq!(high.data, read(&root.join("dat_v4_v2_expected.bin")));
    assert_eq!(high.media_type, "application/octet-stream");
}

#[test]
fn media_xor_and_emoticon_parity() {
    let root = fixtures().join("media");
    let expected = load_json(&root.join("expected.json"));
    let xor_key = expected["xor_png"]["xor_key"].as_u64().unwrap() as u8;

    let xor_input = read(&root.join("xor_png.bin"));
    let xor_expected = read(&root.join("xor_png_expected.bin"));
    let (decoded, media_type) = try_xor_decrypt_by_magic(&xor_input, None);
    assert_eq!(decoded.unwrap(), xor_expected);
    assert_eq!(media_type, Some("image/png"));
    assert_eq!(
        sha256_hex(&xor_expected),
        expected["xor_png"]["expected_sha256"].as_str().unwrap()
    );

    let high = decrypt_media_bytes(
        &xor_input,
        &MediaKeys {
            xor_key: Some(xor_key),
            aes_key: None,
        },
        None,
        None,
    );
    assert_eq!(high.data, xor_expected);
    assert_eq!(high.media_type, "image/png");
    assert_eq!(high.dat_version, Some(0));
    assert!(!high.used_magic_fallback);

    let emoticon_key = expected["emoticon"]["aes_key_hex"].as_str().unwrap();
    let emoticon = wechat_core::media::decrypt_emoticon_aes_cbc(
        &read(&root.join("emoticon.bin")),
        emoticon_key,
    )
    .unwrap();
    assert_eq!(emoticon, read(&root.join("emoticon_expected.bin")));
}

#[test]
fn image_key_parity() {
    let root = fixtures().join("image_key");
    let expected = load_json(&root.join("expected.json"));
    let account_dir = root.join("account");
    let kvcomm_dir = root.join("kvcomm");

    let scan = scan_v2_templates(&account_dir, 32, 500);
    assert_eq!(scan.templates.len(), 1);
    assert_eq!(
        scan.inferred_xor_key.map(u64::from),
        expected["inferred_xor_key"].as_u64()
    );

    let resolution = resolve_local_image_key(
        &kvcomm_dir,
        &account_dir,
        Some("wxid_test_1234"),
        None,
        &[],
        Some(scan),
        32,
        500,
    )
    .expect("fixture must resolve");

    assert_eq!(resolution.code as u64, expected["code"].as_u64().unwrap());
    assert_eq!(resolution.wxid, expected["wxid"].as_str().unwrap());
    assert_eq!(
        resolution.xor_key as u64,
        expected["xor_key"].as_u64().unwrap()
    );
    assert_eq!(
        resolution.xor_key_hex,
        expected["xor_key_hex"].as_str().unwrap()
    );
    assert_eq!(resolution.aes_key, expected["aes_key"].as_str().unwrap());
    assert!(resolution.verified);
    assert_eq!(
        Path::new(&resolution.template_path)
            .file_name()
            .unwrap()
            .to_string_lossy(),
        expected["template_path"].as_str().unwrap()
    );
    assert_eq!(
        resolution.inferred_xor_key.map(u64::from),
        expected["inferred_xor_key"].as_u64()
    );
}

#[test]
fn isaac64_parity() {
    let root = fixtures().join("isaac64");
    let vectors = load_json(&root.join("expected.json"));
    for vector in vectors.as_array().unwrap() {
        let seed = vector["seed"].as_str().unwrap();
        let size = vector["size"].as_u64().unwrap() as usize;
        let format: WordFormat = vector["format"].as_str().unwrap().parse().unwrap();
        let mut isaac = Isaac64::new(seed);
        let actual = isaac.generate_keystream(size, format);
        assert_eq!(
            hex::encode(actual),
            vector["expected_hex"].as_str().unwrap(),
            "seed={seed:?} size={size} format={}",
            vector["format"].as_str().unwrap()
        );
    }
}

#[test]
fn key_role_validation_parity() {
    let root = fixtures().join("sqlcipher");
    let expected = load_json(&root.join("expected.json"));
    let temp = tempfile::tempdir().unwrap();
    let desktop = temp.path().join("db_storage");
    fs::create_dir_all(desktop.join("message")).unwrap();
    fs::create_dir_all(desktop.join("session")).unwrap();
    copy_fixture(
        &root.join("encrypted_raw.db"),
        &desktop.join("message").join("message_0.db"),
    );
    copy_fixture(
        &root.join("encrypted_raw.db"),
        &desktop.join("session").join("session.db"),
    );

    let key_hex = hex::encode((1u8..=32).collect::<Vec<_>>());
    let result = validate_realtime_database_key(&desktop, &key_hex);
    assert!(result.valid, "{result:?}");
    assert_eq!(result.verified_roles, vec!["message", "session"]);
    assert_eq!(
        result.modes.get("message").map(String::as_str),
        Some(expected["raw"]["key_mode"].as_str().unwrap())
    );
}
