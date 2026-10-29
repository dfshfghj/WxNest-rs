use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_ACCOUNT_BYTES: usize = 256;
const MAX_RECIPIENT_BYTES: usize = 511;
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionMode {
    Disabled,
    DryRun,
    Native,
}

impl ActionMode {
    pub fn from_arg(value: &str) -> Option<Self> {
        match value {
            "disabled" => Some(Self::Disabled),
            "dry-run" => Some(Self::DryRun),
            "native" => Some(Self::Native),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::DryRun => "dry-run",
            Self::Native => "native",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ActionSpec {
    name: &'static str,
    id: u32,
    payload: &'static str,
}

const ACTIONS: [ActionSpec; 13] = [
    ActionSpec {
        name: "message.send_text",
        id: 1,
        payload: "text",
    },
    ActionSpec {
        name: "message.pat",
        id: 6,
        payload: "target username",
    },
    ActionSpec {
        name: "moments.refresh",
        id: 2,
        payload: "optional target username",
    },
    ActionSpec {
        name: "moments.like",
        id: 3,
        payload: "0|1\\x1ftarget username",
    },
    ActionSpec {
        name: "moments.comment_image",
        id: 4,
        payload: "target username\\x1fabsolute image path",
    },
    ActionSpec {
        name: "moments.publish",
        id: 5,
        payload: "privacy\\x1fcontent\\x1fimages\\x1freminders\\x1fprivacy users",
    },
    ActionSpec {
        name: "group.set_self_nickname",
        id: 7,
        payload: "nickname",
    },
    ActionSpec {
        name: "group.publish_announcement",
        id: 8,
        payload: "announcement",
    },
    ActionSpec {
        name: "message.send_image",
        id: 9,
        payload: "absolute image path",
    },
    ActionSpec {
        name: "message.send_file",
        id: 10,
        payload: "absolute file path",
    },
    ActionSpec {
        name: "message.send_voice",
        id: 11,
        payload: "absolute audio pathduration_ms",
    },
    ActionSpec {
        name: "message.send_emoticon",
        id: 12,
        payload: "absolute image path",
    },
    ActionSpec {
        name: "message.send_at_text",
        id: 13,
        payload: "member1,member2text",
    },
];

#[derive(Debug, Deserialize)]
pub struct ActionRequest {
    pub action: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub account_directory: String,
    #[serde(default)]
    pub recipient: String,
    #[serde(default)]
    pub payload: String,
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

fn default_dry_run() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct ActionPlan {
    pub status: &'static str,
    pub mode: &'static str,
    pub action: String,
    pub action_id: u32,
    pub account: String,
    pub account_directory: String,
    pub recipient_bytes: usize,
    pub payload_bytes: usize,
    pub executed: bool,
    pub note: &'static str,
}

pub fn catalog(mode: ActionMode) -> Value {
    let actions: Vec<Value> = if mode == ActionMode::Disabled {
        Vec::new()
    } else {
        ACTIONS
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "id": spec.id,
                    "payload": spec.payload,
                    "native": mode == ActionMode::Native
                        && matches!(
                            spec.name,
                            "message.send_text"
                                | "message.send_image"
                                | "message.send_file"
                                | "message.send_voice"
                                | "message.send_emoticon"
                                | "message.send_at_text"
                                | "moments.publish"
                                | "moments.like"
                                | "moments.comment_image"
                        ),
                    "implemented": matches!(
                        spec.name,
                        "message.send_text"
                            | "message.send_image"
                            | "message.send_file"
                            | "message.send_voice"
                            | "message.send_emoticon"
                            | "message.send_at_text"
                            | "moments.publish"
                            | "moments.like"
                            | "moments.comment_image"
                            | "moments.refresh"
                    ),
                })
            })
            .collect()
    };
    json!({
        "mode": mode.label(),
        "actions": actions,
        "execution": if mode == ActionMode::Native {
            "native:message.send_text"
        } else {
            "dry-run"
        },
    })
}

