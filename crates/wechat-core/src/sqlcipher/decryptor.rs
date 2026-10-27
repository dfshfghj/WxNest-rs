use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
#[cfg(not(unix))]
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tempfile::NamedTempFile;

use crate::crypto::aes::aes_cbc_decrypt_256;
use crate::crypto::kdf::hmac_sha512_parts;
use crate::error::{CoreError, Result};
use crate::model::{DecryptResult, Diagnostics, FileSnapshot, InputLayout, PageSample, SiblingSnapshot};

use super::diagnostics::{collect_sqlite_diagnostics, repair_sqlite_indexes, sqlite_diagnostics_status};
use super::key::{compute_page_hmac, parse_key_hex, resolve_page1_key_material, ResolvedKey};
use super::wal::merge_wal_snapshot;
use super::{HMAC_SIZE, IV_SIZE, KEY_SIZE, PAGE_SIZE, RESERVE_SIZE, SALT_SIZE, SQLITE_HEADER};

const JOURNAL_MAGIC: [u8; 8] = [0xD9, 0xD5, 0x05, 0xF9, 0x20, 0xA1, 0x63, 0xD7];

pub fn decrypt_database(db_path: &Path, output_path: &Path, key_hex: &str) -> DecryptResult {
    let mut result = DecryptResult::new(db_path, output_path);
    let key_material = match parse_key_hex(key_hex) {
        Ok(value) => value,
        Err(error) => {
            result.error = error.to_string();
            return result;
        }
    };

    if same_file(db_path, output_path) {
        result.error = "解密输出不能覆盖源数据库".into();
        return result;
    }

    result.source_snapshot_before = safe_file_snapshot(db_path);
    let read_start = Instant::now();
    let source = match fs::read(db_path) {
        Ok(value) => value,
        Err(error) => {
            result.source_snapshot_after = safe_file_snapshot(db_path);
            result.error = error.to_string();
            return result;
        }
    };
    let wal_data = read_optional(&wal_path(db_path));
    if let Some(magic) = read_journal_magic(db_path) {
        result.read_ms = elapsed_ms(read_start);
        result.source_snapshot_after = safe_file_snapshot(db_path);
        result.source_changed_during_read = snapshots_changed(
            &result.source_snapshot_before,
            &result.source_snapshot_after,
        );
        if magic == JOURNAL_MAGIC {
            result.error = "源数据库存在待恢复的回滚日志，请先通过微信完成恢复".into();
            return result;
        }
    }
    result.read_ms = elapsed_ms(read_start);
    result.source_snapshot_after = safe_file_snapshot(db_path);
    result.source_changed_during_read = snapshots_changed(
        &result.source_snapshot_before,
        &result.source_snapshot_after,
    );
    if result.source_changed_during_read {
        result.error = "源数据库或 WAL 在读取期间发生变化，请退出微信后重试".into();
        return result;
    }

    result.input_size = source.len() as u64;
    result.input_layout = build_input_layout(&source);
    if source.len() < PAGE_SIZE {
        result.error = "file_too_small".into();
        return result;
    }

    let data = if source.starts_with(SQLITE_HEADER) {
        match decrypt_plain_sqlite(&source, &wal_data, &mut result) {
            Ok(value) => value,
            Err(error) => {
                if result.error.is_empty() {
                    result.error = error.to_string();
                }
                return result;
            }
        }
    } else {
        match decrypt_encrypted_sqlite(&source, &wal_data, &key_material, &mut result) {
            Ok(value) => value,
            Err(error) => {
                if result.error.is_empty() {
                    result.error = error.to_string();
                }
                return result;
            }
        }
    };

    result.success = true;
    let temp = match write_temp_output(&data, output_path) {
        Ok(value) => value,
        Err(error) => {
            result.success = false;
            result.error = error.to_string();
            return result;
        }
    };
    finalize(result, temp, output_path)
}

