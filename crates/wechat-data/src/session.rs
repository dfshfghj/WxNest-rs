pub fn is_group_username(username: &str) -> bool {
    username.ends_with("@chatroom")
}

pub fn is_official_username(username: &str) -> bool {
    username.starts_with("gh_")
}

pub fn should_keep_session(username: &str, include_official: bool) -> bool {
    if username.is_empty() {
        return false;
    }
    if !include_official && username.starts_with("gh_") {
        return false;
    }
    if username.starts_with("weixin")
        || username.starts_with("qqmail")
        || username.starts_with("fmessage")
        || username.starts_with("medianote")
        || username.starts_with("floatbottle")
        || username.starts_with("newsapp")
    {
        return false;
    }
    if username.contains("@kefu.openim") || username.contains("service_") {
        return false;
    }
    if matches!(
        username,
        "brandsessionholder"
            | "brandservicesessionholder"
            | "notifymessage"
            | "opencustomerservicemsg"
            | "notification_messages"
            | "userexperience_alarm"
    ) {
        return false;
    }
    username.ends_with("@chatroom")
        || username.ends_with("@openim")
        || username.starts_with("wxid_")
        || !username.contains('@')
}

pub fn display_name(remark: &str, nick_name: &str, alias: &str, fallback_username: &str) -> String {
    for value in [remark, nick_name, alias] {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }
    fallback_username.to_string()
}

pub fn normalize_avatar_url(value: &str) -> Option<String> {
    let text = value.trim();
    if text.to_ascii_lowercase().starts_with("http://")
        || text.to_ascii_lowercase().starts_with("https://")
    {
        Some(text.to_string())
    } else {
        None
    }
}
