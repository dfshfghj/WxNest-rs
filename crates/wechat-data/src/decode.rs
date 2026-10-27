use std::io::Cursor;
use std::sync::OnceLock;

use base64::Engine;
use regex::Regex;

use crate::sqlite::{decode_utf8_ignore, SqlValue};

const ZSTD_MAGIC: &[u8] = b"\x28\xb5\x2f\xfd";

pub fn decode_sqlite_text(value: &SqlValue) -> String {
    value.text_lossy()
}

pub fn is_mostly_printable_text(value: &str) -> bool {
    let sample: String = value.chars().take(600).collect();
    if sample.is_empty() {
        return false;
    }
    let printable = sample
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
        .count();
    printable as f64 / sample.chars().count() as f64 >= 0.85
}

pub fn looks_like_xml(value: &str) -> bool {
    let mut text = value.trim();
    if text.starts_with('"') && text.ends_with('"') {
        text = text.trim_matches('"').trim_start();
    }
    text.starts_with('<')
}

pub fn decode_message_content(compress: Option<&SqlValue>, message: &SqlValue) -> String {
    let mut msg_text = decode_sqlite_text(message);

    let s = html_unescape(msg_text.trim());
    if let Some(decoded) = try_decode_text_blob(&s) {
        msg_text = decoded;
    }

    if let Some(raw) = message.bytes() {
        if raw.starts_with(ZSTD_MAGIC) {
            if let Some(out) = zstd_decompress(&raw) {
                let s = html_unescape(out.trim());
                if looks_like_xml(&s) || is_mostly_printable_text(&s) {
                    msg_text = s;
                }
            }
        }
    }

    let Some(compress) = compress else {
        return msg_text;
    };

    if let SqlValue::Text(value) = compress {
        let s = html_unescape(value.trim());
        if let Some(decoded) = try_decode_text_blob(&s) {
            return decoded;
        }
        if looks_like_xml(&s) || is_mostly_printable_text(&s) {
            return s;
        }
        return msg_text;
    }

    let Some(data) = compress.bytes() else {
        return msg_text;
    };
    if data.is_empty() {
        return msg_text;
    }

    if let Some(out) = zstd_decompress(&data) {
        let s = html_unescape(out.trim());
        if looks_like_xml(&s) || is_mostly_printable_text(&s) {
            return s;
        }
    }

    let s = html_unescape(decode_utf8_ignore(&data).trim());
    if let Some(decoded) = try_decode_text_blob(&s) {
        return decoded;
    }
    if looks_like_xml(&s) || is_mostly_printable_text(&s) {
        return s;
    }
    msg_text
}

fn try_decode_text_blob(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    if text.len() >= 16 && text.len() % 2 == 0 && hex_pattern().is_match(text) {
        if let Ok(raw) = hex::decode(text) {
            if raw.starts_with(ZSTD_MAGIC) {
                if let Some(out) = zstd_decompress(&raw) {
                    let decoded = html_unescape(out.trim());
                    if looks_like_xml(&decoded) || is_mostly_printable_text(&decoded) {
                        return Some(decoded);
                    }
                }
            }
            let decoded = html_unescape(decode_utf8_ignore(&raw).trim());
            let lower = decoded.to_lowercase();
            if looks_like_xml(&decoded)
                || (lower.contains("<msg") && lower.contains("</msg>"))
                || lower.contains("<appmsg")
            {
                return Some(decoded);
            }
        }
    }

    if text.len() >= 24 && text.len() % 4 == 0 && base64_pattern().is_match(text) {
        if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(text) {
            if raw.starts_with(ZSTD_MAGIC) {
                if let Some(out) = zstd_decompress(&raw) {
                    let decoded = html_unescape(out.trim());
                    if looks_like_xml(&decoded) || is_mostly_printable_text(&decoded) {
                        return Some(decoded);
                    }
                }
            }
            let decoded = html_unescape(decode_utf8_ignore(&raw).trim());
            let lower = decoded.to_lowercase();
            if looks_like_xml(&decoded)
                || (lower.contains("<msg") && lower.contains("</msg>"))
                || lower.contains("<appmsg")
            {
                return Some(decoded);
            }
        }
    }
    None
}

fn zstd_decompress(data: &[u8]) -> Option<String> {
    let output = zstd::stream::decode_all(Cursor::new(data)).ok()?;
    Some(decode_utf8_ignore(&output))
}

fn html_unescape(value: &str) -> String {
    html_escape::decode_html_entities(value).to_string()
}

fn hex_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^[0-9a-fA-F]+$").expect("valid hex regex"))
}

fn base64_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^[A-Za-z0-9+/=]+$").expect("valid base64 regex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_text_when_nothing_proves_a_blob_payload() {
        assert_eq!(
            decode_message_content(None, &SqlValue::Text("hello".into())),
            "hello"
        );
        assert_eq!(
            decode_message_content(None, &SqlValue::Text("a &amp; b".into())),
            "a &amp; b"
        );
    }

    #[test]
    fn decodes_zstd_hex_and_base64() {
        let raw = b"<msg><appmsg>hello</appmsg></msg>";
        let compressed = zstd::stream::encode_all(Cursor::new(raw), 0).unwrap();
        let hex_value = SqlValue::Text(hex::encode(&compressed));
        assert_eq!(
            decode_message_content(None, &hex_value),
            String::from_utf8_lossy(raw)
        );

        let base64_value =
            SqlValue::Text(base64::engine::general_purpose::STANDARD.encode(&compressed));
        assert_eq!(
            decode_message_content(None, &base64_value),
            String::from_utf8_lossy(raw)
        );
    }
}
