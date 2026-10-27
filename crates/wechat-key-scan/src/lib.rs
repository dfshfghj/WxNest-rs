#[derive(Debug, Clone)]
pub struct RecoveredKey {
    pub key_hex: String,

    pub raw_key: bool,
    pub source: String,
    pub internal_key_hex: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImageKeyMatch {
    pub aes_key: String,
    pub xor_key: u8,
    pub encoding: String,
    pub pid: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyScanError {
    #[error("仅支持 Windows")]
    Unsupported,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("未找到: {0}")]
    NotFound(String),
    #[error("Windows API: {0}")]
    Windows(String),
}

pub type Result<T> = std::result::Result<T, KeyScanError>;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{recover_image_key_for_account, recover_key_for_db_storage};

#[cfg(not(windows))]
pub fn recover_image_key_for_account(
    _account_dir: &std::path::Path,
    _pid: Option<u32>,
) -> Result<ImageKeyMatch> {
    Err(KeyScanError::Unsupported)
}

#[cfg(not(windows))]
pub fn recover_key_for_db_storage(
    _db_storage: &std::path::Path,
    _wechat_dir: Option<&std::path::Path>,
) -> Result<RecoveredKey> {
    Err(KeyScanError::Unsupported)
}