fn decrypt_plain_sqlite(
    source: &[u8],
    wal_data: &[u8],
    result: &mut DecryptResult,
) -> Result<Vec<u8>> {
    let raw_page_size = u16::from_be_bytes(source[16..18].try_into().expect("2 bytes")) as usize;
    let page_size = if raw_page_size == 1 {
        65_536
    } else {
        raw_page_size
    };
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(CoreError::InvalidDatabase("SQLite 页大小无效".into()));
    }

    let (mut data, wal_info) = merge_wal_snapshot(source, wal_data, page_size, None)?;
    if wal_info.committed_frames > 0 && data.len() >= 32 {
        data[28..32].copy_from_slice(&(wal_info.database_pages as u32).to_be_bytes());
    }
    result.wal = wal_info;
    result.copied_as_sqlite = true;
    Ok(data)
}

fn decrypt_encrypted_sqlite(
    source: &[u8],
    wal_data: &[u8],
    key_material: &[u8; KEY_SIZE],
    result: &mut DecryptResult,
) -> Result<Vec<u8>> {
    let page1 = &source[..PAGE_SIZE];
    let resolved = match resolve_page1_key_material(key_material, page1) {
        Some(value) => value,
        None => {
            append_failure(result, 1, "hmac", "");
            result.total_pages = (source.len() / PAGE_SIZE) as u64;
            result.failed_pages = 1;
            result.error = "key_mismatch".into();
            return Err(CoreError::InvalidKey("key_mismatch".into()));
        }
    };
    result.key_mode = resolved.mode.as_str().into();
    result.key_authenticated = true;

    let verify = |page: &[u8], page_num: u32| -> bool {
        compute_page_hmac(&resolved.mac_key, page, page_num)
            .map(|expected| bool::from(page[PAGE_SIZE - HMAC_SIZE..PAGE_SIZE].ct_eq(&expected)))
            .unwrap_or(false)
    };
    let (merged, wal_info) = merge_wal_snapshot(source, wal_data, PAGE_SIZE, Some(&verify))?;
    result.wal = wal_info;

    let total_pages = merged.len().div_ceil(PAGE_SIZE);
    result.total_pages = total_pages as u64;
    result.expected_output_size = (total_pages * PAGE_SIZE) as u64;

    let mut decrypted = Vec::with_capacity(total_pages * PAGE_SIZE);
    for page_index in 0..total_pages {
        let page_num = (page_index + 1) as u32;
        let start = page_index * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(merged.len());
        let mut page = merged[start..end].to_vec();
        if page.len() < PAGE_SIZE {
            page.resize(PAGE_SIZE, 0);
        }

        let stored_hmac = &page[PAGE_SIZE - HMAC_SIZE..PAGE_SIZE];
        let expected_hmac =
            compute_page_hmac(&resolved.mac_key, &page, page_num).expect("page is full size");
        if !bool::from(stored_hmac.ct_eq(&expected_hmac)) {
            result.hmac_warning_pages += 1;
            if result.hmac_warning_samples.len() < 8 {
                result.hmac_warning_samples.push(PageSample {
                    page: page_num as u64,
                    reason: "hmac".into(),
                    error: None,
                });
            }
            if result.hmac_debug_samples.len() < 8 {
                result.hmac_debug_samples.push(build_page_anomaly_debug(
                    &resolved,
                    &page,
                    page_num,
                    Some(stored_hmac),
                    Some(&expected_hmac),
                    "hmac",
                ));
            }
        }

        match decrypt_page(&resolved.enc_key, &page, page_num) {
            Ok(plain) => {
                decrypted.extend_from_slice(&plain);
                result.successful_pages += 1;
            }
            Err(error) => {
                result.failed_pages += 1;
                append_failure(result, page_num, "aes", &error.to_string());
                decrypted.extend(std::iter::repeat(0u8).take(PAGE_SIZE));
                if result.aes_debug_samples.len() < 8 {
                    result.aes_debug_samples.push(build_page_anomaly_debug(
                        &resolved,
                        &page,
                        page_num,
                        Some(stored_hmac),
                        Some(&expected_hmac),
                        "aes",
                    ));
                }
            }
        }
    }

    if result.wal.committed_frames > 0 && decrypted.len() >= 32 {
        decrypted[28..32].copy_from_slice(&(result.wal.database_pages as u32).to_be_bytes());
    }
    Ok(decrypted)
}

