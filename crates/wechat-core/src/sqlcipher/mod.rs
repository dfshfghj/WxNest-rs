pub mod decryptor;
pub mod diagnostics;
pub mod filters;
pub mod key;
pub mod scan;
pub mod wal;

pub const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
pub const PAGE_SIZE: usize = 4096;
pub const KEY_SIZE: usize = 32;
pub const SALT_SIZE: usize = 16;
pub const IV_SIZE: usize = 16;
pub const HMAC_SIZE: usize = 64;
pub const RESERVE_SIZE: usize = IV_SIZE + HMAC_SIZE;

pub use decryptor::decrypt_database;
pub use diagnostics::{
    collect_sqlite_diagnostics, is_usable_sqlite_db, repair_sqlite_indexes,
    sqlite_diagnostics_status,
};
pub use filters::{
    is_index_database_file, list_countable_database_names, normalize_database_file_name,
    should_include_in_database_count, should_skip_source_database,
};
pub use key::{
    compute_page_hmac, parse_key_hex, read_page1, resolve_page1_key_material,
    validate_realtime_database_key, KeyMode, ResolvedKey,
};
pub use scan::{
    derive_account_name_from_path, normalize_account_name, scan_account_databases_from_path,
    AccountDatabase, AccountSource, DetectedAccount, ScanResult,
};
pub use wal::merge_wal_snapshot;
