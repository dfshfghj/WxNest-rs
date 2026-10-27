use std::collections::{BTreeMap, HashMap};

use chrono::TimeZone;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::error::Result;
use crate::sqlite::{row_value, SqlValue};

#[derive(Debug, Clone, Serialize)]
pub struct FinderLive {
    pub live_id: i64,
    pub username: String,
    pub export_id: String,
    pub live_status: i64,
    pub replay_status: i64,
    pub charge_flag: i64,
    pub display_name: String,
    pub description: String,
    pub profile_url: String,
    pub live_url: String,
    pub update_time: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MiniProgram {
    pub username: String,
    pub app_id: String,
    pub display_name: String,
    pub icon_url: String,
    pub last_update_time: i64,
    pub last_update_text: String,
    pub titles: Vec<String>,
    pub categories: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaymentItem {
    pub kind: String,
    pub id: String,
    pub session_name: String,
    pub username: String,
    pub status: String,
    pub status_code: i64,
    pub create_time: i64,
    pub create_time_text: String,
}

pub fn query_rows(connection: &Connection, sql: &str) -> Result<Vec<BTreeMap<String, SqlValue>>> {
    let mut statement = connection.prepare(sql)?;
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows = statement.query([])?;
    let mut output = Vec::new();
    while let Some(row) = rows.next()? {
        let mut map = BTreeMap::new();
        for (index, name) in names.iter().enumerate() {
            map.insert(name.clone(), row_value(row, index));
        }
        output.push(map);
    }
    Ok(output)
}

pub fn parse_finder_lives(
    status_rows: &[BTreeMap<String, SqlValue>],
    user_rows: &[BTreeMap<String, SqlValue>],
) -> Vec<FinderLive> {
    let users: HashMap<String, FinderUser> = user_rows
        .iter()
        .filter_map(|row| {
            let username = text(row, "username");
            let user = parse_finder_userpage(row.get("extra_buffer")?);
            (!username.is_empty()).then_some((username, user))
        })
        .collect();
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in status_rows {
        let username = text(row, "finder_username");
        let live_id = integer(row, "finder_live_id");
        if !seen.insert((username.clone(), live_id)) {
            continue;
        }
        let user = users.get(&username);
        let display_name = user
            .map(|user| user.nickname.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| username.clone());
        let export_id = text(row, "finder_export_id");
        output.push(FinderLive {
            live_id,
            username: username.clone(),
            export_id: export_id.clone(),
            live_status: integer(row, "live_status"),
            replay_status: integer(row, "replay_status"),
            charge_flag: integer(row, "charge_flag"),
            display_name,
            description: user
                .map(|user| user.description.clone())
                .unwrap_or_default(),
            profile_url: if username.is_empty() {
                String::new()
            } else {
                format!(
                    "https://channels.weixin.qq.com/web/pages/profile?username={}",
                    username
                )
            },
            live_url: if export_id.is_empty() {
                String::new()
            } else {
                format!("https://channels.weixin.qq.com/web/pages/live?eid={export_id}")
            },
            update_time: user.map(|user| user.update_time).unwrap_or(0),
        });
    }
    output.sort_by(|left, right| {
        right
            .live_status
            .cmp(&left.live_status)
            .then_with(|| right.update_time.cmp(&left.update_time))
    });
    output
}

pub fn parse_mini_programs(rows: &[BTreeMap<String, SqlValue>]) -> Vec<MiniProgram> {
    let mut output = Vec::new();
    for row in rows {
        let username = text(row, "user_name");
        if username.is_empty() {
            continue;
        }
        let summary = mini_summary(row.get("external_info"));
        let display_name = summary
            .titles
            .first()
            .cloned()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| username.clone());
        let last_update_time = integer(row, "last_update_time");
        output.push(MiniProgram {
            username,
            app_id: text(row, "app_id"),
            display_name,
            icon_url: text(row, "brand_icon_url"),
            last_update_time,
            last_update_text: format_time(last_update_time),
            titles: summary.titles,
            categories: summary.categories,
            summary: summary.summary,
        });
    }
    output
}

pub fn parse_payments(
    transfer_rows: &[BTreeMap<String, SqlValue>],
    red_rows: &[BTreeMap<String, SqlValue>],
) -> Vec<PaymentItem> {
    let mut output = Vec::new();
    for row in transfer_rows {
        let pay_sub_type = integer(row, "pay_sub_type");
        let time = integer(row, "begin_transfer_time").max(integer(row, "last_modified_time"));
        output.push(PaymentItem {
            kind: "transfer".to_string(),
            id: text(row, "transfer_id"),
            session_name: text(row, "session_name"),
            username: text(row, "pay_payer"),
            status: transfer_status(pay_sub_type).to_string(),
            status_code: pay_sub_type,
            create_time: time,
            create_time_text: format_time(time),
        });
    }
    for row in red_rows {
        let status = integer(row, "receive_status");
        let hb_status = integer(row, "hb_status");
        output.push(PaymentItem {
            kind: "red_packet".to_string(),
            id: integer(row, "message_server_id").to_string(),
            session_name: text(row, "session_name"),
            username: text(row, "sender_user_name"),
            status: red_packet_status(hb_status, status).to_string(),
            status_code: status,
            create_time: 0,
            create_time_text: String::new(),
        });
    }
    output.sort_by_key(|left| std::cmp::Reverse(left.create_time));
    output
}

struct FinderUser {
    nickname: String,
    description: String,
    update_time: i64,
}

fn parse_finder_userpage(value: &SqlValue) -> FinderUser {
    let Some(bytes) = value.bytes() else {
        return FinderUser {
            nickname: String::new(),
            description: String::new(),
            update_time: 0,
        };
    };
    let mut user = FinderUser {
        nickname: String::new(),
        description: String::new(),
        update_time: 0,
    };
    let mut index = 0usize;
    while index < bytes.len() {
        let Some((tag, next)) = varint(&bytes, index) else {
            break;
        };
        index = next;
        let field = tag >> 3;
        match tag & 0x7 {
            0 => {
                let Some((value, next)) = varint(&bytes, index) else {
                    break;
                };
                index = next;
                if field == 5 {
                    user.update_time = value as i64;
                }
            }
            2 => {
                let Some((size, next)) = varint(&bytes, index) else {
                    break;
                };
                let end = next.saturating_add(size as usize);
                if end > bytes.len() {
                    break;
                }
                let text = printable(&bytes[next..end]);
                index = end;
                match field {
                    2 if !text.is_empty() => user.nickname = text,
                    4 if !text.is_empty() => user.description = text,
                    _ => {}
                }
            }
            1 => index = index.saturating_add(8),
            5 => index = index.saturating_add(4),
            _ => break,
        }
        if index > bytes.len() {
            break;
        }
    }
    user
}

fn varint(data: &[u8], mut index: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(index)?;
        index += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, index));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn printable(bytes: &[u8]) -> String {
    let text = crate::sqlite::decode_utf8_ignore(bytes)
        .replace('\0', "")
        .trim()
        .to_string();
    if text.is_empty() {
        return text;
    }
    let printable = text.chars().filter(|ch| !ch.is_control()).count();
    if printable * 4 >= text.chars().count() * 3 {
        text
    } else {
        String::new()
    }
}