fn decrypt_page(enc_key: &[u8; KEY_SIZE], page: &[u8], page_num: u32) -> Result<Vec<u8>> {
    if page.len() < PAGE_SIZE {
        return Err(CoreError::Crypto("encrypted page is too short".into()));
    }
    let iv = &page[PAGE_SIZE - RESERVE_SIZE..PAGE_SIZE - RESERVE_SIZE + IV_SIZE];
    let offset = if page_num == 1 { SALT_SIZE } else { 0 };
    let encrypted = &page[offset..PAGE_SIZE - RESERVE_SIZE];
    let plain = aes_cbc_decrypt_256(enc_key, iv, encrypted)?;
    let mut output = Vec::with_capacity(PAGE_SIZE);
    if page_num == 1 {
        output.extend_from_slice(SQLITE_HEADER);
        output.extend_from_slice(&plain);
    } else {
        output.extend_from_slice(&plain);
    }
    output.extend(std::iter::repeat(0u8).take(RESERVE_SIZE));
    debug_assert_eq!(output.len(), PAGE_SIZE);
    Ok(output)
}

fn write_temp_output(data: &[u8], output_path: &Path) -> Result<NamedTempFile> {
    let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".decrypt-")
        .suffix(".db")
        .tempfile_in(parent)?;
    if data.starts_with(SQLITE_HEADER) && data.get(18..20) == Some([2, 2].as_slice()) {
        temp.write_all(&data[..18])?;
        temp.write_all(&[1, 1])?;
        temp.write_all(&data[20..])?;
    } else {
        temp.write_all(data)?;
    }
    temp.flush()?;
    Ok(temp)
}

fn finalize(mut result: DecryptResult, temp: NamedTempFile, output_path: &Path) -> DecryptResult {
    let temp_path = temp.path().to_path_buf();
    result.output_size = fs::metadata(&temp_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    let mut diagnostics = collect_sqlite_diagnostics(&temp_path, true, None, 5);
    if result.success
        && diagnostics.quick_check_ok == Some(false)
        && result.key_authenticated
        && result.failed_pages == 0
        && result.hmac_warning_pages == 0
    {
        let repair = repair_sqlite_indexes(&temp_path);
        let repaired = repair.success;
        result.index_repair = repair;
        if repaired {
            diagnostics = collect_sqlite_diagnostics(&temp_path, true, None, 5);
        }
    }
    result.output_size = fs::metadata(&temp_path)
        .map(|metadata| metadata.len())
        .unwrap_or(result.output_size);
    diagnostics.path = Some(output_path.display().to_string());
    result.diagnostic_status = sqlite_diagnostics_status(&diagnostics).to_string();
    result.output_header_debug = read_plain_sqlite_header_debug(&temp_path, output_path);
    result.diagnostics = diagnostics;

    if result.success {
        let message = build_decrypt_failure_message(&result);
        if !message.is_empty() {
            result.success = false;
            result.error = message;
        }
    }

    if result.success {
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = PathBuf::from(format!("{}{}", output_path.display(), suffix));
            if sidecar.exists() {
                result.success = false;
                result.error = "输出数据库正在使用或遗留日志文件，请关闭读取连接后重试".into();
                break;
            }
        }
    }

    if result.success {
        if let Err(error) = temp.persist(output_path) {
            result.success = false;
            result.error = error.to_string();
        }
    }
    result
}

fn build_decrypt_failure_message(result: &DecryptResult) -> String {
    let detail = diagnostics_detail(result);
    if result.failed_pages > 0 && result.successful_pages == 0 {
        if !detail.is_empty() {
            return format!("数据库校验未通过，密钥可能不匹配当前账号: {detail}");
        }
        return "数据库校验未通过，密钥可能不匹配当前账号".into();
    }
    if !result.diagnostic_status.is_empty() && result.diagnostic_status != "ok" {
        if !detail.is_empty() {
            return format!("解密输出不是有效的 SQLite 数据库: {detail}");
        }
        return "解密输出不是有效的 SQLite 数据库".into();
    }
    if result.failed_pages > 0 {
        return "解密输出包含页失败，结果不完整".into();
    }
    String::new()
}

fn diagnostics_detail(result: &DecryptResult) -> String {
    let diagnostics: &Diagnostics = &result.diagnostics;
    let candidate = diagnostics
        .quick_check_error
        .as_deref()
        .or(diagnostics.connect_error.as_deref())
        .or(diagnostics.table_list_error.as_deref())
        .or(diagnostics.page_count_error.as_deref())
        .or_else(|| quick_check_text(diagnostics.quick_check.as_ref()))
        .unwrap_or(&result.diagnostic_status);
    candidate.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn quick_check_text(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::String(value)) => Some(value),
        _ => None,
    }
}

