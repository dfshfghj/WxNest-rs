use serde::{Deserialize, Serialize};

use crate::message_body::MessageBody;

#[derive(Debug, Clone, Serialize)]
pub struct AccountInfo {
    pub account: String,
    pub account_dir: String,
    pub message_shards: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub username: String,
    pub display_name: String,
    pub is_group: bool,
    pub is_official: bool,
    pub is_hidden: bool,
    pub summary: String,
    pub draft: String,
    pub last_msg_type: Option<i64>,
    pub last_msg_sub_type: Option<i64>,
    pub sort_timestamp: Option<i64>,
    pub last_timestamp: Option<i64>,
    pub last_time_text: String,
    pub has_avatar: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvatarImage {
    pub username: String,
    pub media_type: String,
    pub md5: String,
    pub update_time: i64,
    pub size: usize,
    #[serde(skip)]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contact {
    pub username: String,
    pub display_name: String,
    pub remark: String,
    pub nick_name: String,
    pub alias: String,
    pub big_head_url: Option<String>,
    pub small_head_url: Option<String>,
    pub is_group: bool,
    pub is_official: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContactItem {
    pub username: String,
    pub display_name: String,
    pub remark: String,
    pub nick_name: String,
    pub alias: String,
    pub description: String,
    pub local_type: i64,
    pub flag: i64,
    pub is_group: bool,
    pub is_official: bool,
    pub official_kind: Option<String>,
    pub pinyin_initial: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    Image,
    Voice,
    Video,
    Emoji,
    Location,
    App,
    Voip,
    System,
    Quote,
    Pat,
    File,
    Live,
    Other,
}

impl MessageKind {
    pub fn from_local_type(local_type: i64) -> Self {
        match local_type {
            1 => Self::Text,
            3 => Self::Image,
            34 => Self::Voice,
            43 | 62 => Self::Video,
            47 => Self::Emoji,
            48 => Self::Location,
            49 => Self::App,
            50 => Self::Voip,
            10_000 => Self::System,
            _ => {
                if local_type > u32::MAX as i64 {
                    match local_type >> 32 {
                        8 => Self::Emoji,
                        6 | 74 => Self::File,
                        57 => Self::Quote,
                        62 => Self::Pat,
                        63 => Self::Live,
                        _ => Self::App,
                    }
                } else {
                    Self::Other
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub local_id: i64,
    pub server_id: i64,
    pub create_time: i64,
    pub sort_seq: i64,
    pub local_type: i64,
    pub kind: MessageKind,
    pub sender_username: String,
    pub sender_display_name: String,
    pub is_sent: bool,
    pub content: String,

    #[serde(skip)]
    pub media_md5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<MessageBody>,
    pub shard: String,
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageCursor {
    pub create_time: i64,
    pub sort_seq: i64,
    pub local_id: i64,
    pub shard: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub has_more: bool,
    pub next_before: Option<MessageCursor>,
}

#[cfg(test)]
mod tests {
    use super::MessageKind;

    #[test]
    fn decodes_wechat4_appmsg_local_types() {
        assert_eq!(MessageKind::from_local_type(49), MessageKind::App);
        assert_eq!(
            MessageKind::from_local_type(17_179_869_233),
            MessageKind::App
        );
        assert_eq!(
            MessageKind::from_local_type(81_604_378_673),
            MessageKind::App
        );
        assert_eq!(
            MessageKind::from_local_type(373_662_154_801),
            MessageKind::App
        );
        assert_eq!(
            MessageKind::from_local_type(25_769_803_825),
            MessageKind::File
        );
        assert_eq!(
            MessageKind::from_local_type(34_359_738_417),
            MessageKind::Emoji
        );
        assert_eq!(
            MessageKind::from_local_type(244_813_135_921),
            MessageKind::Quote
        );
        assert_eq!(
            MessageKind::from_local_type(266_287_972_401),
            MessageKind::Pat
        );
        assert_eq!(
            MessageKind::from_local_type(270_582_939_697),
            MessageKind::Live
        );
    }
}
