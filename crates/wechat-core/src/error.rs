use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid key: {0}")]
    InvalidKey(String),

    #[error("invalid database: {0}")]
    InvalidDatabase(String),

    #[error("crypto: {0}")]
    Crypto(String),

    #[error("wal: {0}")]
    Wal(String),

    #[error("sqlite: {0}")]
    Sqlite(String),

    #[error("media: {0}")]
    Media(String),

    #[error("image key: {0}")]
    ImageKey(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