fn build_input_layout(source: &[u8]) -> InputLayout {
    InputLayout {
        page_size: PAGE_SIZE,
        reserve_size: RESERVE_SIZE,
        iv_size: IV_SIZE,
        hmac_size: HMAC_SIZE,
        input_size: source.len() as u64,
        input_size_mod_page: source.len() % PAGE_SIZE,
        total_pages_floor: source.len() / PAGE_SIZE,
        total_pages_ceil: source.len().div_ceil(PAGE_SIZE),
        starts_with_sqlite_header: source.starts_with(SQLITE_HEADER),
        first16_hex: hex::encode(&source[..source.len().min(16)]),
    }
}

fn append_failure(result: &mut DecryptResult, page_num: u32, reason: &str, error: &str) {
    *result
        .failure_reasons
        .entry(reason.to_string())
        .or_insert(0) += 1;
    if result.failed_page_samples.len() >= 8 {
        return;
    }
    let cleaned = error.split_whitespace().collect::<Vec<_>>().join(" ");
    result.failed_page_samples.push(PageSample {
        page: page_num as u64,
        reason: reason.to_string(),
        error: if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.chars().take(200).collect())
        },
    });
}

fn wal_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", db_path.display()))
}

fn read_optional(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_default()
}

fn read_journal_magic(db_path: &Path) -> Option<[u8; 8]> {
    let path = PathBuf::from(format!("{}-journal", db_path.display()));
    let mut file = fs::File::open(path).ok()?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic).ok()?;
    Some(magic)
}

fn elapsed_ms(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

fn same_file(left: &Path, right: &Path) -> bool {
    if left.exists() && right.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let (Ok(left_meta), Ok(right_meta)) = (fs::metadata(left), fs::metadata(right)) {
                if left_meta.dev() == right_meta.dev() && left_meta.ino() == right_meta.ino() {
                    return true;
                }
            }
        }
    }
    let left = canonical_or_absolute(left);
    let right = if right.exists() {
        canonical_or_absolute(right)
    } else {
        let parent = right.parent().unwrap_or_else(|| Path::new("."));
        canonical_or_absolute(parent).join(
            right
                .file_name()
                .map(|value| value.to_os_string())
                .unwrap_or_default(),
        )
    };
    left == right
}

fn canonical_or_absolute(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|directory| directory.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

fn safe_file_snapshot(path: &Path) -> FileSnapshot {
    let mut snapshot = FileSnapshot {
        path: path.display().to_string(),
        exists: false,
        ..FileSnapshot::default()
    };
    match fs::metadata(path) {
        Ok(metadata) => {
            snapshot.exists = true;
            snapshot.size = Some(metadata.len());
            snapshot.inode = Some(inode(&metadata));
            snapshot.mtime_ns = Some(mtime_ns(&metadata));
        }
        Err(error) => {
            snapshot.stat_error = Some(error.to_string());
        }
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let sibling_path = PathBuf::from(format!("{}{}", path.display(), suffix));
        let sibling = match fs::metadata(&sibling_path) {
            Ok(metadata) => SiblingSnapshot {
                exists: true,
                size: Some(metadata.len()),
                inode: Some(inode(&metadata)),
                mtime_ns: Some(mtime_ns(&metadata)),
                stat_error: None,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SiblingSnapshot {
                exists: false,
                ..SiblingSnapshot::default()
            },
            Err(error) => SiblingSnapshot {
                exists: false,
                stat_error: Some(error.to_string()),
                ..SiblingSnapshot::default()
            },
        };
        snapshot.siblings.insert(suffix.to_string(), sibling);
    }
    snapshot
}

fn snapshots_changed(before: &FileSnapshot, after: &FileSnapshot) -> bool {
    before.size != after.size
        || before.mtime_ns != after.mtime_ns
        || before.inode != after.inode
        || ["-wal", "-journal"]
            .iter()
            .any(|suffix| before.siblings.get(*suffix) != after.siblings.get(*suffix))
}

fn inode(metadata: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn mtime_ns(metadata: &fs::Metadata) -> i128 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128
    }
    #[cfg(not(unix))]
    {
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as i128)
            .unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_nanos() as i128)
                    .unwrap_or(0)
            })
    }
}

