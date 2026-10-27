use crate::model::ContactItem;

pub struct ContactRow {
    pub username: String,
    pub remark: String,
    pub nick_name: String,
    pub alias: String,
    pub local_type: i64,
    pub flag: i64,
    pub description: String,
    pub pinyin: String,
}

const SYSTEM_USERNAMES: &[&str] = &[
    "filehelper",
    "fmessage",
    "floatbottle",
    "medianote",
    "notifymessage",
    "newsapp",
    "qqmail",
    "qmessage",
    "tmessage",
    "weixin",
    "mphelper",
    "brandsessionholder",
];

pub fn is_system_username(username: &str) -> bool {
    let lower = username.trim().to_ascii_lowercase();
    SYSTEM_USERNAMES.contains(&lower.as_str())
        || lower.starts_with("fake_")
        || lower.starts_with("gh_f0a92aa7146c")
}

pub fn is_group_username(username: &str) -> bool {
    username.ends_with("@chatroom")
}

pub fn is_official_username(username: &str, local_type: i64) -> bool {
    username == "weixin" || username.starts_with("gh_") || local_type == 4
}

pub fn normalize_contact(row: ContactRow, self_names: &[String]) -> Option<ContactItem> {
    let username = row.username.trim().to_string();
    if username.is_empty() || is_system_username(&username) {
        return None;
    }
    if self_names
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&username))
    {
        return None;
    }
    let is_group = is_group_username(&username);
    let is_official = !is_group && is_official_username(&username, row.local_type);
    let enterprise = username.contains("@openim");
    if !(is_group || is_official || enterprise || matches!(row.local_type, 1 | 4 | 5)) {
        return None;
    }
    let (remark, nick_name, alias) = (row.remark.trim(), row.nick_name.trim(), row.alias.trim());
    let display_name = if !remark.is_empty() {
        remark
    } else if !nick_name.is_empty() {
        nick_name
    } else if !alias.is_empty() {
        alias
    } else {
        username.as_str()
    }
    .to_string();
    let pinyin_initial = initial_key(&row.pinyin, &display_name);
    Some(ContactItem {
        username,
        display_name,
        remark: remark.to_string(),
        nick_name: nick_name.to_string(),
        alias: alias.to_string(),
        description: row.description.trim().to_string(),
        local_type: row.local_type,
        flag: row.flag,
        is_group,
        is_official,
        official_kind: None,
        pinyin_initial,
    })
}

pub fn contact_matches_keyword(item: &ContactItem, keyword: &str) -> bool {
    let keyword = keyword.trim().to_lowercase();
    if keyword.is_empty() {
        return true;
    }
    [
        item.username.as_str(),
        item.display_name.as_str(),
        item.remark.as_str(),
        item.nick_name.as_str(),
        item.alias.as_str(),
        item.description.as_str(),
    ]
    .iter()
    .any(|value| value.to_lowercase().contains(&keyword))
}

fn initial_key(pinyin: &str, display_name: &str) -> String {
    let pinyin = pinyin.trim();
    if let Some(ch) = pinyin.chars().next() {
        let ch = ch.to_ascii_uppercase();
        if ch.is_ascii_uppercase() {
            return ch.to_string();
        }
    }
    if let Some(ch) = display_name.chars().next() {
        let ch = ch.to_ascii_uppercase();
        if ch.is_ascii_uppercase() {
            return ch.to_string();
        }
    }
    "#".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(username: &str, local_type: i64) -> ContactRow {
        ContactRow {
            username: username.into(),
            remark: String::new(),
            nick_name: "昵称".into(),
            alias: String::new(),
            local_type,
            flag: 0,
            description: String::new(),
            pinyin: "NC".into(),
        }
    }

    #[test]
    fn keeps_friends_groups_and_officials() {
        assert!(normalize_contact(row("wxid_a", 1), &[]).is_some());
        assert!(normalize_contact(row("123@chatroom", 2), &[]).is_some());
        assert!(normalize_contact(row("gh_abc", 0), &[]).is_some());
        assert!(normalize_contact(row("wxid_stranger", 3), &[]).is_none());
        assert!(normalize_contact(row("filehelper", 0), &[]).is_none());
    }

    #[test]
    fn excludes_self_and_matches_keyword() {
        let self_names = vec!["wxid_self".to_string()];
        assert!(normalize_contact(row("wxid_self", 1), &self_names).is_none());
        let item = normalize_contact(row("wxid_a", 1), &[]).expect("contact");
        assert!(contact_matches_keyword(&item, "昵"));
        assert!(!contact_matches_keyword(&item, "不存在"));
    }
}
