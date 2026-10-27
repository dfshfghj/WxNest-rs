use std::path::Path;

use crate::error::{DataError, Result};

pub use wechat_key_scan::{ImageKeyMatch, RecoveredKey};

pub fn recover_key_for_db_storage(
    db_storage: &Path,
    wechat_dir: Option<&Path>,
) -> Result<RecoveredKey> {
    wechat_key_scan::recover_key_for_db_storage(db_storage, wechat_dir)
        .map_err(|error| DataError::Decode(format!("密钥扫描失败: {error}")))
}

pub fn recover_image_key_for_account(
    account_dir: &Path,
    pid: Option<u32>,
) -> Result<ImageKeyMatch> {
    wechat_key_scan::recover_image_key_for_account(account_dir, pid)
        .map_err(|error| DataError::Decode(format!("图片密钥扫描失败: {error}")))
}