pub fn plan(mode: ActionMode, request: &ActionRequest) -> Result<ActionPlan, String> {
    if mode == ActionMode::Disabled {
        return Err("actions are disabled".to_string());
    }
    let spec = ACTIONS
        .iter()
        .find(|spec| spec.name == request.action)
        .ok_or_else(|| format!("unsupported action: {}", request.action))?;
    validate_common(request)?;
    match spec.name {
        "message.send_text" => {
            if request.recipient.trim().is_empty() {
                return Err("recipient is empty".to_string());
            }
            if request.payload.is_empty() {
                return Err("text payload is empty".to_string());
            }
        }
        "message.pat" => {
            if request.recipient.trim().is_empty() {
                return Err("recipient is empty".to_string());
            }
        }
        "moments.refresh" => {}
        "moments.like" => {
            validate_feed_id(&request.recipient)?;
            let (liked, target) = request
                .payload
                .split_once('\u{1f}')
                .ok_or_else(|| "moments.like payload must be 0|1\\x1fuser".to_string())?;
            if !matches!(liked, "0" | "1") || target.trim().is_empty() || target.contains('\u{1f}')
            {
                return Err("moments.like payload must be 0|1\\x1fuser".to_string());
            }
        }
        "moments.comment_image" => {
            validate_feed_id(&request.recipient)?;
            let (target, image) = request
                .payload
                .split_once('\u{1f}')
                .ok_or_else(|| "moments.comment_image payload must be user\\x1fpath".to_string())?;
            if target.trim().is_empty() || target.contains('\u{1f}') {
                return Err("moments.comment_image target is invalid".to_string());
            }
            if !Path::new(image).is_absolute() {
                return Err("moments.comment_image path must be absolute".to_string());
            }
        }
        "moments.publish" => {
            validate_moments_publish(request)?;
        }
        "group.set_self_nickname" => {
            if request.recipient.trim().is_empty() {
                return Err("recipient is empty".to_string());
            }
            if request.payload.is_empty() {
                return Err("nickname payload is empty".to_string());
            }
        }
        "group.publish_announcement" => {
            if request.recipient.trim().is_empty() {
                return Err("recipient is empty".to_string());
            }
            if request.payload.is_empty() {
                return Err("announcement payload is empty".to_string());
            }
        }
        "message.send_image" | "message.send_file" | "message.send_emoticon" => {
            if request.recipient.trim().is_empty() {
                return Err("recipient is empty".to_string());
            }
            if !Path::new(&request.payload).is_absolute() {
                return Err("payload must be an absolute path".to_string());
            }
        }
        "message.send_voice" => {
            if request.recipient.trim().is_empty() {
                return Err("recipient is empty".to_string());
            }
            let (path, duration) = request
                .payload
                .split_once('')
                .ok_or_else(|| "voice payload must be path\x1fduration".to_string())?;
            if !Path::new(path).is_absolute() {
                return Err("voice path must be absolute".to_string());
            }
            if duration
                .parse::<u32>()
                .map(|value| value == 0 || value > 600_000)
                .unwrap_or(true)
            {
                return Err("voice duration is invalid".to_string());
            }
        }
        "message.send_at_text" => {
            if !request.recipient.ends_with("@chatroom") {
                return Err("at text recipient must be a group".to_string());
            }
            let (members, text) = request
                .payload
                .split_once('')
                .ok_or_else(|| "at text payload must be members\x1ftext".to_string())?;
            if members.is_empty() || text.is_empty() {
                return Err("at text members or text is empty".to_string());
            }
        }
        _ => {}
    }
    Ok(ActionPlan {
        status: "dry_run",
        mode: mode.label(),
        action: spec.name.to_string(),
        action_id: spec.id,
        account: request.account.clone(),
        account_directory: request.account_directory.clone(),
        recipient_bytes: request.recipient.len(),
        payload_bytes: request.payload.len(),
        executed: false,
        note: "dry-run only; no WeChat process or native core is touched",
    })
}

pub fn not_implemented(plan: &ActionPlan) -> Value {    json!({
        "status": "not_implemented",
        "action": plan.action,
        "action_id": plan.action_id,
        "executed": false,
        "reason": "write actions require a licensed WeChat native backend; this broker is read-only",
    })
}

fn validate_common(request: &ActionRequest) -> Result<(), String> {
    if request.account.trim().is_empty() {
        return Err("account is empty".to_string());
    }
    if request.account_directory.trim().is_empty() {
        return Err("account_directory is empty".to_string());
    }
    if request.account.len() > MAX_ACCOUNT_BYTES {
        return Err("account is too long".to_string());
    }
    if request.recipient.len() > MAX_RECIPIENT_BYTES {
        return Err("recipient is too long".to_string());
    }
    if request.payload.len() > MAX_PAYLOAD_BYTES {
        return Err("payload is too large".to_string());
    }
    Ok(())
}

fn validate_feed_id(value: &str) -> Result<(), String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| "feed id must be an unsigned 64-bit decimal value".to_string())?;
    if parsed == 0 {
        return Err("feed id must be positive".to_string());
    }
    Ok(())
}

