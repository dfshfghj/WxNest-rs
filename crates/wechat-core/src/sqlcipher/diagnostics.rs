use std::fs::File;
use std::io::Read;
use std::path::Path;

use regex::Regex;
use rusqlite::{Connection, OpenFlags};

use crate::model::{Diagnostics, IndexRepairResult};

use super::SQLITE_HEADER;

pub fn collect_sqlite_diagnostics(
    path: &Path,
    quick_check: bool,
    table_name: Option<&str>,
    table_sample_limit: usize,
) -> Diagnostics {
    let mut diagnostics = Diagnostics {
        path: Some(path.display().to_string()),
        exists: Some(path.exists()),
        ..Diagnostics::default()
    };
    if !path.exists() {
        return diagnostics;
    }

    match std::fs::metadata(path) {
        Ok(metadata) => diagnostics.size = Some(metadata.len()),
        Err(error) => diagnostics.size_error = Some(clean_error(&error)),
    }

    let header = read_header(path, SQLITE_HEADER.len());
    match header {
        Ok(bytes) => {
            diagnostics.header_ok = Some(bytes == SQLITE_HEADER);
            diagnostics.header_hex = Some(hex::encode(&bytes));
        }
        Err(error) => diagnostics.header_error = Some(clean_error(&error)),
    }

    if !quick_check {
        return diagnostics;
    }

    let connection = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.connect_error = Some(clean_error(&error));
            return diagnostics;
        }
    };

    match query_i64(&connection, "PRAGMA page_size") {
        Ok(value) => diagnostics.page_size = Some(value),
        Err(error) => diagnostics.page_size_error = Some(clean_error(&error)),
    }
    match query_i64(&connection, "PRAGMA page_count") {
        Ok(value) => diagnostics.page_count = Some(value),
        Err(error) => diagnostics.page_count_error = Some(clean_error(&error)),
    }

    match list_tables(&connection) {
        Ok(tables) => {
            diagnostics.table_count = Some(tables.len());
            if !tables.is_empty() {
                let limit = table_sample_limit.max(1);
                diagnostics.tables_sample = Some(tables.into_iter().take(limit).collect());
            }
        }
        Err(error) => diagnostics.table_list_error = Some(clean_error(&error)),
    }

    if let Some(name) = table_name {
        diagnostics.target_table = Some(name.to_string());
        match table_info(&connection, name) {
            Ok(columns) => {
                diagnostics.target_table_exists = Some(!columns.is_empty());
                if !columns.is_empty() {
                    diagnostics.target_table_columns = Some(columns.into_iter().take(8).collect());
                }
            }
            Err(error) => diagnostics.target_table_error = Some(clean_error(&error)),
        }
    }

    match quick_check_rows(&connection) {
        Ok(rows) => {
            let values: Vec<String> = rows.iter().map(|value| clean_text(value)).collect();
            if values.is_empty() {
                diagnostics.quick_check = Some(serde_json::Value::String(String::new()));
                diagnostics.quick_check_ok = None;
            } else {
                diagnostics.quick_check_ok =
                    Some(values.len() == 1 && values[0].eq_ignore_ascii_case("ok"));
                if values.len() > 5 {
                    diagnostics.quick_check_truncated = Some(values.len() - 5);
                }
                diagnostics.quick_check = Some(if values.len() == 1 {
                    serde_json::Value::String(values[0].clone())
                } else {
                    serde_json::Value::Array(
                        values
                            .into_iter()
                            .take(5)
                            .map(serde_json::Value::String)
                            .collect(),
                    )
                });
            }
        }
        Err(error) => {
            diagnostics.quick_check_error = Some(clean_error(&error));
            diagnostics.quick_check_ok = Some(false);
        }
    }

    diagnostics
}

pub fn sqlite_diagnostics_status(diagnostics: &Diagnostics) -> &'static str {
    if diagnostics.path.is_none() && diagnostics.exists.is_none() {
        return "not_run";
    }
    if diagnostics.exists == Some(false) {
        return "missing";
    }
    if diagnostics.header_ok == Some(false) {
        return "bad_header";
    }
    if diagnostics.connect_error.is_some() {
        return "connect_error";
    }
    if diagnostics.quick_check_error.is_some() {
        return "quick_check_error";
    }
    if diagnostics.quick_check_ok == Some(false) {
        return "quick_check_failed";
    }
    if diagnostics.quick_check_ok == Some(true) {
        return "ok";
    }
    "header_only"
}

