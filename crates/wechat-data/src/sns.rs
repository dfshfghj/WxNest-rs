use std::iter::Peekable;
use std::str::Chars;

use roxmltree::{Document, Node};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SnsMedia {
    pub id: String,
    pub kind: String,
    pub url: String,
    pub thumb_url: String,
    pub md5: String,
    pub token: String,
    pub key: String,
    pub enc_idx: String,
    pub thumb_token: String,
    pub thumb_key: String,
    pub thumb_enc_idx: String,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnsLike {
    pub username: String,
    pub nickname: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnsComment {
    pub id: String,
    pub username: String,
    pub nickname: String,
    pub content: String,
    pub ref_comment_id: String,
    pub ref_username: String,
    pub ref_nickname: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnsPost {
    pub tid: String,
    #[serde(skip)]
    pub tid_i64: i64,
    pub create_time: i64,
    pub username: String,
    pub display_name: String,
    pub text: String,
    pub title: String,
    pub link_url: String,
    pub location: String,
    pub source: String,
    pub media: Vec<SnsMedia>,
    pub likes: Vec<SnsLike>,
    pub comments: Vec<SnsComment>,
}

pub fn parse_sns_post(tid: i64, content: &str) -> Option<SnsPost> {
    let content = decode_sns_content(content);
    let sanitized = sanitize_ampersands(&content);
    let document = Document::parse(&sanitized).ok()?;
    let timeline = document
        .descendants()
        .find(|node| node.has_tag_name("TimelineObject"))?;
    let username = child_text(timeline, "username");
    let create_time = child_text(timeline, "createTime")
        .parse::<i64>()
        .unwrap_or(0);
    let text = child_text(timeline, "contentDesc");
    let local = document
        .descendants()
        .find(|node| node.has_tag_name("LocalExtraInfo"));
    let nickname = local
        .map(|node| child_text(node, "nickname"))
        .unwrap_or_default();
    let source = first_non_empty([
        timeline
            .children()
            .find(|node| node.has_tag_name("sourceNickName"))
            .map(|node| node_text(node))
            .unwrap_or_default(),
        document
            .descendants()
            .find(|node| node.has_tag_name("appName"))
            .map(|node| node.text().unwrap_or_default().trim().to_string())
            .unwrap_or_default(),
    ]);

    let content_object = timeline
        .children()
        .find(|node| node.has_tag_name("ContentObject"));
    let title = content_object
        .map(|node| child_text(node, "title"))
        .unwrap_or_default();
    let link_url = content_object
        .map(|node| first_non_empty([child_text(node, "contentUrl"), child_text(node, "url")]))
        .unwrap_or_default();
    let media = content_object.map(parse_media).unwrap_or_default();
    let location = timeline
        .children()
        .find(|node| node.has_tag_name("location"))
        .map(parse_location)
        .unwrap_or_default();

    let mut likes = Vec::new();
    let mut comments = Vec::new();
    if let Some(local) = local {
        if let Some(list) = local
            .children()
            .find(|node| node.has_tag_name("like_user_list"))
        {
            for entry in list.children().filter(|node| node.is_element()) {
                let username = child_text(entry, "username");
                if username.is_empty() {
                    continue;
                }
                likes.push(SnsLike {
                    nickname: child_text(entry, "nickname"),
                    username,
                });
            }
        }
        if let Some(list) = local
            .children()
            .find(|node| node.has_tag_name("comment_user_list"))
        {
            for entry in list.children().filter(|node| node.is_element()) {
                let content = child_text(entry, "content");
                let username = child_text(entry, "username");
                if content.is_empty() && username.is_empty() {
                    continue;
                }
                comments.push(SnsComment {
                    id: child_text(entry, "comment_64id"),
                    username,
                    nickname: child_text(entry, "nickname"),
                    content,
                    ref_comment_id: first_non_empty([
                        child_text(entry, "ref_comment_64id"),
                        child_text(entry, "ref_comment_id"),
                    ]),
                    ref_username: String::new(),
                    ref_nickname: String::new(),
                });
            }
        }
    }
    let names: std::collections::HashMap<String, String> = comments
        .iter()
        .filter(|comment| !comment.id.is_empty())
        .map(|comment| (comment.id.clone(), comment.nickname.clone()))
        .collect();
    for comment in &mut comments {
        if let Some(name) = names.get(&comment.ref_comment_id) {
            comment.ref_nickname = name.clone();
        }
    }

    Some(SnsPost {
        tid: (tid as u64).to_string(),
        tid_i64: tid,
        create_time,
        username,
        display_name: nickname,
        text,
        title,
        link_url,
        location,
        source,
        media,
        likes,
        comments,
    })
}

fn parse_media(content_object: Node) -> Vec<SnsMedia> {
    let Some(list) = content_object
        .children()
        .find(|node| node.has_tag_name("mediaList"))
    else {
        return Vec::new();
    };
    let mut output = Vec::new();
    for media in list.children().filter(|node| node.has_tag_name("media")) {
        let media_type = child_text(media, "type").parse::<i64>().unwrap_or(2);
        let url_node = media.children().find(|node| node.has_tag_name("url"));
        let thumb_node = media.children().find(|node| node.has_tag_name("thumb"));
        let url = url_node.map(|node| node_text(node)).unwrap_or_default();
        let thumb_url = thumb_node.map(|node| node_text(node)).unwrap_or_default();
        let md5 = url_node
            .and_then(|node| node.attribute("md5"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        let token = url_node
            .and_then(|node| node.attribute("token"))
            .unwrap_or_default()
            .to_string();
        let key = url_node
            .and_then(|node| node.attribute("key"))
            .unwrap_or_default()
            .to_string();
        let enc_idx = url_node
            .and_then(|node| node.attribute("enc_idx"))
            .unwrap_or_default()
            .to_string();
        let thumb_token = thumb_node
            .and_then(|node| node.attribute("token"))
            .unwrap_or_default()
            .to_string();
        let thumb_key = thumb_node
            .and_then(|node| node.attribute("key"))
            .unwrap_or_default()
            .to_string();
        let thumb_enc_idx = thumb_node
            .and_then(|node| node.attribute("enc_idx"))
            .unwrap_or_default()
            .to_string();
        let id = child_text(media, "id");
        let size = media.children().find(|node| node.has_tag_name("size"));
        let width = size
            .and_then(|node| node.attribute("width"))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let height = size
            .and_then(|node| node.attribute("height"))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let kind = if media_type == 4 || media_type == 6 {
            "video"
        } else {
            "image"
        };
        output.push(SnsMedia {
            id,
            kind: kind.to_string(),
            url,
            thumb_url,
            md5,
            token,
            key,
            enc_idx,
            thumb_token,
            thumb_key,
            thumb_enc_idx,
            width,
            height,
        });
    }
    output
}

fn parse_location(node: Node) -> String {
    let city = node.attribute("city").unwrap_or_default().trim();
    let poi = node.attribute("poiName").unwrap_or_default().trim();
    let address = node.attribute("address").unwrap_or_default().trim();
    first_non_empty([poi.to_string(), city.to_string(), address.to_string()])
}

fn child_text(node: Node, tag: &str) -> String {
    node.children()
        .find(|child| child.has_tag_name(tag))
        .map(node_text)
        .unwrap_or_default()
}

fn node_text(node: Node) -> String {
    let text = node.text().unwrap_or_default().trim().to_string();
    if text.is_empty() {
        node.descendants()
            .filter_map(|child| child.text())
            .collect::<String>()
            .trim()
            .to_string()
    } else {
        text
    }
}

fn first_non_empty<const N: usize>(values: [String; N]) -> String {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

pub fn decode_sns_content(raw: &str) -> String {
    let value = crate::sqlite::SqlValue::Text(raw.to_string());
    let decoded = crate::decode::decode_message_content(None, &value);
    if decoded.trim_start().starts_with('<') {
        decoded
    } else {
        raw.to_string()
    }
}

fn sanitize_ampersands(xml: &str) -> String {
    let mut output = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<![CDATA[") {
        output.push_str(&sanitize_segment(&rest[..start]));
        let after = &rest[start..];
        if let Some(end) = after.find("]]>") {
            output.push_str(&after[..end + 3]);
            rest = &after[end + 3..];
        } else {
            output.push_str(&sanitize_segment(after));
            rest = "";
            break;
        }
    }
    output.push_str(&sanitize_segment(rest));
    output
}

fn sanitize_segment(xml: &str) -> String {
    let mut output = String::with_capacity(xml.len());
    let mut chars: Peekable<Chars<'_>> = xml.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '&' {
            output.push(ch);
            continue;
        }
        let rest: String = chars.clone().take(12).collect();
        let known = ["amp;", "lt;", "gt;", "quot;", "apos;", "#"]
            .iter()
            .any(|prefix| rest.starts_with(prefix));
        if known {
            output.push('&');
        } else {
            output.push_str("&amp;");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_post_with_likes_and_comments() {
        let xml = r#"<SnsDataItem><TimelineObject>
            <username>wxid_a</username><createTime>1700000000</createTime>
            <contentDesc>hello &amp; world</contentDesc>
            <location poiName="人民广场" city="上海"/>
            <ContentObject><type>1</type></ContentObject>
        </TimelineObject><LocalExtraInfo><nickname>甲</nickname>
            <like_user_list><user_comment><username>wxid_b</username><nickname>乙</nickname></user_comment></like_user_list>
            <comment_user_list><user_comment><comment_64id>9</comment_64id><username>wxid_c</username><nickname>丙</nickname><content>不错</content></user_comment></comment_user_list>
        </LocalExtraInfo></SnsDataItem>"#;
        let post = parse_sns_post(-1, xml).expect("post");
        assert_eq!(post.text, "hello & world");
        assert_eq!(post.display_name, "甲");
        assert_eq!(post.location, "人民广场");
        assert_eq!(post.likes.len(), 1);
        assert_eq!(post.comments[0].content, "不错");
        assert_eq!(post.tid, u64::MAX.to_string());
    }

    #[test]
    fn parses_image_media() {
        let xml = r#"<SnsDataItem><TimelineObject>
            <username>wxid_a</username><createTime>1</createTime><contentDesc>x</contentDesc>
            <ContentObject><type>2</type><mediaList><media><type>2</type>
                <thumb>https://example.com/t.jpg</thumb>
                <url md5="0123456789abcdef0123456789abcdef">https://example.com/a.jpg</url>
                <size width="100" height="200"/>
            </media></mediaList></ContentObject>
        </TimelineObject></SnsDataItem>"#;
        let post = parse_sns_post(1, xml).expect("post");
        assert_eq!(post.media.len(), 1);
        assert_eq!(post.media[0].width, 100);
        assert_eq!(post.media[0].md5, "0123456789abcdef0123456789abcdef");
    }
}
