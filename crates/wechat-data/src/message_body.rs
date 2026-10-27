use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use serde::Serialize;

use crate::model::{Message, MessageKind};

fn cached_regex(pattern: &str) -> Option<Arc<Regex>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Regex>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(regex) = map.get(pattern) {
            return Some(regex.clone());
        }
    }
    let regex = Arc::new(Regex::new(pattern).ok()?);
    if let Ok(mut map) = cache.lock() {
        map.insert(pattern.to_string(), regex.clone());
    }
    Some(regex)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "renderType",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MessageBody {
    Text {
        content: String,
    },
    Link {
        content: String,
        title: String,
        url: String,
        thumb_url: String,
        from: String,
        from_username: String,
        link_type: String,
        link_style: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        object_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        object_nonce_id: Option<String>,
    },
    ChatHistory {
        content: String,
        title: String,
        record_item: String,
    },
    File {
        content: String,
        title: String,
        size: String,
        file_md5: String,
    },
    Quote {
        content: String,
        quote_username: String,
        quote_title: String,
        quote_content: String,
        quote_type: String,
        quote_thumb_url: String,
        quote_server_id: String,
        quote_voice_length: String,
    },
    Transfer {
        content: String,
        title: String,
        amount: String,
        pay_sub_type: String,
        receive_status: String,
        sender_title: String,
        receiver_title: String,
        sender_des: String,
        receiver_des: String,
        transfer_id: String,
        invalid_time: String,
    },
    RedPacket {
        content: String,
        title: String,
        cover_url: String,
    },
    Location {
        content: String,
        location_lat: Option<f64>,
        location_lng: Option<f64>,
        location_poiname: String,
        location_label: String,
    },
    Voip {
        content: String,
        voip_type: String,
    },
    System {
        content: String,
    },
}

pub fn parse_message_body(local_type: i64, content: &str) -> Option<MessageBody> {
    parse_message_body_with_resolver(local_type, content, &|wxid| wxid.to_string())
}

pub fn parse_message_body_with_resolver(
    local_type: i64,
    content: &str,
    resolve: &dyn Fn(&str) -> String,
) -> Option<MessageBody> {
    match MessageKind::from_local_type(local_type) {
        MessageKind::App | MessageKind::Quote => Some(parse_app_body(content, resolve)),
        MessageKind::Location => Some(parse_location_message(content)),
        MessageKind::Voip => Some(parse_voip_message(content)),
        MessageKind::Pat => Some(MessageBody::System {
            content: parse_pat_message(content, resolve),
        }),

        MessageKind::Emoji => None,
        MessageKind::System => Some(MessageBody::System {
            content: parse_system_message_content(content, &|username, fallback| {
                let resolved = resolve(username);
                if resolved.is_empty() {
                    fallback.to_string()
                } else {
                    resolved
                }
            }),
        }),
        _ => {
            let lower = content.to_ascii_lowercase();
            if lower.contains("<appmsg") {
                Some(parse_app_body(content, resolve))
            } else {
                None
            }
        }
    }
}

fn parse_app_body(content: &str, resolve: &dyn Fn(&str) -> String) -> MessageBody {
    let mut body = parse_app_message(content);
    if let MessageBody::Quote {
        quote_username,
        quote_title,
        ..
    } = &mut body
    {
        if quote_title.trim().is_empty() && !quote_username.trim().is_empty() {
            let resolved = resolve(quote_username);
            if !resolved.is_empty() {
                *quote_title = resolved;
            }
        }
    }
    body
}

pub fn message_wxid_references(content: &str) -> Vec<String> {
    let mut output: Vec<String> = Vec::new();
    if content.is_empty() {
        return output;
    }
    for tag in ["username", "fromusername", "fromusr", "newusername"] {
        let value = extract_tag_text(content, tag);
        if !value.is_empty() && !output.contains(&value) {
            output.push(value);
        }
    }
    let Some(pattern) = cached_regex(r"\$\{([^}]+)\}") else {
        return output;
    };
    for captures in pattern.captures_iter(content) {
        if let Some(value) = captures.get(1) {
            let value = value.as_str().trim().to_string();
            if !value.is_empty() && !output.contains(&value) {
                output.push(value);
            }
        }
    }
    output
}

pub fn strip_group_sender_prefix(
    message: &mut Message,
    sender_display_name: &str,
    group_nickname: Option<&str>,
) {
    let Some((prefix, rest)) = message.content.split_once('\n') else {
        return;
    };
    let prefix = prefix.trim().trim_end_matches(':').trim();
    if prefix.is_empty() || prefix.len() > 64 {
        return;
    }
    let sender = message.sender_username.trim();
    if prefix == sender || prefix == sender_display_name || group_nickname == Some(prefix) {
        message.content = rest.to_string();
    }
}

fn strip_cdata(value: &str) -> String {
    value
        .replace("<![CDATA[", "")
        .replace("]]>", "")
        .trim()
        .to_string()
}

fn tag_pattern(tag: &str) -> String {
    format!(
        r"(?is)<{}>(.*?)</{}>",
        regex::escape(tag),
        regex::escape(tag)
    )
}

pub(crate) fn extract_tag_text(xml: &str, tag: &str) -> String {
    if xml.is_empty() || tag.is_empty() {
        return String::new();
    }
    let Some(pattern) = cached_regex(&tag_pattern(tag)) else {
        return String::new();
    };
    pattern
        .captures(xml)
        .and_then(|captures| captures.get(1))
        .map(|value| strip_cdata(value.as_str()))
        .unwrap_or_default()
}

pub(crate) fn extract_attr(xml: &str, attr: &str) -> String {
    if xml.is_empty() || attr.is_empty() {
        return String::new();
    }
    let pattern = format!(r#"(?i){}\s*=\s*['"]([^'"]+)['"]"#, regex::escape(attr));
    let Some(pattern) = cached_regex(&pattern) else {
        return String::new();
    };
    pattern
        .captures(xml)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn extract_tag_or_attr(xml: &str, name: &str) -> String {
    let value = extract_tag_text(xml, name);
    if !value.is_empty() {
        return value;
    }
    extract_attr(xml, name)
}

fn normalize_xml_url(value: &str) -> String {
    let url = value.trim();
    if url.is_empty() {
        return String::new();
    }
    html_escape::decode_html_entities(url).trim().to_string()
}

fn remove_block(xml: &str, block: &str) -> String {
    let pattern = format!(
        r"(?is)<{}\b[^>]*>.*?</{}>",
        regex::escape(block),
        regex::escape(block)
    );
    match cached_regex(&pattern) {
        Some(pattern) => pattern.replace_all(xml, "").to_string(),
        None => xml.to_string(),
    }
}

fn extract_block_ci<'a>(xml: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let lower = xml.to_ascii_lowercase();
    let start = lower.find(&open.to_ascii_lowercase())?;
    let end = lower[start..].find(&close.to_ascii_lowercase())? + start + close.len();
    Some(&xml[start..end])
}

fn collapse_whitespace(value: &str) -> String {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| Regex::new(r"\s+").expect("valid whitespace regex"));
    pattern.replace_all(value, " ").trim().to_string()
}

