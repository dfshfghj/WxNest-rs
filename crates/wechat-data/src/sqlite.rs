use std::path::Path;
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};

use crate::error::Result;

pub fn decode_utf8_ignore(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                output.push_str(valid);
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if let Ok(valid) = std::str::from_utf8(&rest[..valid_up_to]) {
                    output.push_str(valid);
                }
                let skip = error.error_len().unwrap_or(rest.len() - valid_up_to);
                rest = &rest[valid_up_to + skip..];
            }
        }
    }
    output
}

#[derive(Debug, Clone)]
pub enum SqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    pub fn from_ref(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(value) => Self::Integer(value),
            ValueRef::Real(value) => Self::Real(value),
            ValueRef::Text(value) => Self::Text(decode_utf8_ignore(value)),
            ValueRef::Blob(value) => Self::Blob(value.to_vec()),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn text_lossy(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Integer(value) => value.to_string(),
            Self::Real(value) => value.to_string(),
            Self::Text(value) => value.clone(),
            Self::Blob(value) => decode_utf8_ignore(value),
        }
    }

    pub fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::Blob(value) => Some(value.clone()),
            Self::Text(value) => Some(value.as_bytes().to_vec()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            Self::Real(value) => Some(*value as i64),
            Self::Text(value) => value.trim().parse().ok(),
            Self::Null | Self::Blob(_) => None,
        }
    }
}

pub fn open_read_only(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "query_only", "ON")?;
    Ok(connection)
}

pub fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub fn table_names(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare(&format!("PRAGMA table_info({})", quote_ident(table)))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn column_set(columns: &[String]) -> std::collections::HashSet<String> {
    columns
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

pub fn row_value(row: &Row<'_>, index: usize) -> SqlValue {
    SqlValue::from_ref(row.get_ref(index).unwrap_or(ValueRef::Null))
}

pub fn row_text(row: &Row<'_>, index: usize) -> String {
    row_value(row, index).text_lossy()
}

pub fn row_i64(row: &Row<'_>, index: usize) -> i64 {
    row_value(row, index).as_i64().unwrap_or(0)
}

pub fn row_optional_i64(row: &Row<'_>, index: usize) -> Option<i64> {
    row_value(row, index).as_i64()
}

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn table_has_column(columns: &std::collections::HashSet<String>, name: &str) -> bool {
    columns.contains(&name.to_ascii_lowercase())
}
