use std::path::{Path, PathBuf};

const IGNORED_SOURCE_DATABASE_NAMES: &[&str] = &["key_info.db"];
const INDEX_DATABASE_NAMES: &[&str] = &["chat_search_index.db", "chat_search_index.tmp.db"];
const INDEX_DATABASE_SUFFIXES: &[&str] = &["_fts.db"];
const INTERNAL_OUTPUT_DATABASE_NAMES: &[&str] = &[
    "chat_search_index.db",
    "chat_search_index.tmp.db",
    "session_last_message.db",
];

pub fn normalize_database_file_name(file_name: &Path) -> String {
    file_name
        .file_name()
        .map(|value| value.to_string_lossy().trim().to_lowercase())
        .unwrap_or_default()
}

pub fn is_index_database_file(file_name: &Path) -> bool {
    let lower = normalize_database_file_name(file_name);
    if lower.is_empty() {
        return false;
    }
    INDEX_DATABASE_NAMES.contains(&lower.as_str())
        || INDEX_DATABASE_SUFFIXES
            .iter()
            .any(|suffix| lower.ends_with(suffix))
}

pub fn should_skip_source_database(file_name: &Path) -> bool {
    let lower = normalize_database_file_name(file_name);
    if lower.is_empty() {
        return true;
    }
    IGNORED_SOURCE_DATABASE_NAMES.contains(&lower.as_str()) || is_index_database_file(file_name)
}

pub fn should_include_in_database_count(file_name: &Path) -> bool {
    let lower = normalize_database_file_name(file_name);
    if !lower.ends_with(".db") {
        return false;
    }
    if should_skip_source_database(file_name) {
        return false;
    }
    !INTERNAL_OUTPUT_DATABASE_NAMES.contains(&lower.as_str())
}

pub fn list_countable_database_names(account_dir: &Path) -> Vec<String> {
    if !account_dir.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = std::fs::read_dir(account_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if should_include_in_database_count(&path) {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

pub fn is_sqlite_database_path(path: &Path) -> bool {
    path.extension()
        .map(|value| value.to_string_lossy().eq_ignore_ascii_case("db"))
        .unwrap_or(false)
}

pub fn path_file_name(path: &Path) -> PathBuf {
    path.file_name().map(PathBuf::from).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_python_names() {
        assert!(should_skip_source_database(Path::new("key_info.db")));
        assert!(should_skip_source_database(Path::new(
            "chat_search_index.db"
        )));
        assert!(should_skip_source_database(Path::new(
            "chat_search_index.tmp.db"
        )));
        assert!(should_skip_source_database(Path::new("message_fts.db")));
        assert!(!should_skip_source_database(Path::new("message_0.db")));
        assert!(!should_include_in_database_count(Path::new(
            "session_last_message.db"
        )));
        assert!(should_include_in_database_count(Path::new("message_0.db")));
    }
}
