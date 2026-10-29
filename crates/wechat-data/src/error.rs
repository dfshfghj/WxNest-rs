use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("invalid account: {0}")]
    InvalidAccount(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("decode: {0}")]
    Decode(String),

    #[error("action: {0}")]
    Action(String),

    #[error("broker: {0}")]
    Broker(#[from] wechat_broker::BrokerError),
}

pub type Result<T> = std::result::Result<T, DataError>;
