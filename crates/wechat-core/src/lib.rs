#![forbid(unsafe_code)]

pub mod account;
pub mod crypto;
pub mod error;
pub mod image_key;
pub mod media;
pub mod model;
pub mod sqlcipher;

pub use account::{
    decrypt_account, decrypt_account_with_options, AccountDatabaseOutcome, AccountDecryptOptions,
    AccountDecryptReport, SOURCE_INFO_FILE,
};
pub use error::{CoreError, Result};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
