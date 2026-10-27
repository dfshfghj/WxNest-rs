use std::io::Read;
use std::time::Duration;

use url::Url;
use wechat_core::crypto::wx_isaac64;
use wechat_core::media::detect_image_media_type;

use crate::error::{DataError, Result};

const ALLOWED_SUFFIXES: &[&str] = &[".qpic.cn", ".qlogo.cn", ".tc.qq.com", ".video.qq.com"];
const TLS_HOST_ALIASES: &[(&str, &str)] = &[
    ("vweixinthumb.tc.qq.com", "socwxsns.video.qq.com"),
    ("vweixinf.tc.qq.com", "socwxsns.video.qq.com"),
];
const VIDEO_DECRYPT_LIMIT: usize = 128 * 1024;

#[derive(Debug, Clone)]
pub struct RemoteMedia {
    pub data: Vec<u8>,
    pub media_type: String,
    pub source: &'static str,
}

pub fn is_allowed_tencent_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    ALLOWED_SUFFIXES.iter().any(|suffix| host.ends_with(suffix))
}

pub fn normalize_tencent_url(
    raw: &str,
    token: Option<&str>,
    is_video: bool,
    force_original: bool,
) -> Option<String> {
    let raw = html_escape::decode_html_entities(raw).trim().to_string();
    let mut url = Url::parse(&raw).ok()?;
    if url.scheme() != "https" {
        url.set_scheme("https").ok()?;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !is_allowed_tencent_host(&host) {
        return None;
    }
    if let Some((_, alias)) = TLS_HOST_ALIASES.iter().find(|(from, _)| *from == host) {
        url.set_host(Some(alias)).ok()?;
    }
    if force_original && !is_video {
        let path = url.path().to_string();
        let replaced = path
            .strip_suffix("/60")
            .or_else(|| path.strip_suffix("/150"))
            .or_else(|| path.strip_suffix("/200"))
            .or_else(|| path.strip_suffix("/480"))
            .map(|prefix| format!("{prefix}/0"))
            .unwrap_or(path);
        url.set_path(&replaced);
    }
    if let Some(token) = token.map(str::trim).filter(|token| !token.is_empty()) {
        let retained: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| {
                !key.eq_ignore_ascii_case("token") && !key.eq_ignore_ascii_case("idx")
            })
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        {
            let mut query = url.query_pairs_mut();
            if is_video {
                query.append_pair("token", token);
                query.append_pair("idx", "1");
            }
            for (key, value) in retained {
                query.append_pair(&key, &value);
            }
            if !is_video {
                query.append_pair("token", token);
                query.append_pair("idx", "1");
            }
        }
    }
    Some(url.to_string())
}

pub fn fetch_tencent_media(
    raw_url: &str,
    token: Option<&str>,
    decrypt_key: Option<&str>,
    is_video: bool,
    max_bytes: usize,
) -> Result<RemoteMedia> {
    let url = normalize_tencent_url(raw_url, token, is_video, false)
        .ok_or_else(|| DataError::NotFound(format!("不允许的腾讯 CDN 地址: {raw_url}")))?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build();
    let response = agent
        .get(&url)
        .set("User-Agent", "Mozilla/5.0")
        .call()
        .map_err(|error| DataError::Decode(format!("腾讯 CDN 请求失败: {error}")))?;
    let mut data = Vec::new();
    response
        .into_reader()
        .take(max_bytes as u64)
        .read_to_end(&mut data)?;
    if data.is_empty() {
        return Err(DataError::NotFound("腾讯 CDN 返回空内容".into()));
    }

    let media_type = detect_image_media_type(&data);
    if media_type != "application/octet-stream" {
        return Ok(RemoteMedia {
            data,
            media_type: media_type.to_string(),
            source: "tencent-cdn",
        });
    }

    if let Some(key) = decrypt_key.map(str::trim).filter(|key| !key.is_empty()) {
        let limit = is_video.then_some(VIDEO_DECRYPT_LIMIT);
        wx_isaac64::xor_in_place(&mut data, key, limit);
        let media_type = detect_image_media_type(&data);
        if media_type != "application/octet-stream" {
            return Ok(RemoteMedia {
                data,
                media_type: media_type.to_string(),
                source: "tencent-cdn-decrypted",
            });
        }
    }
    Err(DataError::Decode(
        "腾讯 CDN 返回的内容不是可识别的图片（可能已过期或需要其它密钥）".into(),
    ))
}