fn extract_appmsg_type(text: &str) -> i64 {
    let Some(pattern) = cached_regex(r"(?is)<appmsg\b[^>]*>(.*?)</appmsg>") else {
        return 0;
    };
    if let Some(captures) = pattern.captures(text) {
        let mut inner = captures
            .get(1)
            .map(|value| value.as_str().to_string())
            .unwrap_or_default();
        for block in [
            "refermsg",
            "patmsg",
            "recorditem",
            "weappinfo",
            "wxaappinfo",
        ] {
            inner = remove_block(&inner, block);
        }
        return extract_tag_text(&inner, "type").trim().parse().unwrap_or(0);
    }
    extract_tag_text(text, "type").trim().parse().unwrap_or(0)
}

fn extract_refermsg_block(xml: &str) -> String {
    let Some(pattern) = cached_regex(r"(?is)(<refermsg[^>]*>.*?</refermsg>)") else {
        return String::new();
    };
    pattern
        .captures(xml)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().trim().to_string())
        .unwrap_or_default()
}

fn extract_refermsg_content(refer_block: &str) -> String {
    if refer_block.is_empty() {
        return String::new();
    }
    let Some(pattern) = cached_regex(r"(?is)<content\b[^>]*>\s*<!\[CDATA\[(.*?)\]\]>\s*</content>")
    else {
        return extract_tag_text(refer_block, "content");
    };
    if let Some(captures) = pattern.captures(refer_block) {
        return captures
            .get(1)
            .map(|value| value.as_str().trim().to_string())
            .unwrap_or_default();
    }
    extract_tag_text(refer_block, "content")
}

fn summarize_nested_quote_content(raw: &str) -> String {
    let candidate = raw.trim();
    if candidate.is_empty() {
        return String::new();
    }
    let lower = candidate.to_lowercase();
    if !lower.contains("<msg") && !lower.contains("<appmsg") {
        return candidate.to_string();
    }
    for tag in ["title", "des"] {
        let value = extract_tag_text(candidate, tag);
        if !value.is_empty() {
            return value;
        }
    }
    let content = extract_tag_text(candidate, "content");
    if !content.is_empty() && !content.trim_start().starts_with('<') {
        return content;
    }
    String::new()
}

fn extract_nested_quote_thumb_url(raw: &str) -> String {
    let candidate = raw.trim();
    if candidate.is_empty() {
        return String::new();
    }
    let mut probes = vec![candidate.to_string()];
    if candidate.starts_with("wxid_") {
        if let Some(colon) = candidate.find(':') {
            if colon > 0 && colon <= 64 {
                let rest = candidate[colon + 1..].trim();
                if !rest.is_empty() {
                    probes.push(rest.to_string());
                }
            }
        }
    }
    for probe in probes {
        for key in ["thumburl", "cdnthumburl", "cdnthumurl", "coverurl", "cover"] {
            let value = normalize_xml_url(&extract_tag_or_attr(&probe, key));
            if !value.is_empty() {
                return value;
            }
        }
    }
    String::new()
}

fn is_mp_weixin_article_url(url: &str) -> bool {
    let lower = url.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    if lower.contains("mp.weixin.qq.com/") {
        return true;
    }
    let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    host == "mp.weixin.qq.com" || host.ends_with(".mp.weixin.qq.com")
}

fn is_mp_weixin_feed_article_url(url: &str) -> bool {
    let lower = url.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    if lower.contains("exptype=masonry_feed") {
        return true;
    }
    let query = lower.split_once('?').map(|(_, query)| query).unwrap_or("");
    query.split('&').any(|segment| {
        segment
            .strip_prefix("exptype=")
            .map(|value| value.contains("masonry_feed"))
            .unwrap_or(false)
    })
}

fn classify_link_share(
    app_type: i64,
    url: &str,
    source_username: &str,
    desc: &str,
) -> (String, String) {
    let source = source_username.trim().to_lowercase();
    let is_official_article = (app_type == 5 || app_type == 68)
        && (is_mp_weixin_article_url(url) || source.starts_with("gh_"));

    let link_type = if is_official_article {
        "official_article"
    } else {
        "web_link"
    };

    let desc = desc.trim();
    let hashtag_count = cached_regex(r"#[^#\s]+")
        .map(|pattern| pattern.find_iter(desc).count())
        .unwrap_or(0);
    let cover_like = is_official_article
        && (desc.starts_with('#') || hashtag_count >= 2 || is_mp_weixin_feed_article_url(url));

    (
        link_type.to_string(),
        if cover_like { "cover" } else { "default" }.to_string(),
    )
}

fn first_non_empty<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values
        .into_iter()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn to_float(value: &str) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parsed: f64 = value.parse().ok()?;
    (-180.0..=180.0).contains(&parsed).then_some(parsed)
}

fn clean_location_text(value: &str) -> String {
    let candidate = strip_cdata(value.trim());
    if candidate.is_empty() {
        return String::new();
    }
    collapse_whitespace(&html_escape::decode_html_entities(&candidate))
}

pub fn parse_location_message(text: &str) -> MessageBody {
    let raw = html_escape::decode_html_entities(text.trim()).to_string();

    let poiname = clean_location_text(&first_non_empty([
        extract_tag_or_attr(&raw, "poiname").as_str(),
        extract_tag_or_attr(&raw, "poiName").as_str(),
        extract_tag_or_attr(&raw, "name").as_str(),
    ]));
    let label = clean_location_text(&first_non_empty([
        extract_tag_or_attr(&raw, "label").as_str(),
        extract_tag_or_attr(&raw, "labelname").as_str(),
        extract_tag_or_attr(&raw, "address").as_str(),
    ]));

    let mut lat = to_float(&first_non_empty([
        extract_tag_or_attr(&raw, "x").as_str(),
        extract_tag_or_attr(&raw, "latitude").as_str(),
        extract_tag_or_attr(&raw, "lat").as_str(),
    ]));
    if lat.is_some_and(|value| !(-90.0..=90.0).contains(&value)) {
        lat = None;
    }
    let lng = to_float(&first_non_empty([
        extract_tag_or_attr(&raw, "y").as_str(),
        extract_tag_or_attr(&raw, "longitude").as_str(),
        extract_tag_or_attr(&raw, "lng").as_str(),
        extract_tag_or_attr(&raw, "lon").as_str(),
    ]));

    let title = first_non_empty([poiname.as_str(), label.as_str(), "位置"]);
    MessageBody::Location {
        content: if title.is_empty() {
            "[Location]".to_string()
        } else {
            title
        },
        location_lat: lat,
        location_lng: lng,
        location_poiname: poiname,
        location_label: label,
    }
}