fn read_plain_sqlite_header_debug(path: &Path, output_path: &Path) -> Value {
    let mut value = json!({"path": output_path.display().to_string()});
    let mut file = match fs::File::open(path) {
        Ok(value) => value,
        Err(error) => {
            value["error"] = Value::String(error.to_string());
            return value;
        }
    };
    let mut header = vec![0u8; 100];
    let read = file.read(&mut header).unwrap_or(0);
    header.truncate(read);
    value["header_len"] = json!(header.len());
    value["header_ok"] = json!(header.starts_with(SQLITE_HEADER));
    value["header_hex"] = json!(hex::encode(&header[..header.len().min(32)]));
    if header.len() >= 100 {
        let raw_page_size = u16::from_be_bytes(header[16..18].try_into().expect("2 bytes"));
        value["page_size_header"] = json!(if raw_page_size == 1 {
            65_536
        } else {
            raw_page_size as usize
        });
        value["write_version"] = json!(header[18] as usize);
        value["read_version"] = json!(header[19] as usize);
        value["reserved_space"] = json!(header[20] as usize);
        value["max_payload_fraction"] = json!(header[21] as usize);
        value["min_payload_fraction"] = json!(header[22] as usize);
        value["leaf_payload_fraction"] = json!(header[23] as usize);
        value["file_change_counter"] = json!(u32::from_be_bytes(
            header[24..28].try_into().expect("4 bytes")
        ));
        value["db_size_pages_header"] = json!(u32::from_be_bytes(
            header[28..32].try_into().expect("4 bytes")
        ));
        value["freelist_trunk_page"] = json!(u32::from_be_bytes(
            header[32..36].try_into().expect("4 bytes")
        ));
        value["freelist_pages"] = json!(u32::from_be_bytes(
            header[36..40].try_into().expect("4 bytes")
        ));
        value["schema_cookie"] = json!(u32::from_be_bytes(
            header[40..44].try_into().expect("4 bytes")
        ));
        value["schema_format"] = json!(u32::from_be_bytes(
            header[44..48].try_into().expect("4 bytes")
        ));
        value["text_encoding"] = json!(u32::from_be_bytes(
            header[56..60].try_into().expect("4 bytes")
        ));
    }
    value
}

fn build_page_anomaly_debug(
    resolved: &ResolvedKey,
    page: &[u8],
    page_num: u32,
    stored_hmac: Option<&[u8]>,
    expected_hmac: Option<&[u8]>,
    reason: &str,
) -> Value {
    let stored = stored_hmac
        .map(|value| value.to_vec())
        .unwrap_or_else(|| page[PAGE_SIZE - HMAC_SIZE..PAGE_SIZE].to_vec());
    let expected = expected_hmac
        .map(|value| value.to_vec())
        .unwrap_or_else(|| {
            compute_page_hmac(&resolved.mac_key, page, page_num)
                .map(|value| value.to_vec())
                .unwrap_or_default()
        });
    let iv = &page[PAGE_SIZE - RESERVE_SIZE..PAGE_SIZE - RESERVE_SIZE + IV_SIZE];
    let payload_offset = if page_num == 1 { SALT_SIZE } else { 0 };
    let encrypted_payload = &page[payload_offset..PAGE_SIZE - RESERVE_SIZE];

    let mut variants: Vec<String> = Vec::new();
    for candidate in [page_num.wrapping_sub(1), page_num, page_num.wrapping_add(1)] {
        if candidate == 0 {
            continue;
        }
        for big_endian in [false, true] {
            for include_iv in [true, false] {
                if let Some(expected_variant) = compute_page_hmac_variant(
                    &resolved.mac_key,
                    page,
                    candidate,
                    big_endian,
                    include_iv,
                ) {
                    if bool::from(stored.as_slice().ct_eq(&expected_variant)) {
                        variants.push(format!(
                            "page={candidate};endian={};include_iv={}",
                            if big_endian { "big" } else { "little" },
                            u8::from(include_iv)
                        ));
                    }
                }
            }
        }
    }

    let mut value = json!({
        "reason": reason,
        "page": page_num,
        "byte_start": (page_num as u64 - 1) * PAGE_SIZE as u64,
        "byte_end_exclusive": page_num as u64 * PAGE_SIZE as u64,
        "page_size": page.len(),
        "page_sha256": hash_prefix(page, 24),
        "encrypted_payload_sha256": hash_prefix(encrypted_payload, 24),
        "iv_hex": hex::encode(&iv[..16]),
        "stored_hmac_prefix": hex::encode(&stored[..stored.len().min(16)]),
        "expected_hmac_prefix": hex::encode(&expected[..expected.len().min(16)]),
        "hmac_match_current": bool::from(stored.as_slice().ct_eq(&expected)),
        "hmac_variant_matches": variants,
    });

    match decrypt_page(&resolved.enc_key, page, page_num) {
        Ok(plain) => {
            value["aes_decrypt_ok"] = json!(true);
            value["plain"] = build_plain_page_debug(&plain, page_num);
        }
        Err(error) => {
            value["aes_decrypt_ok"] = json!(false);
            value["aes_error"] = json!(error.to_string());
        }
    }
    value
}