pub fn repair_sqlite_indexes(path: &Path) -> IndexRepairResult {
    let mut result = IndexRepairResult::default();
    let connection = match Connection::open(path) {
        Ok(value) => value,
        Err(error) => {
            result.error = Some(clean_error(&error));
            return result;
        }
    };
    if let Err(error) = connection.execute_batch("PRAGMA trusted_schema=OFF") {
        result.error = Some(clean_error(&error));
        return result;
    }

    let errors = match integrity_check(&connection, Some(1000)) {
        Ok(values) => values,
        Err(error) => {
            result.error = Some(clean_error(&error));
            return result;
        }
    };
    if errors.is_empty() || errors == ["ok"] || errors.len() >= 1000 {
        return result;
    }

    let pattern = Regex::new(r"^(?:wrong # of entries in index|row \d+ missing from index) (.+)$")
        .expect("valid index repair regex");
    let mut indexes: Vec<String> = Vec::new();
    for error in &errors {
        let Some(captures) = pattern.captures(error) else {
            return result;
        };
        let name = captures[1].to_string();
        if !indexes.contains(&name) {
            indexes.push(name);
        }
    }
    if indexes.is_empty() {
        return result;
    }

    let known = match list_index_names(&connection) {
        Ok(values) => values,
        Err(error) => {
            result.error = Some(clean_error(&error));
            return result;
        }
    };
    if !indexes.iter().all(|name| known.contains(name)) {
        return result;
    }

    indexes.sort();
    result.attempted = true;
    result.indexes = Some(indexes.clone());
    result.original_errors = Some(errors.iter().take(5).cloned().collect());

    for name in &indexes {
        if let Err(error) = connection.execute(&format!("REINDEX {}", quote_ident(name)), []) {
            result.error = Some(clean_error(&error));
            return result;
        }
    }
    match integrity_check(&connection, None) {
        Ok(values) => result.success = values == ["ok"],
        Err(error) => result.error = Some(clean_error(&error)),
    }
    result
}

pub fn is_usable_sqlite_db(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let metadata = match std::fs::metadata(path) {
        Ok(value) => value,
        Err(_) => return false,
    };
    if metadata.len() <= SQLITE_HEADER.len() as u64 {
        return false;
    }
    match read_header(path, SQLITE_HEADER.len()) {
        Ok(bytes) if bytes == SQLITE_HEADER => {}
        _ => return false,
    }
    let connection = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(value) => value,
        Err(_) => return false,
    };
    query_i64(&connection, "PRAGMA schema_version").is_ok()
        && connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' LIMIT 1",
                [],
                |_| Ok(()),
            )
            .is_ok()
}

fn read_header(path: &Path, length: usize) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; length];
    let mut read = 0;
    while read < length {
        let count = file.read(&mut buffer[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    buffer.truncate(read);
    Ok(buffer)
}

fn query_i64(connection: &Connection, sql: &str) -> rusqlite::Result<i64> {
    connection.query_row(sql, [], |row| row.get::<_, i64>(0))
}

fn list_tables(connection: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement =
        connection.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn list_index_names(connection: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare("SELECT name FROM sqlite_master WHERE type='index'")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn table_info(connection: &Connection, name: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({})", quote_ident(name)))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect()
}

fn quick_check_rows(connection: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare("PRAGMA quick_check")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn integrity_check(connection: &Connection, limit: Option<usize>) -> rusqlite::Result<Vec<String>> {
    let sql = match limit {
        Some(limit) => format!("PRAGMA integrity_check({limit})"),
        None => "PRAGMA integrity_check".to_string(),
    };
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn clean_text(value: &str) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > 240 {
        let prefix: String = text.chars().take(237).collect();
        format!("{prefix}...")
    } else {
        text
    }
}

fn clean_error(error: &dyn std::fmt::Display) -> String {
    let text = clean_text(&error.to_string());
    if text.is_empty() {
        "error".into()
    } else {
        text
    }
}