pub fn parse_voip_message(text: &str) -> MessageBody {
    let raw = html_escape::decode_html_entities(text.trim()).to_string();

    let block =
        extract_block_ci(&raw, "<VoIPBubbleMsg", "</VoIPBubbleMsg>").unwrap_or(raw.as_str());
    let room_type = extract_tag_text(block, "room_type");
    let voip_type = match room_type.trim() {
        "0" => "video",
        "1" => "audio",
        _ => "",
    };
    let content = first_non_empty([
        extract_tag_text(block, "diaplay_content").as_str(),
        extract_tag_text(block, "display_content").as_str(),
        extract_tag_text(block, "msg").as_str(),
        extract_tag_text(block, "wording").as_str(),
        "通话",
    ]);
    MessageBody::Voip {
        content,
        voip_type: voip_type.to_string(),
    }
}

pub fn parse_pat_message(text: &str, resolve: &dyn Fn(&str) -> String) -> String {
    let template = extract_tag_text(text, "template");
    if template.is_empty() {
        return "[拍一拍]".to_string();
    }
    let Some(pattern) = cached_regex(r"\$\{([^}]+)\}") else {
        return "[拍一拍]".to_string();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut rendered = template.clone();
    for captures in pattern.captures_iter(&template) {
        let Some(wxid) = captures.get(1).map(|value| value.as_str()) else {
            continue;
        };
        if !seen.insert(wxid.to_string()) {
            continue;
        }
        let name = resolve(wxid);
        let name = if name.trim().is_empty() {
            "有人".to_string()
        } else {
            name
        };
        rendered = rendered.replace(&format!("${{{wxid}}}"), &name);
    }
    let rendered = rendered.trim().to_string();
    if rendered.is_empty() {
        "[拍一拍]".to_string()
    } else {
        rendered
    }
}

fn parse_sysmsg_template(text: &str, resolve: &dyn Fn(&str, &str) -> String) -> Option<String> {
    let template = extract_tag_text(text, "template");
    if template.is_empty() {
        return None;
    }
    let link_pattern = cached_regex(r#"(?is)<link\s+name=["']([^"']+)["'][^>]*>(.*?)</link>"#)?;
    let member_pattern = cached_regex(r"(?is)<member>(.*?)</member>")?;
    let mut rendered = template;
    for captures in link_pattern.captures_iter(text) {
        let name = captures.get(1).map(|value| value.as_str()).unwrap_or("");
        let block = captures.get(2).map(|value| value.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let separator = {
            let value = extract_tag_text(block, "separator");
            if value.is_empty() {
                "、".to_string()
            } else {
                value
            }
        };
        let mut members: Vec<String> = Vec::new();
        for member in member_pattern.captures_iter(block) {
            let body = member.get(1).map(|value| value.as_str()).unwrap_or("");
            let username = extract_tag_text(body, "username");
            let nickname = extract_tag_text(body, "nickname");
            let display = resolve(&username, &nickname);
            let display = if display.trim().is_empty() {
                nickname
            } else {
                display
            };
            if !display.trim().is_empty() {
                members.push(display.trim().to_string());
            }
        }
        if !members.is_empty() {
            rendered = rendered.replace(&format!("${name}$"), &members.join(&separator));
        }
    }
    let leftover = cached_regex(r"\$[A-Za-z_][A-Za-z0-9_]*\$")?;
    rendered = leftover.replace_all(&rendered, "有人").to_string();
    Some(collapse_whitespace(&rendered))
}

fn clean_system_text(value: &str) -> String {
    let candidate = value.trim();
    if candidate.is_empty() {
        return String::new();
    }
    let nested = extract_tag_text(candidate, "content");
    let mut candidate = if nested.is_empty() {
        candidate.to_string()
    } else {
        nested
    };

    if let Some(pattern) = cached_regex(r"(?is)<!--.*?-->") {
        candidate = pattern.replace_all(&candidate, " ").to_string();
    }
    candidate = candidate.replace("<![CDATA[", "").replace("]]>", "");
    if let Some(pattern) = cached_regex(r"(?is)</?[_a-zA-Z0-9]+[^>]*>") {
        candidate = pattern.replace_all(&candidate, "").to_string();
    }
    collapse_whitespace(&candidate)
}

fn is_int_token(value: &str) -> bool {
    let candidate = value.strip_prefix(['+', '-']).unwrap_or(value);
    !candidate.is_empty() && candidate.chars().all(|ch| ch.is_ascii_digit())
}

#[derive(Debug, Default)]
struct ChatroomTopMessage {
    operation: String,
    operator_username: String,
    operator_display_name: String,
}

fn extract_chatroom_top_message_metadata(raw_text: &str) -> Option<ChatroomTopMessage> {
    let text = raw_text.trim();
    if text.is_empty() {
        return None;
    }
    let lower_text = text.to_lowercase();

    if lower_text.contains("<mmchatroomtopmsg") || lower_text.contains("<sysmsg") {
        let chatroom_id = extract_tag_text(text, "chatroomname");
        let operation = extract_tag_text(text, "op");
        let operator_username = extract_tag_text(text, "username");
        let operator_display_name = extract_tag_text(text, "nickname");
        if chatroom_id.ends_with("@chatroom")
            && (operation == "1" || operation == "2")
            && !operator_username.is_empty()
        {
            return Some(ChatroomTopMessage {
                operation,
                operator_username,
                operator_display_name,
            });
        }
    }

    let mut normalized = text.to_string();
    for marker in ["ChatRoomTopMsgRequest", "ChatRoomTopMsgResponse"] {
        let pattern = format!(r"(?is)<!--\s*{marker}\s*-->");
        if let Some(pattern) = cached_regex(&pattern) {
            normalized = pattern.replace_all(&normalized, " ").to_string();
        }
    }
    let normalized = collapse_whitespace(&normalized);
    if normalized.is_empty() {
        return None;
    }

    let parts: Vec<&str> = normalized.split(' ').collect();
    let has_markers = lower_text.contains("chatroomtopmsgrequest")
        || lower_text.contains("chatroomtopmsgresponse");
    if parts.len() < 5 {
        return None;
    }

    let chatroom_id = parts[0].trim();
    let operation = parts[1].trim();
    if !chatroom_id.ends_with("@chatroom") || (operation != "1" && operation != "2") {
        return None;
    }
    if !has_markers
        && (parts.len() < 6
            || !is_int_token(parts[2])
            || !is_int_token(parts[3])
            || !is_int_token(parts[5]))
    {
        return None;
    }

    let operator_username = parts[4].trim();
    if operator_username.is_empty() {
        return None;
    }

    let mut operator_display_name = String::new();
    if parts.len() >= 6 && is_int_token(parts[5]) {
        let mut response_tokens = &parts[6..];
        if response_tokens.len() >= 2 && is_int_token(response_tokens[response_tokens.len() - 1]) {
            response_tokens = &response_tokens[..response_tokens.len() - 1];
        }
        operator_display_name = response_tokens.join(" ").trim().to_string();
    }

    Some(ChatroomTopMessage {
        operation: operation.to_string(),
        operator_username: operator_username.to_string(),
        operator_display_name,
    })
}

fn parse_chatroom_top_message(raw_text: &str, resolve: &dyn Fn(&str, &str) -> String) -> String {
    let Some(metadata) = extract_chatroom_top_message_metadata(raw_text) else {
        return String::new();
    };
    let action = match metadata.operation.as_str() {
        "1" => "置顶了一条消息",
        "2" => "移除了一条置顶消息",
        _ => return String::new(),
    };
    let mut display = metadata.operator_display_name.trim().to_string();
    if !metadata.operator_username.is_empty() {
        let resolved = resolve(&metadata.operator_username, &display)
            .trim()
            .to_string();
        if !resolved.is_empty() {
            display = resolved;
        }
    }
    if display.is_empty() {
        display = if metadata.operator_username.is_empty() {
            "有人".to_string()
        } else {
            metadata.operator_username
        };
    }
    format!("{display}{action}")
}

pub fn parse_system_message_content(
    raw_text: &str,
    resolve: &dyn Fn(&str, &str) -> String,
) -> String {
    let text = raw_text.trim();
    if text.is_empty() {
        return "[系统消息]".to_string();
    }

    let top_message = parse_chatroom_top_message(text, resolve);
    if !top_message.is_empty() {
        return top_message;
    }

    if text.to_lowercase().contains("sysmsgtemplate") {
        if let Some(rendered) = parse_sysmsg_template(text, resolve) {
            return rendered;
        }
    }

    if text.to_lowercase().contains("revokemsg") {
        let replace = clean_system_text(&extract_tag_text(text, "replacemsg"));
        if !replace.is_empty() {
            return replace;
        }
        let revoke = clean_system_text(&extract_tag_text(text, "revokemsg"));
        if !revoke.is_empty() {
            return revoke;
        }
        return "撤回了一条消息".to_string();
    }

    let cleaned = clean_system_text(text);
    if cleaned.is_empty() {
        "[系统消息]".to_string()
    } else {
        cleaned
    }
}

pub fn parse_quote_message(text: &str) -> String {
    let title = extract_tag_text(text, "title");
    if !title.is_empty() {
        return title;
    }
    let content = extract_tag_text(text, "content");
    if !content.is_empty() {
        return content;
    }
    "[引用消息]".to_string()
}

pub fn parse_app_message(text: &str) -> MessageBody {
    let app_type = extract_appmsg_type(text);
    let title = extract_tag_text(text, "title");
    let des = extract_tag_text(text, "des");
    let url = normalize_xml_url(&extract_tag_text(text, "url"));

    let source_display_name = first_non_empty([
        extract_tag_text(text, "sourcedisplayname").as_str(),
        extract_tag_text(text, "sourceDisplayName").as_str(),
        extract_tag_text(text, "appname").as_str(),
    ]);
    let source_username = first_non_empty([
        extract_tag_text(text, "sourceusername").as_str(),
        extract_tag_text(text, "sourceUsername").as_str(),
    ]);

    let lower = text.to_lowercase();

    if app_type == 3 {
        let thumb_url = normalize_xml_url(&first_non_empty([
            extract_tag_or_attr(text, "songalbumurl").as_str(),
            extract_tag_or_attr(text, "mvCoverUrl").as_str(),
            extract_tag_or_attr(text, "thumburl").as_str(),
            extract_tag_or_attr(text, "cdnthumburl").as_str(),
        ]));
        let music_url = if url.is_empty() {
            normalize_xml_url(&extract_tag_text(text, "dataurl"))
        } else {
            url
        };
        return MessageBody::Link {
            content: if !des.is_empty() {
                des.clone()
            } else if !title.is_empty() {
                title.clone()
            } else {
                "[音乐]".to_string()
            },
            title: if !title.is_empty() {
                title
            } else if !des.is_empty() {
                des
            } else {
                "[音乐]".to_string()
            },
            url: music_url,
            thumb_url,
            from: source_display_name,
            from_username: source_username,
            link_type: "music".to_string(),
            link_style: "default".to_string(),
            object_id: None,
            object_nonce_id: None,
        };
    }

    if app_type == 19 {
        let record_item = extract_tag_text(text, "recorditem");
        let mut preview = des.trim().to_string();
        if preview.is_empty() && !record_item.is_empty() {
            preview = extract_tag_text(&record_item, "desc").trim().to_string();
        }
        return MessageBody::ChatHistory {
            content: if preview.is_empty() {
                "[聊天记录]".to_string()
            } else {
                preview
            },
            title: {
                let title = title.trim();
                if title.is_empty() {
                    "聊天记录".to_string()
                } else {
                    title.to_string()
                }
            },
            record_item,
        };
    }

    if matches!(app_type, 4 | 5 | 68) && !url.is_empty() {
        let thumb_url = normalize_xml_url(&first_non_empty([
            extract_tag_text(text, "thumburl").as_str(),
            extract_tag_text(text, "cdnthumburl").as_str(),
        ]));
        let (link_type, link_style) =
            classify_link_share(app_type, &url, source_username.as_str(), des.as_str());
        return MessageBody::Link {
            content: first_non_empty([des.as_str(), title.as_str(), "[链接]"]),
            title: first_non_empty([title.as_str(), des.as_str()]),
            url,
            thumb_url,
            from: source_display_name,
            from_username: source_username,
            link_type,
            link_style,
            object_id: None,
            object_nonce_id: None,
        };
    }

    if app_type == 51 {
        let finder_feed = extract_tag_text(text, "finderFeed");
        let finder_desc = first_non_empty([
            extract_tag_text(&finder_feed, "desc").as_str(),
            extract_tag_text(text, "finderdesc").as_str(),
            des.as_str(),
        ]);
        let finder_nickname = first_non_empty([
            extract_tag_text(text, "findernickname").as_str(),
            extract_tag_text(text, "finder_nickname").as_str(),
            extract_tag_text(&finder_feed, "nickname").as_str(),
            extract_tag_text(&finder_feed, "findernickname").as_str(),
        ]);
        let finder_username = first_non_empty([
            extract_tag_text(text, "finderusername").as_str(),
            extract_tag_text(text, "finder_username").as_str(),
            extract_tag_text(&finder_feed, "username").as_str(),
            extract_tag_text(&finder_feed, "finderusername").as_str(),
        ]);
        let object_id = first_non_empty([
            extract_tag_or_attr(&finder_feed, "objectid").as_str(),
            extract_tag_or_attr(text, "objectid").as_str(),
        ]);
        let object_nonce_id = first_non_empty([
            extract_tag_or_attr(&finder_feed, "objectnonceid").as_str(),
            extract_tag_or_attr(text, "objectnonceid").as_str(),
        ]);
        let thumb_url = normalize_xml_url(&first_non_empty([
            extract_tag_or_attr(text, "thumburl").as_str(),
            extract_tag_or_attr(text, "cdnthumburl").as_str(),
            extract_tag_or_attr(text, "coverurl").as_str(),
            extract_tag_or_attr(text, "cover").as_str(),
            extract_tag_or_attr(&finder_feed, "thumbUrl").as_str(),
            extract_tag_or_attr(&finder_feed, "thumburl").as_str(),
            extract_tag_or_attr(&finder_feed, "coverUrl").as_str(),
            extract_tag_or_attr(&finder_feed, "coverurl").as_str(),
        ]));
        let finder_url = if url.is_empty() {
            normalize_xml_url(&first_non_empty([
                extract_tag_text(&finder_feed, "url").as_str(),
                extract_tag_text(text, "playurl").as_str(),
                extract_tag_text(text, "dataurl").as_str(),
            ]))
        } else {
            url
        };

        let mut display_title = title.trim().to_string();
        if display_title.is_empty() || display_title.contains("不支持") {
            display_title = finder_desc.trim().to_string();
        }
        if display_title.is_empty() {
            display_title = des.trim().to_string();
        }
        if display_title.is_empty() {
            display_title = "[视频号]".to_string();
        }
        let summary = if finder_desc.trim().is_empty() {
            display_title.clone()
        } else {
            finder_desc.trim().to_string()
        };
        let from = first_non_empty([
            finder_nickname.as_str(),
            source_display_name.as_str(),
            "视频号",
        ]);
        let from_username = first_non_empty([finder_username.as_str(), source_username.as_str()]);

        return MessageBody::Link {
            content: summary,
            title: display_title,
            url: finder_url,
            thumb_url,
            from,
            from_username,
            link_type: "finder".to_string(),
            link_style: "finder".to_string(),
            object_id: Some(object_id.trim().to_string()),
            object_nonce_id: Some(object_nonce_id.trim().to_string()),
        };
    }

    if matches!(app_type, 33 | 36) {
        let weapp_block = first_non_empty([
            extract_tag_text(text, "weappinfo").as_str(),
            extract_tag_text(text, "wxaappinfo").as_str(),
        ]);
        let weapp_username = extract_tag_text(&weapp_block, "username");
        let weapp_icon = normalize_xml_url(&extract_tag_or_attr(&weapp_block, "weappiconurl"));
        let weapp_icon = if weapp_icon.is_empty() {
            normalize_xml_url(&extract_tag_or_attr(text, "weappiconurl"))
        } else {
            weapp_icon
        };
        let thumb_url = normalize_xml_url(&first_non_empty([
            extract_tag_or_attr(text, "thumburl").as_str(),
            extract_tag_or_attr(text, "cdnthumburl").as_str(),
            extract_tag_or_attr(text, "coverurl").as_str(),
            extract_tag_or_attr(text, "cover").as_str(),
            weapp_icon.as_str(),
        ]));

        let mut from_display = source_display_name.trim().to_string();
        if from_display.is_empty() && !weapp_block.is_empty() {
            from_display = first_non_empty([
                extract_tag_text(&weapp_block, "nickname").as_str(),
                extract_tag_text(&weapp_block, "appname").as_str(),
            ]);
        }
        if from_display.is_empty() {
            from_display = extract_tag_text(text, "sourcename").trim().to_string();
        }
        let from_username = first_non_empty([weapp_username.as_str(), source_username.as_str()]);

        let content = first_non_empty([des.as_str(), title.as_str(), "[Mini Program]"]);
        let title_text = first_non_empty([title.as_str(), des.as_str()]);

        return MessageBody::Link {
            content: content.clone(),
            title: if title_text.is_empty() {
                content
            } else {
                title_text
            },
            url,
            thumb_url,
            from: from_display,
            from_username,
            link_type: "mini_program".to_string(),
            link_style: "default".to_string(),
            object_id: None,
            object_nonce_id: None,
        };
    }

    if matches!(app_type, 6 | 74) {
        let file_name = title;
        let total_len = extract_tag_text(text, "totallen");
        let file_md5 = first_non_empty([
            extract_tag_or_attr(text, "md5").as_str(),
            extract_tag_or_attr(text, "filemd5").as_str(),
            extract_tag_or_attr(text, "file_md5").as_str(),
        ]);
        return MessageBody::File {
            content: if file_name.is_empty() {
                "[文件]".to_string()
            } else {
                format!("[文件] {file_name}").trim().to_string()
            },
            title: file_name,
            size: total_len,
            file_md5,
        };
    }

    let refermsg_probe = if lower.contains("<recorditem") && lower.contains("<refermsg") {
        remove_block(text, "recorditem").to_lowercase()
    } else {
        lower.clone()
    };

    if app_type == 57 || refermsg_probe.contains("<refermsg") {
        let refer_block = extract_refermsg_block(text);
        let text_wo_refer = remove_block(text, "refermsg");

        let reply_text = {
            let title = extract_tag_text(&text_wo_refer, "title");
            if title.is_empty() {
                extract_tag_text(text, "title")
            } else {
                title
            }
        };
        let refer_displayname = extract_tag_or_attr(&refer_block, "displayname");
        let refer_fromusr = first_non_empty([
            extract_tag_or_attr(&refer_block, "fromusr").as_str(),
            extract_tag_or_attr(&refer_block, "fromusername").as_str(),
        ]);
        let refer_svrid = extract_tag_or_attr(&refer_block, "svrid");
        let mut refer_content = extract_refermsg_content(&refer_block);
        let refer_type = extract_tag_or_attr(&refer_block, "type");

        let reply = reply_text.trim();
        let content = refer_content.trim();
        if !reply.is_empty() && !content.is_empty() {
            if content == reply {
                refer_content = String::new();
            } else {
                let lines: Vec<&str> = content.lines().map(str::trim).collect();
                if lines.first().map(|line| *line == reply).unwrap_or(false) {
                    let mut parts = content.lines();
                    parts.next();
                    refer_content = parts
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim_start()
                        .to_string();
                } else if let Some(rest) = content.strip_prefix(reply) {
                    refer_content = rest.trim_start().to_string();
                }
            }
        }

        let quote_type = refer_type.trim().to_string();
        let mut quote_thumb_url = String::new();
        let mut quote_voice_length = String::new();
        match quote_type.as_str() {
            "3" => refer_content = "[图片]".to_string(),
            "47" => refer_content = "[表情]".to_string(),
            "43" | "62" => refer_content = "[视频]".to_string(),
            "34" => {
                let content = refer_content.trim();
                let parts: Vec<&str> = content.split(':').collect();
                if parts.len() >= 2 {
                    let duration = parts[1].trim();
                    if !duration.is_empty() && duration.chars().all(|ch| ch.is_ascii_digit()) {
                        quote_voice_length = duration
                            .parse::<i64>()
                            .map(|value| value.to_string())
                            .unwrap_or_default();
                    }
                }
                refer_content = "[语音]".to_string();
            }
            "57" => {
                let summarized = summarize_nested_quote_content(refer_content.trim());
                if !summarized.is_empty() {
                    refer_content = summarized;
                } else if refer_content.trim_start().starts_with('<') {
                    refer_content = "[引用消息]".to_string();
                }
            }
            "49" | "5" | "68" => {
                let raw_link_content = refer_content.trim().to_string();
                let summarized = summarize_nested_quote_content(&raw_link_content);
                let mut link_text = if summarized.is_empty() {
                    raw_link_content.clone()
                } else {
                    summarized
                };
                quote_thumb_url = extract_nested_quote_thumb_url(&raw_link_content);

                if link_text.starts_with("wxid_") {
                    if let Some(colon) = link_text.find(':') {
                        if colon > 0 && colon <= 64 {
                            let maybe_rest = link_text[colon + 1..].trim().to_string();
                            if !maybe_rest.is_empty() {
                                let second_try = summarize_nested_quote_content(&maybe_rest);
                                link_text = if second_try.is_empty() {
                                    maybe_rest.clone()
                                } else {
                                    second_try
                                };
                            }
                            if quote_thumb_url.is_empty() {
                                quote_thumb_url = extract_nested_quote_thumb_url(&maybe_rest);
                            }
                        }
                    }
                }

                refer_content = if link_text.is_empty() {
                    "[链接]".to_string()
                } else {
                    format!("[链接] {}", link_text).trim().to_string()
                };
            }
            _ => {}
        }

        return MessageBody::Quote {
            content: if reply.is_empty() {
                "[引用消息]".to_string()
            } else {
                reply.to_string()
            },
            quote_username: refer_fromusr.trim().to_string(),
            quote_title: refer_displayname,
            quote_content: refer_content.trim().to_string(),
            quote_type,
            quote_thumb_url,
            quote_server_id: refer_svrid.trim().to_string(),
            quote_voice_length,
        };
    }

    let patmsg_attr = cached_regex(r#"(?is)<(sysmsg|appmsg)\b[^>]*\btype=['"]patmsg['"]"#)
        .map(|pattern| pattern.is_match(text))
        .unwrap_or(false);
    if app_type == 62 || patmsg_attr {
        return MessageBody::System {
            content: "[拍一拍]".to_string(),
        };
    }

    if app_type == 2000
        || (text.contains("<wcpayinfo")
            && (lower.contains("transfer") || lower.contains("paysubtype")))
    {
        let feedesc = extract_tag_or_attr(text, "feedesc");
        let pay_memo = extract_tag_or_attr(text, "pay_memo");
        let paysubtype = extract_tag_or_attr(text, "paysubtype");
        let receivestatus = extract_tag_or_attr(text, "receivestatus");
        let sendertitle = extract_tag_or_attr(text, "sendertitle");
        let receivertitle = extract_tag_or_attr(text, "receivertitle");
        let senderdes = extract_tag_or_attr(text, "senderdes");
        let receiverdes = extract_tag_or_attr(text, "receiverdes");
        let transferid = extract_tag_or_attr(text, "transferid");
        let invalidtime = extract_tag_or_attr(text, "invalidtime");

        return MessageBody::Transfer {
            content: pay_memo.trim().to_string(),
            title: first_non_empty([feedesc.as_str(), title.as_str()]),
            amount: feedesc,
            pay_sub_type: paysubtype.trim().to_string(),
            receive_status: receivestatus.trim().to_string(),
            sender_title: sendertitle,
            receiver_title: receivertitle,
            sender_des: senderdes,
            receiver_des: receiverdes,
            transfer_id: transferid.trim().to_string(),
            invalid_time: invalidtime.trim().to_string(),
        };
    }

    if matches!(app_type, 2001 | 2003)
        || (text.contains("<wcpayinfo")
            && (lower.contains("redenvelope") || lower.contains("sendertitle")))
    {
        let sendertitle = extract_tag_text(text, "sendertitle");
        let receivertitle = extract_tag_text(text, "receivertitle");
        let senderdes = extract_tag_text(text, "senderdes");
        let receiverdes = extract_tag_text(text, "receiverdes");
        let cover = extract_tag_text(text, "receiverc2cshowsourceurl");
        let content = first_non_empty([
            sendertitle.as_str(),
            receivertitle.as_str(),
            title.as_str(),
            "红包",
        ]);
        return MessageBody::RedPacket {
            content: if content.trim().is_empty() {
                "红包".to_string()
            } else {
                content.trim().to_string()
            },
            title: first_non_empty([senderdes.as_str(), receiverdes.as_str(), des.as_str()]),
            cover_url: cover,
        };
    }

    if app_type == 87 {
        let announcement = extract_tag_text(text, "announcement");
        let mut parts: Vec<String> = Vec::new();
        let plain = extract_tag_text(&announcement, "text");
        if !plain.trim().is_empty() {
            parts.push(collapse_whitespace(&plain));
        }
        if let Some(pattern) = cached_regex(r"(?is)<dataitem\b[^>]*>(.*?)</dataitem>") {
            for captures in pattern.captures_iter(&announcement) {
                let item = captures
                    .get(1)
                    .map(|value| value.as_str())
                    .unwrap_or_default();
                let name = first_non_empty([
                    extract_tag_text(item, "datatitle").as_str(),
                    extract_tag_text(item, "datadesc").as_str(),
                ]);
                if !name.is_empty() && !parts.contains(&name) {
                    parts.push(name);
                }
            }
        }
        let content = if parts.is_empty() {
            "群公告".to_string()
        } else {
            parts.join(" · ")
        };
        return MessageBody::Link {
            content,
            title: "群公告".to_string(),
            url: String::new(),
            thumb_url: String::new(),
            from: "群公告".to_string(),
            from_username: String::new(),
            link_type: "announcement".to_string(),
            link_style: "default".to_string(),
            object_id: None,
            object_nonce_id: None,
        };
    }

    if !title.is_empty() || !des.is_empty() {
        return MessageBody::Text {
            content: first_non_empty([title.as_str(), des.as_str()]),
        };
    }

    MessageBody::Text {
        content: "[应用消息]".to_string(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EmojiReference {
    pub md5: String,
    pub url: String,
    pub encrypt_url: String,
    pub thumb_url: String,
    pub aes_key: String,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
}

impl EmojiReference {
    pub fn is_empty(&self) -> bool {
        self.md5.is_empty() && self.url.is_empty() && self.encrypt_url.is_empty()
    }

    pub fn best_url(&self) -> (&str, &str) {
        if !self.url.is_empty() {
            (&self.url, "")
        } else if !self.encrypt_url.is_empty() {
            (&self.encrypt_url, self.aes_key.as_str())
        } else {
            (&self.thumb_url, "")
        }
    }
}

pub fn parse_emoji_reference(content: &str) -> Option<EmojiReference> {
    let text = content.trim();
    if text.is_empty() {
        return None;
    }

    let mut reference = EmojiReference::default();
    let md5_attr = extract_tag_or_attr(text, "md5").to_ascii_lowercase();
    let md5_emoticon = extract_tag_or_attr(text, "emoticonmd5").to_ascii_lowercase();
    let md5 = first_non_empty([md5_attr.as_str(), md5_emoticon.as_str()]);
    if md5.len() == 32 && md5.chars().all(|ch| ch.is_ascii_hexdigit()) {
        reference.md5 = md5;
    }

    reference.url = first_non_empty([
        extract_tag_or_attr(text, "cdnurl").as_str(),
        extract_tag_or_attr(text, "cdn_url").as_str(),
    ]);
    reference.encrypt_url = first_non_empty([
        extract_tag_or_attr(text, "encrypturl").as_str(),
        extract_tag_or_attr(text, "encrypt_url").as_str(),
    ]);
    reference.thumb_url = first_non_empty([
        extract_tag_or_attr(text, "thumburl").as_str(),
        extract_tag_or_attr(text, "thumb_url").as_str(),
        extract_tag_or_attr(text, "externurl").as_str(),
        extract_tag_or_attr(text, "extern_url").as_str(),
    ]);
    reference.aes_key = first_non_empty([
        extract_tag_or_attr(text, "aeskey").as_str(),
        extract_tag_or_attr(text, "aes_key").as_str(),
    ])
    .to_ascii_lowercase();

    let width = first_non_empty([
        extract_tag_or_attr(text, "width").as_str(),
        extract_tag_or_attr(text, "cdnthumbwidth").as_str(),
    ]);
    let height = first_non_empty([
        extract_tag_or_attr(text, "height").as_str(),
        extract_tag_or_attr(text, "cdnthumbheight").as_str(),
    ]);
    reference.width = width.parse().unwrap_or(0);
    reference.height = height.parse().unwrap_or(0);
    reference.animated = extract_tag_or_attr(text, "type").trim() == "2";

    if reference.url.is_empty() || reference.md5.is_empty() {
        if let Some(info) = extract_emoji_info(text) {
            if reference.md5.is_empty() {
                reference.md5 = info.md5;
            }
            if reference.url.is_empty() {
                reference.url = info.url;
            }
            if reference.encrypt_url.is_empty() {
                reference.encrypt_url = info.encrypt_url;
            }
        }
    }

    if reference.is_empty() {
        None
    } else {
        Some(reference)
    }
}

fn extract_emoji_info(text: &str) -> Option<EmojiReference> {
    use base64::Engine as _;

    let raw = extract_tag_text(text, "emojiinfo");
    if raw.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .ok()?;
    let text = String::from_utf8_lossy(&decoded);
    let mut reference = EmojiReference::default();

    let md5_pattern = cached_regex(r"[0-9a-fA-F]{32}")?;
    if let Some(value) = md5_pattern.find(&text) {
        reference.md5 = value.as_str().to_ascii_lowercase();
    }
    let url_pattern = cached_regex(r#"https?://[!-~]+"#)?;
    let mut urls: Vec<String> = url_pattern
        .find_iter(&text)
        .map(|value| html_escape::decode_html_entities(value.as_str()).to_string())
        .collect();
    urls.reverse();
    reference.url = urls.pop().unwrap_or_default();
    reference.encrypt_url = urls.pop().unwrap_or_default();
    reference.thumb_url = urls.pop().unwrap_or_default();
    Some(reference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_link_card() {
        let xml = r#"<msg><appmsg appid="" sdkver="0"><title>标题</title><des>描述</des>
            <type>5</type><url>https://mp.weixin.qq.com/s/abc</url>
            <sourceusername>gh_source</sourceusername>
            <sourcedisplayname>来源号</sourcedisplayname></appmsg></msg>"#;
        let body = parse_app_message(xml);
        let MessageBody::Link {
            content,
            title,
            link_type,
            link_style,
            ..
        } = body
        else {
            panic!("expected link");
        };
        assert_eq!(content, "描述");
        assert_eq!(title, "标题");
        assert_eq!(link_type, "official_article");
        assert_eq!(link_style, "default");
    }

    #[test]
    fn parses_quote_with_refermsg() {
        let xml = r#"<msg><appmsg><title>回复内容</title><type>57</type>
            <refermsg><type>1</type><displayname>甲</displayname><fromusr>wxid_a</fromusr>
            <svrid>42</svrid><content><![CDATA[原始消息]]></content></refermsg></appmsg></msg>"#;
        let body = parse_app_message(xml);
        let MessageBody::Quote {
            content,
            quote_title,
            quote_content,
            quote_username,
            quote_server_id,
            ..
        } = body
        else {
            panic!("expected quote");
        };
        assert_eq!(content, "回复内容");
        assert_eq!(quote_title, "甲");
        assert_eq!(quote_content, "原始消息");
        assert_eq!(quote_username, "wxid_a");
        assert_eq!(quote_server_id, "42");
    }

    #[test]
    fn parses_location() {
        let body = parse_location_message(
            r#"<msg><location x="31.2" y="121.5" poiName="外滩" label="上海市黄浦区"/></msg>"#,
        );
        let MessageBody::Location {
            content,
            location_lat,
            location_lng,
            location_poiname,
            location_label,
        } = body
        else {
            panic!("expected location");
        };
        assert_eq!(content, "外滩");
        assert_eq!(location_lat, Some(31.2));
        assert_eq!(location_lng, Some(121.5));
        assert_eq!(location_poiname, "外滩");
        assert_eq!(location_label, "上海市黄浦区");
    }

    #[test]
    fn parses_voip_messages() {
        let legacy = parse_voip_message(
            r#"<VoIPBubbleMsg><room_type>0</room_type><msg>通话时长 00:12</msg></VoIPBubbleMsg>"#,
        );
        let MessageBody::Voip { content, voip_type } = legacy else {
            panic!("expected voip");
        };
        assert_eq!(content, "通话时长 00:12");
        assert_eq!(voip_type, "video");

        let v4 = parse_voip_message(
            r#"<voipinvitemsg><invite_type>1</invite_type></voipinvitemsg><voiplocalinfo><diaplay_content>对方已取消</diaplay_content></voiplocalinfo>"#,
        );
        let MessageBody::Voip { content, voip_type } = v4 else {
            panic!("expected voip");
        };
        assert_eq!(content, "对方已取消");
        assert_eq!(voip_type, "");
    }

    #[test]
    fn parses_group_announcement_attachment_names() {
        let xml = r#"<msg><appmsg><type>87</type><announcement><![CDATA[
            <group_notice_item><datalist count="2">
            <dataitem datatype="8"><datatitle>课程说明.pdf</datatitle><datadesc>186.83KB</datadesc></dataitem>
            <dataitem datatype="1"><datadesc>周五交作业</datadesc></dataitem>
            </datalist></group_notice_item>]]></announcement></appmsg></msg>"#;
        let body = parse_app_message(xml);
        let MessageBody::Link {
            content,
            title,
            link_type,
            ..
        } = body
        else {
            panic!("expected announcement link card");
        };
        assert_eq!(title, "群公告");
        assert_eq!(link_type, "announcement");
        assert!(content.contains("课程说明.pdf"));
        assert!(content.contains("周五交作业"));
    }

    #[test]
    fn emoji_local_type_does_not_fall_back_to_app_message() {
        let xml = r#"<msg><appmsg><type>8</type><appattach><fileext>pic</fileext></appattach></appmsg></msg>"#;
        assert_eq!(parse_message_body(34_359_738_417, xml), None);
    }

    #[test]
    fn parses_emoji_message_reference() {
        let xml = r#"<msg><emoji fromusername="wxid_a" type="1" md5="666dd69e86c256a4805104c1ff957dea" cdnurl="http://vweixinf.tc.qq.com/a?sig=1" encrypturl="http://vweixinf.tc.qq.com/b?sig=2" aeskey="b74169515dbb4f8ab11498c696ec4db8" width="249" height="300" /></msg>"#;
        let reference = parse_emoji_reference(xml).expect("emoji reference");
        assert_eq!(reference.md5, "666dd69e86c256a4805104c1ff957dea");
        assert_eq!(reference.width, 249);
        assert_eq!(reference.height, 300);
        assert_eq!(reference.aes_key, "b74169515dbb4f8ab11498c696ec4db8");
        assert_eq!(reference.best_url().0, "http://vweixinf.tc.qq.com/a?sig=1");
    }

    #[test]
    fn parses_appmsg_emoji_info() {
        use base64::Engine as _;

        let mut info = Vec::new();
        info.extend_from_slice(b"\n 1ca878a4ca7ba63a0964c53308fe0112");
        info.extend_from_slice(b"\x12\x0chttp://vweixinf.tc.qq.com/one");
        info.extend_from_slice(b"\x12\x0chttp://vweixinf.tc.qq.com/two");
        let encoded = base64::engine::general_purpose::STANDARD.encode(&info);
        let xml = format!(
            r#"<msg><appmsg><type>8</type><appattach><emoticonmd5>1ca878a4ca7ba63a0964c53308fe0112</emoticonmd5><emojiinfo>{encoded}</emojiinfo></appattach></appmsg></msg>"#
        );
        let reference = parse_emoji_reference(&xml).expect("emoji reference");
        assert_eq!(reference.md5, "1ca878a4ca7ba63a0964c53308fe0112");
        assert_eq!(reference.url, "http://vweixinf.tc.qq.com/one");
        assert_eq!(reference.encrypt_url, "http://vweixinf.tc.qq.com/two");
    }

    #[test]
    fn renders_pat_template() {
        let rendered = parse_pat_message(
            r#"<msg><patMsg><template>${wxid_a}拍了拍${wxid_b}</template></patMsg></msg>"#,
            &|wxid| {
                if wxid == "wxid_b" {
                    "乙".to_string()
                } else {
                    wxid.to_string()
                }
            },
        );
        assert_eq!(rendered, "wxid_a拍了拍乙");
    }

    #[test]
    fn parses_revoke_system_message() {
        let text = r#"<sysmsg type="revokemsg"><revokemsg><replacemsg><![CDATA["甲" 撤回了一条消息]]></replacemsg></revokemsg></sysmsg>"#;
        assert_eq!(
            parse_system_message_content(text, &|username, fallback| {
                if fallback.is_empty() {
                    username.to_string()
                } else {
                    fallback.to_string()
                }
            }),
            "\"甲\" 撤回了一条消息"
        );
    }
}