fn compute_page_hmac_variant(
    mac_key: &[u8],
    page: &[u8],
    page_num: u32,
    big_endian: bool,
    include_iv: bool,
) -> Option<[u8; 64]> {
    if page.len() < PAGE_SIZE {
        return None;
    }
    let offset = if page_num == 1 { SALT_SIZE } else { 0 };
    let data_end = PAGE_SIZE - RESERVE_SIZE + if include_iv { IV_SIZE } else { 0 };
    let page_bytes = if big_endian {
        page_num.to_be_bytes()
    } else {
        page_num.to_le_bytes()
    };
    Some(hmac_sha512_parts(
        mac_key,
        &[&page[offset..data_end], &page_bytes],
    ))
}

fn build_plain_page_debug(page: &[u8], page_num: u32) -> Value {
    let header_offset = if page_num == 1 { 100 } else { 0 };
    let mut value = json!({
        "page": page_num,
        "plain_sha256": hash_prefix(page, 24),
    });
    if page.len() >= header_offset + 12 {
        let page_type = page[header_offset] as usize;
        value["btree_header_offset"] = json!(header_offset);
        value["btree_page_type"] = json!(page_type);
        value["btree_page_type_name"] = json!(match page_type {
            2 => "interior_index",
            5 => "interior_table",
            10 => "leaf_index",
            13 => "leaf_table",
            _ => "unknown",
        });
        value["first_freeblock"] = json!(u16::from_be_bytes(
            page[header_offset + 1..header_offset + 3]
                .try_into()
                .expect("2 bytes")
        ));
        value["cell_count"] = json!(u16::from_be_bytes(
            page[header_offset + 3..header_offset + 5]
                .try_into()
                .expect("2 bytes")
        ));
        value["cell_content_area"] = json!(u16::from_be_bytes(
            page[header_offset + 5..header_offset + 7]
                .try_into()
                .expect("2 bytes")
        ));
        value["fragmented_free_bytes"] = json!(page[header_offset + 7] as usize);
        if matches!(page_type, 2 | 5) {
            value["right_most_pointer"] = json!(u32::from_be_bytes(
                page[header_offset + 8..header_offset + 12]
                    .try_into()
                    .expect("4 bytes")
            ));
        }
    }
    value
}

fn hash_prefix(data: &[u8], length: usize) -> String {
    let digest = Sha256::digest(data);
    let encoded = hex::encode(digest);
    encoded[..encoded.len().min(length)].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_output_equal_to_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.db");
        fs::write(&path, b"test").unwrap();
        let result = decrypt_database(&path, &path, &"00".repeat(32));
        assert!(!result.success);
        assert!(result.error.contains("覆盖源数据库"));
    }

    #[test]
    fn rejects_invalid_key() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("a.db");
        let output = directory.path().join("b.db");
        fs::write(&source, vec![0u8; PAGE_SIZE]).unwrap();
        let result = decrypt_database(&source, &output, "xyz");
        assert!(!result.success);
        assert!(result.error.contains("64"));
    }
}
