use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::error::Result;
use crate::sqlite::{open_read_only, row_value, table_exists, SqlValue};

pub fn load_group_nickname_map(
    contact_db_path: &Path,
    chatroom_id: &str,
    sender_usernames: &[String],
) -> Result<HashMap<String, String>> {
    let chatroom = chatroom_id.trim();
    if !chatroom.ends_with("@chatroom") || !contact_db_path.is_file() {
        return Ok(HashMap::new());
    }

    let connection = open_read_only(contact_db_path)?;
    if !table_exists(&connection, "chat_room")? {
        return Ok(HashMap::new());
    }
    let ext_buffer: Option<Vec<u8>> = connection
        .query_row(
            "SELECT ext_buffer FROM chat_room WHERE username = ?1 LIMIT 1",
            [chatroom],
            |row| Ok(row_value(row, 0)),
        )
        .ok()
        .and_then(|value| match value {
            SqlValue::Blob(bytes) => Some(bytes),
            _ => None,
        });
    let Some(ext_buffer) = ext_buffer else {
        return Ok(HashMap::new());
    };
    Ok(parse_group_nickname_map(&ext_buffer, sender_usernames))
}

pub(crate) fn parse_group_nickname_map(
    ext_buffer: &[u8],
    sender_usernames: &[String],
) -> HashMap<String, String> {
    if ext_buffer.is_empty() {
        return HashMap::new();
    }
    let targets: Vec<String> = sender_usernames
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .fold(Vec::new(), |mut unique, value| {
            if !unique.contains(&value) {
                unique.push(value);
            }
            unique
        });
    if targets.is_empty() {
        return HashMap::new();
    }
    let target_set: HashSet<&str> = targets.iter().map(String::as_str).collect();

    let mut member_records: Vec<Vec<(u64, String)>> = Vec::new();
    for (_, chunk) in iter_fields(ext_buffer) {
        let mut strings = Vec::new();
        for (field_no, value) in iter_fields(chunk) {
            if value.is_empty() || value.len() > 256 {
                continue;
            }
            let Ok(text) = std::str::from_utf8(value) else {
                continue;
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            strings.push((field_no, text.to_string()));
        }
        if !strings.is_empty() {
            member_records.push(strings);
        }
    }

    let uses_current_layout = member_records.iter().any(|strings| {
        strings.iter().any(|(field_no, value)| {
            *field_no == 2 || (*field_no == 1 && target_set.contains(value.as_str()))
        })
    });

    let mut output: HashMap<String, String> = HashMap::new();
    if uses_current_layout {
        for strings in &member_records {
            let primary_targets: Vec<&str> = strings
                .iter()
                .filter(|(field_no, value)| {
                    *field_no == 1
                        && target_set.contains(value.as_str())
                        && !output.contains_key(value)
                })
                .map(|(_, value)| value.as_str())
                .collect();
            for target in primary_targets {
                let display = pick_display(strings, target, &[2]);
                if !display.is_empty() {
                    output.insert(target.to_string(), display);
                }
            }
            if output.len() >= target_set.len() {
                break;
            }
        }
        return output;
    }

    let legacy_records: Vec<&Vec<(u64, String)>> = member_records
        .iter()
        .filter(|strings| {
            strings
                .iter()
                .any(|(field_no, value)| *field_no == 4 && target_set.contains(value.as_str()))
        })
        .collect();
    if legacy_records.is_empty() {
        return HashMap::new();
    }
    for strings in &legacy_records {
        let field1_values: Vec<&str> = strings
            .iter()
            .filter(|(field_no, value)| *field_no == 1 && !value.trim().is_empty())
            .map(|(_, value)| value.as_str())
            .collect();
        if field1_values.is_empty() || field1_values.iter().any(|value| looks_like_username(value))
        {
            return HashMap::new();
        }
    }

    for strings in &legacy_records {
        let legacy_targets: Vec<&str> = strings
            .iter()
            .filter(|(field_no, value)| {
                *field_no == 4 && target_set.contains(value.as_str()) && !output.contains_key(value)
            })
            .map(|(_, value)| value.as_str())
            .collect();
        for target in legacy_targets {
            let display = pick_display(strings, target, &[1]);
            if !display.is_empty() {
                output.insert(target.to_string(), display);
            }
        }
        if output.len() >= target_set.len() {
            break;
        }
    }
    output
}

fn decode_varint(raw: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0;
    while pos < raw.len() {
        let byte = raw[pos];
        pos += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((value, pos));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

fn iter_fields(raw: &[u8]) -> Vec<(u64, &[u8])> {
    let mut fields = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        let Some((tag, next)) = decode_varint(raw, index) else {
            break;
        };
        if next <= index {
            break;
        }
        index = next;
        let field_no = tag >> 3;
        let wire_type = tag & 0x7;

        match wire_type {
            0 => {
                let Some((_, next)) = decode_varint(raw, index) else {
                    break;
                };
                if next <= index {
                    break;
                }
                index = next;
            }
            2 => {
                let Some((size, next)) = decode_varint(raw, index) else {
                    break;
                };
                if next <= index {
                    break;
                }
                index = next;
                let end = index.saturating_add(size as usize);
                if end > raw.len() {
                    break;
                }
                fields.push((field_no, &raw[index..end]));
                index = end;
            }
            1 => index += 8,
            5 => index += 4,
            _ => break,
        }
    }
    fields
}

fn is_strong_username_hint(value: &str) -> bool {
    value.starts_with("wxid_")
        || value.ends_with("@chatroom")
        || value.starts_with("gh_")
        || value.contains('@')
}

fn looks_like_username(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if is_strong_username_hint(value) {
        return true;
    }
    let length = value.chars().count();
    if !(6..=32).contains(&length) {
        return false;
    }
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
        return false;
    }
    !value.chars().all(|ch| ch.is_ascii_digit())
}

fn pick_display(strings: &[(u64, String)], target: &str, allowed_fields: &[u64]) -> String {
    let mut best_score: i64 = -1;
    let mut best = String::new();
    for (index, (field_no, value)) in strings.iter().enumerate() {
        if !allowed_fields.contains(field_no) {
            continue;
        }
        let value = value.trim();
        if value.is_empty() || value == target {
            continue;
        }
        if is_strong_username_hint(value) || value.contains(['\n', '\r']) {
            continue;
        }
        let length = value.chars().count();
        if length > 64 {
            continue;
        }

        let mut score: i64 = 0;
        if *field_no == 2 {
            score += 100;
        }
        if !looks_like_username(value) {
            score += 20;
        }
        score += (32i64 - length as i64).max(0);
        score = score * 1000 - index as i64;
        if score > best_score {
            best_score = score;
            best = value.to_string();
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut value: u64) -> Vec<u8> {
        let mut output = Vec::new();
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                output.push(byte);
                break;
            }
            output.push(byte | 0x80);
        }
        output
    }

    fn string_field(field_no: u64, value: &str) -> Vec<u8> {
        let mut output = varint((field_no << 3) | 2);
        output.extend(varint(value.len() as u64));
        output.extend_from_slice(value.as_bytes());
        output
    }

    fn member(fields: &[(u64, &str)]) -> Vec<u8> {
        let mut payload = Vec::new();
        for (field_no, value) in fields {
            payload.extend(string_field(*field_no, value));
        }
        let mut output = varint((1 << 3) | 2);
        output.extend(varint(payload.len() as u64));
        output.extend(payload);
        output
    }

    #[test]
    fn parses_current_layout_records() {
        let blob = [
            member(&[(1, "wxid_groupmember"), (2, "群昵称甲")]),
            member(&[(1, "wxid_other"), (2, "其他人")]),
        ]
        .concat();
        let records: Vec<Vec<(u64, String)>> = iter_fields(&blob)
            .into_iter()
            .map(|(_, chunk)| {
                iter_fields(chunk)
                    .into_iter()
                    .filter_map(|(field_no, value)| {
                        std::str::from_utf8(value)
                            .ok()
                            .map(|text| (field_no, text.to_string()))
                    })
                    .collect()
            })
            .collect();
        let section = &records[0];
        assert_eq!(pick_display(section, "wxid_groupmember", &[2]), "群昵称甲");
        assert!(looks_like_username("wxid_groupmember"));
        assert!(!looks_like_username("群昵称甲"));
    }

    #[test]
    fn varint_round_trip() {
        for value in [0, 1, 127, 128, 300, 1_700_000_000] {
            let encoded = varint(value);
            assert_eq!(decode_varint(&encoded, 0), Some((value, encoded.len())));
        }
    }
}