pub fn fetch_emoticon_media(
    raw_url: &str,
    aes_key: &str,
    image_keys: Option<&crate::key_store::ImageKeys>,
    max_bytes: usize,
) -> Result<RemoteMedia> {
    let raw_url = html_escape::decode_html_entities(raw_url)
        .trim()
        .to_string();
    let parsed = Url::parse(&raw_url)
        .map_err(|error| DataError::NotFound(format!("表情地址无效: {error}")))?;
    let host = parsed
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| DataError::NotFound("表情地址缺少主机名".into()))?;
    if !is_allowed_tencent_host(&host) {
        return Err(DataError::NotFound(format!(
            "不允许的腾讯 CDN 地址: {raw_url}"
        )));
    }

    let mut candidates = Vec::new();
    if parsed.scheme() == "http" {
        let mut secure = parsed.clone();
        secure.set_scheme("https").ok();
        candidates.push(secure.to_string());
    }
    candidates.push(parsed.to_string());

    let mut last_error = None;
    for url in candidates {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .build();
        let response = match agent.get(&url).set("User-Agent", "Mozilla/5.0").call() {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(format!("{error}"));
                continue;
            }
        };
        let mut data = Vec::new();
        response
            .into_reader()
            .take(max_bytes as u64)
            .read_to_end(&mut data)?;
        if data.is_empty() {
            last_error = Some("返回空内容".to_string());
            continue;
        }
        if let Some(media) = decode_emoticon_payload(data, aes_key, image_keys) {
            return Ok(media);
        }
        last_error = Some("不是可识别的表情图片".to_string());
    }

    Err(DataError::Decode(format!(
        "拉取表情失败: {}",
        last_error.unwrap_or_else(|| "未知错误".to_string())
    )))
}

fn decode_emoticon_payload(
    data: Vec<u8>,
    aes_key: &str,
    image_keys: Option<&crate::key_store::ImageKeys>,
) -> Option<RemoteMedia> {
    let media_type = detect_image_media_type(&data);
    if media_type != "application/octet-stream" {
        return Some(RemoteMedia {
            data,
            media_type: media_type.to_string(),
            source: "tencent-emoticon",
        });
    }

    if let Some(key) = parse_emoticon_aes_key(aes_key) {
        if let Ok(decrypted) = wechat_core::crypto::aes_cbc_decrypt_128(&key, &key, &data) {
            if let Ok(unpadded) = wechat_core::crypto::pkcs7_unpad(&decrypted, 16) {
                let media_type = detect_image_media_type(&unpadded);
                if media_type != "application/octet-stream" {
                    return Some(RemoteMedia {
                        data: unpadded,
                        media_type: media_type.to_string(),
                        source: "tencent-emoticon-aes",
                    });
                }
            }
        }
    }

    if let Some(keys) = image_keys {
        let result = wechat_core::media::decrypt_media_bytes(
            &data,
            &wechat_core::media::MediaKeys {
                xor_key: keys.xor_key,
                aes_key: Some(keys.aes_key.as_bytes().to_vec()),
            },
            None,
            None,
        );
        if result.media_type.starts_with("image/") && !result.data.is_empty() {
            return Some(RemoteMedia {
                data: result.data,
                media_type: result.media_type,
                source: "tencent-emoticon-dat",
            });
        }
    }

    None
}

fn parse_emoticon_aes_key(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    if value.len() == 32 && value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return hex::decode(value).ok().filter(|key| key.len() == 16);
    }
    if value.len() == 16 {
        return Some(value.as_bytes().to_vec());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_namespaced_tencent_urls() {
        let url = normalize_tencent_url(
            "http://shmmsns.qpic.cn/mmsns/abc/150?old=1&token=old&idx=1",
            Some("new"),
            false,
            false,
        )
        .expect("url");
        assert!(url.starts_with("https://shmmsns.qpic.cn/mmsns/abc/150?"));
        assert!(url.contains("token=new"));
        assert!(!url.contains("token=old"));
        assert!(url.contains("old=1"));
    }

    #[test]
    fn rejects_non_tencent_hosts() {
        assert!(normalize_tencent_url("https://example.com/a.jpg", None, false, false).is_none());
        assert!(normalize_tencent_url("https://wx.qlogo.cn/a", None, false, false).is_some());
    }

    #[test]
    fn decrypts_vendor_wasm_fixture_bytes() {
        let mut data = vec![0u8; 16];
        wx_isaac64::xor_in_place(&mut data, "2026090201020304", None);
        assert_eq!(hex::encode(data), "cc8ab6592528fbaa2238f7fad7c416ee");
    }
}
