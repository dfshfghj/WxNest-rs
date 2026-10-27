use std::collections::{HashMap, HashSet};

use md5::{Digest, Md5};
use rusqlite::params_from_iter;

use crate::account::{AccountStore, MessageShard};
use crate::decode::decode_message_content;
use crate::error::Result;
use crate::group::load_group_nickname_map;
use crate::message_body::{
    message_wxid_references, parse_message_body_with_resolver, strip_group_sender_prefix,
};
use crate::model::{Message, MessageCursor, MessageKind, MessagePage};
use crate::sqlite::{
    column_set, open_read_only, quote_ident, row_i64, row_optional_i64, row_text, row_value,
    table_columns, table_exists, table_has_column, table_names,
};

#[derive(Debug, Clone)]
pub struct MessageQuery {
    pub limit: usize,
    pub before: Option<MessageCursor>,
}

impl Default for MessageQuery {
    fn default() -> Self {
        Self {
            limit: 50,
            before: None,
        }
    }
}

pub(crate) fn load_messages_for_store(
    store: &AccountStore,
    username: &str,
    query: &MessageQuery,
) -> Result<MessagePage> {
    let username = username.trim();
    let limit = query.limit.clamp(1, 500);
    if username.is_empty() {
        return Ok(MessagePage {
            messages: Vec::new(),
            has_more: false,
            next_before: None,
        });
    }

    let mut all = Vec::new();
    let mut any_extra = false;
    for shard in store.shards() {
        let Some(table) = shard.resolve_table(username) else {
            continue;
        };
        let connection = open_read_only(&shard.path)?;
        let mut rows = query_shard(&connection, shard, &table, query.before.as_ref(), limit + 1)?;
        if rows.len() > limit {
            any_extra = true;
        }
        all.append(&mut rows);
    }

    all.sort_by(|left, right| {
        right
            .create_time
            .cmp(&left.create_time)
            .then_with(|| right.sort_seq.cmp(&left.sort_seq))
            .then_with(|| right.local_id.cmp(&left.local_id))
            .then_with(|| right.shard.cmp(&left.shard))
    });
    let has_more = any_extra || all.len() > limit;
    all.truncate(limit);
    all.reverse();
    attach_sender_display_names(store, username, &mut all);

    let next_before = if has_more {
        all.first().map(|message| MessageCursor {
            create_time: message.create_time,
            sort_seq: message.sort_seq,
            local_id: message.local_id,
            shard: message.shard.clone(),
        })
    } else {
        None
    };

    Ok(MessagePage {
        messages: all,
        has_more,
        next_before,
    })
}

fn attach_sender_display_names(store: &AccountStore, username: &str, messages: &mut [Message]) {
    let is_group = username.ends_with("@chatroom");
    let mut referenced: Vec<String> = messages
        .iter()
        .map(|message| message.sender_username.trim().to_string())
        .filter(|sender| !sender.is_empty())
        .collect();
    for message in messages.iter() {
        referenced.extend(message_wxid_references(&message.content));
    }
    referenced.sort();
    referenced.dedup();
    let contacts = store.load_contacts_for(&referenced).unwrap_or_default();
    let group_nicknames = if is_group {
        load_group_nickname_map(store.contact_db_path(), username, &referenced).unwrap_or_default()
    } else {
        HashMap::new()
    };
    for message in messages {
        let sender = message.sender_username.trim().to_string();
        let group_name = group_nicknames
            .get(&sender)
            .filter(|nickname| !nickname.trim().is_empty())
            .cloned();
        let sender_display = group_name
            .clone()
            .or_else(|| {
                contacts
                    .get(&sender)
                    .map(|contact| contact.display_name.clone())
            })
            .unwrap_or_else(|| sender.clone());
        message.sender_display_name = sender_display.clone();
        if is_group && !sender.is_empty() {
            strip_group_sender_prefix(message, &sender_display, group_name.as_deref());
        }
        let resolve = |wxid: &str| -> String {
            group_nicknames
                .get(wxid)
                .filter(|nickname| !nickname.trim().is_empty())
                .cloned()
                .or_else(|| {
                    contacts
                        .get(wxid)
                        .map(|contact| contact.display_name.clone())
                })
                .or_else(|| (wxid == store.self_username()).then(|| "我".to_string()))
                .unwrap_or_default()
        };
        message.body =
            parse_message_body_with_resolver(message.local_type, &message.content, &resolve);
    }
}

