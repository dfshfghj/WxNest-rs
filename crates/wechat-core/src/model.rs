use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct PageSample {
    pub page: u64,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SiblingSnapshot {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileSnapshot {
    pub path: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat_error: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub siblings: BTreeMap<String, SiblingSnapshot>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WalInfo {
    pub wal_bytes: usize,
    pub committed_frames: usize,
    pub applied_pages: usize,
    pub ignored_tail_bytes: usize,
    pub database_pages: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct InputLayout {
    pub page_size: usize,
    pub reserve_size: usize,
    pub iv_size: usize,
    pub hmac_size: usize,
    pub input_size: u64,
    pub input_size_mod_page: usize,
    pub total_pages_floor: usize,
    pub total_pages_ceil: usize,
    pub starts_with_sqlite_header: bool,
    pub first16_hex: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Diagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables_sample: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_list_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_table_exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_table_columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_table_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_check: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_check_ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_check_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_check_truncated: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexRepairResult {
    pub attempted: bool,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_errors: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecryptResult {
    pub db_path: String,
    pub db_name: String,
    pub output_path: String,
    pub success: bool,
    pub copied_as_sqlite: bool,
    pub input_size: u64,
    pub output_size: u64,
    pub total_pages: u64,
    pub successful_pages: u64,
    pub failed_pages: u64,
    pub failed_page_samples: Vec<PageSample>,
    pub failure_reasons: BTreeMap<String, u64>,
    pub hmac_warning_pages: u64,
    pub hmac_warning_samples: Vec<PageSample>,
    pub hmac_debug_samples: Vec<serde_json::Value>,
    pub aes_debug_samples: Vec<serde_json::Value>,
    pub source_snapshot_before: FileSnapshot,
    pub source_snapshot_after: FileSnapshot,
    pub source_changed_during_read: bool,
    pub read_ms: f64,
    pub key_mode: String,
    pub key_authenticated: bool,
    pub wal: WalInfo,
    pub index_repair: IndexRepairResult,
    pub input_layout: InputLayout,
    pub expected_output_size: u64,
    pub output_header_debug: serde_json::Value,
    pub diagnostics: Diagnostics,
    pub diagnostic_status: String,
    pub error: String,
}

impl DecryptResult {
    pub fn new(db_path: &Path, output_path: &Path) -> Self {
        Self {
            db_path: db_path.display().to_string(),
            db_name: db_path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_default(),
            output_path: output_path.display().to_string(),
            success: false,
            copied_as_sqlite: false,
            input_size: 0,
            output_size: 0,
            total_pages: 0,
            successful_pages: 0,
            failed_pages: 0,
            failed_page_samples: Vec::new(),
            failure_reasons: BTreeMap::new(),
            hmac_warning_pages: 0,
            hmac_warning_samples: Vec::new(),
            hmac_debug_samples: Vec::new(),
            aes_debug_samples: Vec::new(),
            source_snapshot_before: FileSnapshot::default(),
            source_snapshot_after: FileSnapshot::default(),
            source_changed_during_read: false,
            read_ms: 0.0,
            key_mode: String::new(),
            key_authenticated: false,
            wal: WalInfo::default(),
            index_repair: IndexRepairResult::default(),
            input_layout: InputLayout::default(),
            expected_output_size: 0,
            output_header_debug: serde_json::json!({}),
            diagnostics: Diagnostics::default(),
            diagnostic_status: "not_run".into(),
            error: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KeyValidationResult {
    pub valid: bool,
    pub required_roles: Vec<String>,
    pub verified_roles: Vec<String>,
    pub modes: BTreeMap<String, String>,
    pub reason: String,
}

impl KeyValidationResult {
    pub fn invalid(reason: &str) -> Self {
        Self {
            valid: false,
            required_roles: vec!["message".into(), "session".into()],
            verified_roles: Vec::new(),
            modes: BTreeMap::new(),
            reason: reason.into(),
        }
    }
}