struct MiniSummary {
    titles: Vec<String>,
    categories: Vec<String>,
    summary: String,
}

fn mini_summary(value: Option<&SqlValue>) -> MiniSummary {
    let Some(value) = value else {
        return MiniSummary {
            titles: Vec::new(),
            categories: Vec::new(),
            summary: String::new(),
        };
    };
    let raw = value.bytes().unwrap_or_default();
    let text = crate::sqlite::decode_utf8_ignore(&raw);
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return MiniSummary {
            titles: Vec::new(),
            categories: Vec::new(),
            summary: String::new(),
        };
    };
    let bind = json.get("BindWxaInfo");
    let mut titles = Vec::new();
    for key in ["bizEntryInfo", "wxaEntryInfo"] {
        for entry in bind
            .and_then(|value| value.get(key))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for field in ["title", "nickname", "name"] {
                if let Some(title) = entry.get(field).and_then(Value::as_str) {
                    let title = title.trim();
                    if !title.is_empty() && !titles.iter().any(|value| value == title) {
                        titles.push(title.to_string());
                        break;
                    }
                }
            }
        }
    }
    let categories: Vec<String> = json
        .get("WxaAppDynamic")
        .and_then(|value| value.get("NewCategories"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| match entry {
            Value::String(value) => Some(value.clone()),
            Value::Object(_) => entry
                .get("name")
                .or_else(|| entry.get("category"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .filter(|value| !value.trim().is_empty())
        .collect();
    let register = json
        .get("RegisterSource")
        .and_then(|value| value.get("RegisterBody"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let summary = if !register.is_empty() {
        register
    } else {
        categories.join(" · ")
    };
    titles.truncate(8);
    MiniSummary {
        titles,
        categories,
        summary,
    }
}

fn transfer_status(pay_sub_type: i64) -> &'static str {
    match pay_sub_type {
        1 => "转账",
        3 => "已收款",
        4 | 9 => "已退还",
        8 => "发起转账",
        10 => "已过期",
        _ => "转账",
    }
}

fn red_packet_status(hb_status: i64, receive_status: i64) -> &'static str {
    if receive_status == 2 {
        "已领取"
    } else if hb_status == 2 {
        "已退款"
    } else {
        "微信红包"
    }
}

fn text(row: &BTreeMap<String, SqlValue>, key: &str) -> String {
    row.get(key)
        .map(SqlValue::text_lossy)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn integer(row: &BTreeMap<String, SqlValue>, key: &str) -> i64 {
    row.get(key).and_then(SqlValue::as_i64).unwrap_or(0)
}

fn format_time(timestamp: i64) -> String {
    if timestamp <= 0 {
        return String::new();
    }
    chrono::Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_finder_userpage_header() {
        let mut data = vec![0x12, 0x06];
        data.extend_from_slice("昵称".as_bytes());
        data.extend_from_slice(&[0x28, 0x64]);
        data.push(0x32);
        data.push(9);
        data.extend_from_slice(b"https://");
        let user = parse_finder_userpage(&SqlValue::Blob(data));
        assert_eq!(user.nickname, "昵称");
        assert_eq!(user.update_time, 100);
    }

    #[test]
    fn parses_mini_program_titles() {
        let json = r#"{"BindWxaInfo":{"bizEntryInfo":[{"title":"标题"}]},"WxaAppDynamic":{"NewCategories":["工具"]}}"#;
        let summary = mini_summary(Some(&SqlValue::Text(json.into())));
        assert_eq!(summary.titles, vec!["标题"]);
        assert_eq!(summary.categories, vec!["工具"]);
    }
}