fn query_shard(
    connection: &rusqlite::Connection,
    shard: &MessageShard,
    table: &str,
    before: Option<&MessageCursor>,
    limit: usize,
) -> Result<Vec<Message>> {
    let columns = table_columns(connection, table)?;
    let columns = column_set(&columns);
    if !table_has_column(&columns, "local_id") {
        return Ok(Vec::new());
    }

    let local_id = "m.local_id";
    let server_id = if table_has_column(&columns, "server_id") {
        "m.server_id"
    } else {
        "0"
    };
    let create_time = if table_has_column(&columns, "create_time") {
        "m.create_time"
    } else {
        "0 + 0"
    };
    let sort_seq = if table_has_column(&columns, "sort_seq") {
        "m.sort_seq"
    } else {
        "0 + 0"
    };
    let local_type = if table_has_column(&columns, "local_type") {
        "m.local_type"
    } else {
        "0"
    };
    let message_content = if table_has_column(&columns, "message_content") {
        "m.message_content"
    } else {
        "NULL"
    };
    let compress_content = if table_has_column(&columns, "compress_content") {
        "m.compress_content"
    } else {
        "NULL"
    };
    let real_sender_id = if table_has_column(&columns, "real_sender_id") {
        "m.real_sender_id"
    } else {
        "NULL"
    };
    let packed_info = if table_has_column(&columns, "packed_info_data") {
        "m.packed_info_data"
    } else {
        "NULL"
    };

    let name2id = find_name2id_table(connection)?;
    let (join, sender_username) = match &name2id {
        Some(table) if table_has_column(&columns, "real_sender_id") => (
            format!(
                "LEFT JOIN {} n ON n.rowid = m.real_sender_id",
                quote_ident(table)
            ),
            "n.user_name",
        ),
        _ => (String::new(), "NULL"),
    };

    let has_sort_seq = table_has_column(&columns, "sort_seq");
    let (where_clause, order_clause) = if has_sort_seq {
        (
            if before.is_some() {
                format!("WHERE ({sort_seq}, {local_id}) < (?1, ?2)")
            } else {
                String::new()
            },
            format!("ORDER BY {sort_seq} DESC, {local_id} DESC"),
        )
    } else {
        (
            if before.is_some() {
                format!("WHERE ({create_time}, {sort_seq}, {local_id}) < (?1, ?2, ?3)")
            } else {
                String::new()
            },
            format!("ORDER BY {create_time} DESC, {sort_seq} DESC, {local_id} DESC"),
        )
    };
    let sql = format!(
        "SELECT {local_id} AS local_id, {server_id} AS server_id, \
                {create_time} AS create_time, {sort_seq} AS sort_seq, \
                {local_type} AS local_type, {message_content} AS message_content, \
                {compress_content} AS compress_content, {real_sender_id} AS real_sender_id, \
                {packed_info} AS packed_info, {sender_username} AS sender_username \
         FROM {} m {join} {where_clause} {order_clause} LIMIT ?",
        quote_ident(table)
    );

    let mut parameters: Vec<i64> = Vec::new();
    if let Some(cursor) = before {
        if has_sort_seq {
            parameters.push(cursor.sort_seq);
            parameters.push(cursor.local_id);
        } else {
            parameters.push(cursor.create_time);
            parameters.push(cursor.sort_seq);
            parameters.push(cursor.local_id);
        }
    }
    parameters.push(limit as i64);

    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(parameters))?;
    let mut output = Vec::new();
    let mut seen: HashSet<(i64, i64, i64)> = HashSet::new();
    while let Some(row) = rows.next()? {
        let local_id = row_i64(row, 0);
        let server_id = row_i64(row, 1);
        let create_time = row_i64(row, 2);
        let sort_seq = row_i64(row, 3);
        let local_type = row_i64(row, 4);
        let message_value = row_value(row, 5);
        let compress_value = row_value(row, 6);
        let compress_ref = if compress_value.is_null() {
            None
        } else {
            Some(&compress_value)
        };
        let real_sender_id = row_optional_i64(row, 7);
        let packed_value = row_value(row, 8);
        let sender_username = row_text(row, 9);

        if !seen.insert((create_time, sort_seq, local_id)) {
            continue;
        }

        let content = decode_message_content(compress_ref, &message_value);
        let media_md5 = crate::media_index::extract_md5_from_packed_info(&packed_value);
        output.push(Message {
            local_id,
            server_id,
            create_time,
            sort_seq,
            local_type,
            kind: MessageKind::from_local_type(local_type),
            sender_username,
            sender_display_name: String::new(),
            is_sent: shard.self_rowid.is_some() && shard.self_rowid == real_sender_id,
            body: None,
            content,
            media_md5: (!media_md5.is_empty()).then_some(media_md5),
            shard: shard.name.clone(),
            table: table.to_string(),
        });
    }
    Ok(output)
}

fn find_name2id_table(connection: &rusqlite::Connection) -> Result<Option<String>> {
    if table_exists(connection, "Name2Id")? {
        return Ok(Some("Name2Id".into()));
    }
    for name in table_names(connection)? {
        if name.eq_ignore_ascii_case("Name2Id") {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

pub(crate) fn resolve_table_name(table_names: &[String], username: &str) -> Option<String> {
    if username.is_empty() {
        return None;
    }
    let mut digest = Md5::new();
    digest.update(username.as_bytes());
    let md5_hex = hex::encode(digest.finalize());
    let expected = format!("msg_{md5_hex}");
    let expected_chat = format!("chat_{md5_hex}");
    let partial = &md5_hex[..24];

    for name in table_names {
        if name.eq_ignore_ascii_case(&expected) {
            return Some(name.clone());
        }
    }
    for name in table_names {
        if name.eq_ignore_ascii_case(&expected_chat) {
            return Some(name.clone());
        }
    }
    for name in table_names {
        let lower = name.to_lowercase();
        if (lower.starts_with("msg_") || lower.starts_with("chat_")) && lower.contains(&md5_hex) {
            return Some(name.clone());
        }
    }
    for name in table_names {
        if name.to_lowercase().contains(&md5_hex) {
            return Some(name.clone());
        }
    }
    for name in table_names {
        if name.to_lowercase().contains(partial) {
            return Some(name.clone());
        }
    }
    None
}

pub(crate) fn resolve_self_rowid(
    connection: &rusqlite::Connection,
    candidates: &[String],
) -> Option<i64> {
    let table = find_name2id_table(connection).ok()??;
    let sql = format!(
        "SELECT rowid FROM {} WHERE user_name = ?1 LIMIT 1",
        quote_ident(&table)
    );
    for candidate in candidates {
        if let Ok(row) = connection.query_row(&sql, [candidate], |row| row.get::<_, i64>(0)) {
            return Some(row);
        }
    }
    None
}