fn validate_moments_publish(request: &ActionRequest) -> Result<(), String> {
    if !Path::new(&request.recipient).is_absolute() {
        return Err("moments.publish recipient must be an absolute wxid directory".to_string());
    }
    let fields: Vec<&str> = request.payload.split('\u{1f}').collect();
    if fields.len() != 5 {
        return Err("moments.publish payload must contain 5 fields".to_string());
    }
    let privacy = fields[0]
        .parse::<u8>()
        .map_err(|_| "moments.publish privacy must be 1, 2, 3, or 4".to_string())?;
    if !(1..=4).contains(&privacy) {
        return Err("moments.publish privacy must be 1, 2, 3, or 4".to_string());
    }
    let content = fields[1];
    let images = split_list(fields[2]);
    let reminders = split_list(fields[3]);
    let privacy_users = split_list(fields[4]);
    if images.len() > 9 {
        return Err("moments.publish supports at most 9 images".to_string());
    }
    if content.is_empty() && images.is_empty() {
        return Err("moments.publish content and images are both empty".to_string());
    }
    if privacy >= 3 && privacy_users.is_empty() {
        return Err("moments.publish privacy 3/4 requires privacy users".to_string());
    }
    if privacy <= 2 && !privacy_users.is_empty() {
        return Err("moments.publish privacy 1/2 must not have privacy users".to_string());
    }
    for value in images
        .iter()
        .chain(reminders.iter())
        .chain(privacy_users.iter())
    {
        if value.contains('\u{1e}') || value.contains('\u{1f}') {
            return Err("moments.publish list value contains a reserved separator".to_string());
        }
        if images.contains(value) && !Path::new(value).is_absolute() {
            return Err("moments.publish image path must be absolute".to_string());
        }
    }
    Ok(())
}

fn split_list(value: &str) -> Vec<&str> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split('\u{1e}').collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(action: &str, recipient: &str, payload: &str) -> ActionRequest {
        ActionRequest {
            action: action.to_string(),
            account: "wxid_test".to_string(),
            account_directory: "C:\\wxid_test".to_string(),
            recipient: recipient.to_string(),
            payload: payload.to_string(),
            dry_run: true,
        }
    }

    #[test]
    fn catalog_lists_manifest_actions() {
        let value = catalog(ActionMode::DryRun);
        assert_eq!(value["mode"], "dry-run");
        assert_eq!(value["actions"].as_array().unwrap().len(), 13);
    }

    #[test]
    fn rejects_unknown_action() {
        let error = plan(ActionMode::DryRun, &request("message.delete", "", "")).unwrap_err();
        assert!(error.contains("unsupported"));
    }

    #[test]
    fn plans_send_text_without_execution() {
        let plan = plan(
            ActionMode::DryRun,
            &request("message.send_text", "wxid_friend", "hi"),
        )
        .unwrap();
        assert_eq!(plan.action_id, 1);
        assert!(!plan.executed);
    }

    #[test]
    fn rejects_empty_text() {
        assert!(plan(
            ActionMode::DryRun,
            &request("message.send_text", "wxid_friend", "")
        )
        .is_err());
    }

    #[test]
    fn validates_moments_like_payload() {
        assert!(plan(
            ActionMode::DryRun,
            &request("moments.like", "42", "1\u{1f}wxid_friend")
        )
        .is_ok());
        assert!(plan(
            ActionMode::DryRun,
            &request("moments.like", "42", "2\u{1f}wxid_friend")
        )
        .is_err());
        assert!(plan(
            ActionMode::DryRun,
            &request("moments.like", "0", "1\u{1f}wxid_friend")
        )
        .is_err());
    }

    #[test]
    fn validates_moments_publish_payload() {
        let dir = std::env::temp_dir().to_string_lossy().to_string();
        let image = std::env::temp_dir()
            .join("image.jpg")
            .to_string_lossy()
            .to_string();
        let payload = format!("1\u{1f}hello\u{1f}{image}\u{1f}\u{1f}");
        assert!(plan(
            ActionMode::DryRun,
            &request("moments.publish", &dir, &payload)
        )
        .is_ok());
        let invalid = "3\u{1f}hello\u{1f}\u{1f}\u{1f}";
        assert!(plan(
            ActionMode::DryRun,
            &request("moments.publish", &dir, invalid)
        )
        .is_err());
    }

    #[test]
    fn disabled_mode_rejects_everything() {
        let error = plan(
            ActionMode::Disabled,
            &request("message.pat", "wxid_friend", "wxid_friend"),
        )
        .unwrap_err();
        assert!(error.contains("disabled"));
    }
}
