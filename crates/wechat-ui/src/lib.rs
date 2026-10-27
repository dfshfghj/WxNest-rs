use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{Datelike, Local, TimeZone};
use serde::{Deserialize, Serialize};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Bounds, ClipboardItem, Context, DispatchPhase, Element, ElementId, Entity,
    GlobalElementId, HitboxBehavior, Image, ImageFormat, InspectorElementId, InteractiveElement,
    IntoElement, LayoutId, ListAlignment, ListOffset, ListState, MouseButton, ObjectFit,
    ParentElement, PathPromptOptions, Point, Render, ScrollHandle, ScrollWheelEvent, Subscription,
    Task, TestSupportExt as _, TitlebarOptions, Window, WindowBounds, WindowOptions, div, img,
    point, px, rgb, size, svg,
};
use wechat_data::{
    AccountInfo, AccountStore, AvatarImage, ContactItem, FinderLive, ImageKeys, KeyStore, Message,
    MessageBody, MessageCursor, MessageKind, MessageQuery, MiniProgram, PaymentItem,
    RealtimeAccountStore, SearchHit, SearchQuery, Session, SnsPost, build_search_index,
    cache_media_bytes, chat_search_index_path, discover_accounts, extract_image_md5,
    fetch_emoticon_media, fetch_tencent_media, find_image_keys_for_db_storage,
    find_key_for_db_storage_with_account, find_resource_image, load_emoji_image,
    load_message_image, load_message_image_cached, load_message_image_original, load_sns_image,
    normalize_tencent_url, parse_emoji_reference, recover_image_key_for_account,
    recover_key_for_db_storage, search_index, set_media_root,
};

mod tray;

enum Store {
    Decrypted(Box<AccountStore>),
    Realtime(Box<RealtimeAccountStore>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Chat,
    Moments,
    Contacts,
    Finder,
    MiniPrograms,
    Payments,
    Search,
}

#[derive(Clone)]
struct RealtimeParams {
    broker: PathBuf,
    root: PathBuf,
    key: String,
    raw_key: bool,
    account: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AppSettings {
    #[serde(default)]
    account_path: Option<PathBuf>,
    #[serde(default)]
    storage_dir: Option<PathBuf>,
    #[serde(default)]
    media_root: Option<PathBuf>,
    #[serde(default)]
    key_store: Option<PathBuf>,
    #[serde(default)]
    broker: Option<PathBuf>,
    #[serde(default = "default_remote_cdn")]
    remote_cdn: bool,
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default = "default_minimize_to_tray")]
    minimize_to_tray: bool,
}

fn default_remote_cdn() -> bool {
    true
}

fn default_minimize_to_tray() -> bool {
    true
}

fn default_theme() -> String {
    "light".to_string()
}

fn remote_cdn_allowed() -> bool {
    std::env::var("WCE_REMOTE_CDN").ok().as_deref() != Some("0")
}

fn locate_db_storage(path: &Path) -> PathBuf {
    if path.file_name().is_some_and(|name| name == "db_storage") {
        return path.to_path_buf();
    }
    let direct = path.join("db_storage");
    if direct.is_dir() {
        return direct;
    }
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("db_storage");
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    path.to_path_buf()
}

fn find_existing_key_store(
    storage: &Path,
    wechat: &Path,
    settings: &AppSettings,
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.push(storage.join("account_keys.json"));
    if let Some(path) = settings.key_store.as_ref() {
        candidates.push(path.clone());
    }
    if let Some(path) = std::env::var_os("WECHAT_KEY_STORE") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(parent) = wechat.parent() {
        candidates.push(parent.join("account_keys.json"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("account_keys.json"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(target_os = "windows")]
pub fn install_tray(window: &mut Window, cx: &mut App) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let hwnd = HasWindowHandle::window_handle(window)
        .ok()
        .and_then(|handle| match handle.as_raw() {
            RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
            _ => None,
        });
    if let Some(hwnd) = hwnd {
        tray::install(hwnd);
    }
    window.on_window_should_close(cx, |_, _| {
        if tray::allow_close_requested() {
            tray::shutdown();
            return true;
        }
        if minimize_to_tray() && tray::hide_main_window() {
            return false;
        }
        tray::shutdown();
        true
    });
}

fn find_broker(storage: &Path, settings: &AppSettings) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("wce-self-broker.exe"));
    }
    if let Some(path) = settings.broker.as_ref() {
        candidates.push(path.clone());
    }
    candidates.push(storage.join("wce-self-broker.exe"));
    if let Some(path) = std::env::var_os("WCE_SELF_BROKER") {
        candidates.push(PathBuf::from(path));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn write_recovered_key_store(
    path: &Path,
    db_storage: &Path,
    key: &wechat_data::RecoveredKey,
) -> Result<(), String> {
    let account = db_storage
        .parent()
        .and_then(|parent| parent.file_name())
        .or_else(|| db_storage.file_name())
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "无法确定账号名".to_string())?;
    let image = db_storage
        .parent()
        .and_then(|account_dir| recover_image_key_for_account(account_dir, None).ok());
    let mut entry = serde_json::Map::new();
    entry.insert(
        "db_key".to_string(),
        serde_json::Value::String(key.key_hex.clone()),
    );
    if let Some(image) = image {
        entry.insert(
            "image_aes_key".to_string(),
            serde_json::Value::String(image.aes_key),
        );
        entry.insert(
            "image_xor_key".to_string(),
            serde_json::Value::Number(image.xor_key.into()),
        );
    }
    let payload = serde_json::json!({ account: serde_json::Value::Object(entry) });
    let text = serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, text).map_err(|error| error.to_string())
}

#[derive(Clone, Copy)]
enum SettingsField {
    Account,
    Storage,
}

#[derive(Clone, Copy)]
struct PathRowSpec {
    label: &'static str,
    hint: &'static str,
    field: SettingsField,
    directory: bool,
    prompt: &'static str,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            account_path: None,
            storage_dir: None,
            media_root: None,
            key_store: None,
            broker: None,
            remote_cdn: true,
            theme: "light".to_string(),
            minimize_to_tray: true,
        }
    }
}

fn settings_path() -> PathBuf {
    if let Some(path) = std::env::var_os("WCE_SETTINGS_PATH") {
        return PathBuf::from(path);
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Path::new(&appdata)
            .join("wechat-analysis-rs")
            .join("settings.json");
    }
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        return Path::new(&config)
            .join("wechat-analysis-rs")
            .join("settings.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home)
            .join(".config")
            .join("wechat-analysis-rs")
            .join("settings.json");
    }
    PathBuf::from("settings.json")
}

fn load_settings() -> Option<AppSettings> {
    if cfg!(test) && std::env::var_os("WCE_SETTINGS_PATH").is_none() {
        return None;
    }
    let text = std::fs::read_to_string(settings_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_settings(settings: &AppSettings) -> Result<(), String> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    std::fs::write(path, text).map_err(|error| error.to_string())
}

static DARK_MODE: AtomicBool = AtomicBool::new(false);
static MINIMIZE_TO_TRAY: AtomicBool = AtomicBool::new(true);

fn set_dark_mode(dark: bool) {
    DARK_MODE.store(dark, Ordering::Relaxed);
}

fn dark_mode() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}

fn set_minimize_to_tray(enabled: bool) {
    MINIMIZE_TO_TRAY.store(enabled, Ordering::Relaxed);
}

fn minimize_to_tray() -> bool {
    MINIMIZE_TO_TRAY.load(Ordering::Relaxed)
}

fn ui_color(light: u32) -> gpui_kit::Rgba {
    if !dark_mode() {
        return rgb(light);
    }
    let mapped = match light {
        0xffffff => 0x2b2b2b,
        0xfcfcfc | 0xfafafa | 0xf8faf8 | 0xf7f7f7 | 0xf5f7f5 | 0xf5f5f5 | 0xf3faf6 | 0xf3f3f3
        | 0xf2f2f2 | 0xf0f0f0 | 0xefefef | 0xeeeeee | 0xededed | 0xebebeb | 0xeaeaea | 0xe9f8ef
        | 0xe8eaec | 0xe5e5e5 | 0xe4e6e8 | 0xe3e3e3 | 0xe0e0e0 | 0xdedede => 0x252525,
        0xe5e7eb | 0xe2e2e2 | 0xdcdcdc | 0xd6d6d6 | 0xe7e7e7 | 0xececec => 0x3a3a3a,
        0x191919 | 0x1f1f1f | 0x111827 | 0x222222 | 0x1f2937 => 0xe8e8e8,
        0x6b7280 | 0x8a8a8a | 0x9e9e9e | 0x9aa0a6 | 0x8a919b | 0x7a7a7a | 0x4b5563 | 0x555b56
        | 0x909090 => 0xa8a8a8,
        0x95ec69 => 0x3d6b2f,
        0x07b75b | 0x07964c | 0x058f48 => 0x35c47f,
        0xe4f6ec => 0x1f3a2a,
        0x2f6f4e => 0x4b9a70,
        0x576b95 => 0x86a9d8,
        0x4a90c4 | 0x2f80ed => 0x6fa8e8,
        0xe35b3a | 0xfa9d3b => 0xff9a72,
        0x9a5050 | 0xc0764f => 0xe0a090,
        0xb07a2c => 0xd6a34c,
        0xfbf1e8 | 0xfaecda | 0xfdf0e9 | 0xfbf4e8 => 0x3a3128,
        0xdbeafe | 0xeef1f6 | 0xf3f4f6 => 0x2e3640,
        other => other,
    };
    rgb(mapped)
}

fn settings_step_row(text: &str, done: bool) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(18.))
                .h(px(18.))
                .flex_none()
                .rounded_full()
                .bg(if done {
                    ui_color(0x07b75b)
                } else {
                    ui_color(0xd1d5db)
                })
                .text_xs()
                .text_color(ui_color(0xffffff))
                .flex()
                .items_center()
                .justify_center()
                .child(if done { "✓" } else { "·" }),
        )
        .child(
            div()
                .text_xs()
                .text_color(ui_color(0x4b5563))
                .child(text.to_string()),
        )
        .into_any_element()
}

#[derive(Clone)]
enum MessageListItem {
    OlderButton,
    Day(i64),
    Message(usize),
}

#[derive(Clone)]
struct CachedSession {
    messages: Vec<Message>,
    older_cursor: Option<MessageCursor>,
    has_more_older: bool,
    loaded_at: std::time::Instant,
}

type WheelCallback = Rc<dyn Fn(&ScrollWheelEvent, &mut Window, &mut App)>;

const ICON_CHAT: &[u8] = include_bytes!("../assets/icons/chat.svg");
const ICON_MOMENTS: &[u8] = include_bytes!("../assets/icons/moments.svg");
const ICON_CONTACTS: &[u8] = include_bytes!("../assets/icons/contacts.svg");
const ICON_FINDER: &[u8] = include_bytes!("../assets/icons/finder.svg");
const ICON_MINI: &[u8] = include_bytes!("../assets/icons/mini.svg");
const ICON_PAYMENTS: &[u8] = include_bytes!("../assets/icons/payments.svg");
const ICON_SEARCH: &[u8] = include_bytes!("../assets/icons/search.svg");
const ICON_SETTINGS: &[u8] = include_bytes!("../assets/icons/settings.svg");

include!(concat!(env!("OUT_DIR"), "/emoji_assets.rs"));

struct SmoothWheel<E: Element> {
    child: E,
    on_wheel: WheelCallback,
}

impl<E: Element> IntoElement for SmoothWheel<E> {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl<E: Element> Element for SmoothWheel<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = (E::PrepaintState, gpui_kit::Hitbox);

    fn id(&self) -> Option<ElementId> {
        self.child.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.child.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.child.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<gpui_kit::Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let state = self
            .child
            .prepaint(id, inspector_id, bounds, request_layout, window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        (state, hitbox)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<gpui_kit::Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            &mut prepaint.0,
            window,
            cx,
        );
        let hitbox = prepaint.1.clone();
        let on_wheel = self.on_wheel.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture && hitbox.should_handle_scroll(window) {
                on_wheel(event, window, cx);
                cx.stop_propagation();
                window.prevent_default();
            }
        });
    }
}

#[derive(Clone)]
enum SmoothTarget {
    Handle(ScrollHandle),
    List(ListState),
}

impl SmoothTarget {
    fn offset_bounds(&self) -> (gpui_kit::Pixels, gpui_kit::Pixels) {
        match self {
            Self::Handle(handle) => {
                let max = handle.max_offset().y.max(px(0.));
                (handle.offset().y, -max)
            }
            Self::List(list) => {
                let max = list.max_offset_for_scrollbar().y.max(px(0.));
                (list.scroll_px_offset_for_scrollbar().y, -max)
            }
        }
    }

    fn set_offset_y(&self, y: gpui_kit::Pixels) {
        match self {
            Self::Handle(handle) => {
                let x = handle.offset().x;
                handle.set_offset(Point { x, y });
            }
            Self::List(list) => {
                list.set_offset_from_scrollbar(Point { x: px(0.), y });
            }
        }
    }

    fn scroll_down(&self, distance: gpui_kit::Pixels) -> bool {
        let (current, min) = self.offset_bounds();
        let next = (current - distance).clamp(min, px(0.));
        if (next - current).abs() < px(0.01) {
            return false;
        }
        self.set_offset_y(next);
        true
    }
}

struct ImagePreview {
    image: Arc<Image>,
    caption: String,
}

impl Render for ImagePreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("image-preview")
            .size_full()
            .bg(ui_color(0x0b0d0f))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(36.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui_color(0x9aa0a6))
                            .truncate()
                            .child(self.caption.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui_color(0x9aa0a6))
                            .child("点击任意位置关闭"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_4()
                    .child(
                        img(self.image.clone())
                            .size_full()
                            .object_fit(ObjectFit::Contain),
                    ),
            )
            .on_click(|_, window, _| window.remove_window())
    }
}

struct MediaViewStyle {
    fallback_label: &'static str,
    background: u32,
    foreground: u32,
}

impl Store {
    fn account(&self) -> &str {
        match self {
            Self::Decrypted(store) => store.account(),
            Self::Realtime(store) => store.account(),
        }
    }

    fn self_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        match self {
            Self::Decrypted(store) => {
                names.push(store.account().to_string());
                names.push(store.self_username().to_string());
                names.extend(store.self_candidates().iter().cloned());
            }
            Self::Realtime(store) => {
                names.push(store.account().to_string());
                names.push(store.self_username().to_string());
                names.extend(store.self_candidates().iter().cloned());
            }
        }
        names.retain(|name| !name.trim().is_empty());
        names.sort();
        names.dedup();
        names
    }

    fn account_dir(&self) -> Option<&Path> {
        match self {
            Self::Decrypted(store) => Some(store.account_dir()),
            Self::Realtime(_) => None,
        }
    }

    fn account_root(&self) -> Option<&Path> {
        match self {
            Self::Decrypted(store) => Some(store.account_dir()),
            Self::Realtime(store) => store.account_root(),
        }
    }

    fn list_sessions(
        &self,
        include_hidden: bool,
        include_official: bool,
    ) -> wechat_data::Result<Vec<Session>> {
        match self {
            Self::Decrypted(store) => store.list_sessions(include_hidden, include_official),
            Self::Realtime(store) => store.list_sessions(include_hidden, include_official),
        }
    }

    fn load_avatars(
        &self,
        usernames: &[String],
    ) -> wechat_data::Result<HashMap<String, AvatarImage>> {
        match self {
            Self::Decrypted(store) => store.load_avatars(usernames),
            Self::Realtime(store) => store.load_avatars(usernames),
        }
    }

    fn load_messages(
        &self,
        username: &str,
        query: &MessageQuery,
    ) -> wechat_data::Result<wechat_data::MessagePage> {
        match self {
            Self::Decrypted(store) => store.load_messages(username, query),
            Self::Realtime(store) => store.load_messages(username, query),
        }
    }

    fn list_contact_directory(
        &self,
        keyword: &str,
        limit: usize,
    ) -> wechat_data::Result<Vec<wechat_data::ContactItem>> {
        match self {
            Self::Decrypted(store) => store.list_contact_directory(keyword, limit),
            Self::Realtime(store) => store.list_contact_directory(keyword, limit),
        }
    }

    fn list_sns_posts(
        &self,
        before: Option<i64>,
        limit: usize,
    ) -> wechat_data::Result<Vec<wechat_data::SnsPost>> {
        match self {
            Self::Decrypted(store) => store.list_sns_posts(before, limit),
            Self::Realtime(store) => store.list_sns_posts(before, limit),
        }
    }

    fn list_finder_lives(&self) -> wechat_data::Result<Vec<wechat_data::FinderLive>> {
        match self {
            Self::Decrypted(store) => store.list_finder_lives(),
            Self::Realtime(store) => store.list_finder_lives(),
        }
    }

    fn list_mini_programs(&self) -> wechat_data::Result<Vec<wechat_data::MiniProgram>> {
        match self {
            Self::Decrypted(store) => store.list_mini_programs(),
            Self::Realtime(store) => store.list_mini_programs(),
        }
    }

    fn list_payments(&self) -> wechat_data::Result<Vec<wechat_data::PaymentItem>> {
        match self {
            Self::Decrypted(store) => store.list_payments(),
            Self::Realtime(store) => store.list_payments(),
        }
    }
}

fn realtime_store_from_env() -> Option<wechat_data::Result<(RealtimeAccountStore, RealtimeParams)>>
{
    let root = std::env::var("WECHAT_REALTIME_ROOT").ok()?;
    let broker = std::env::var("WCE_SELF_BROKER").ok()?;
    if root.trim().is_empty() || broker.trim().is_empty() {
        return None;
    }
    let account_from_store = || {
        let store = std::env::var("WECHAT_KEY_STORE").ok()?;
        wechat_data::find_key_for_db_storage_with_account(Path::new(&store), Path::new(&root))
            .ok()
            .and_then(|(_, _, account)| account)
    };
    let (key, raw_key, account) = if let Some(value) = std::env::var("WCE_DB_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        (value.trim().to_string(), false, account_from_store())
    } else if let Ok(store) = std::env::var("WECHAT_KEY_STORE") {
        match wechat_data::find_key_for_db_storage_with_account(Path::new(&store), Path::new(&root))
        {
            Ok(triple) => triple,
            Err(error) => return Some(Err(error)),
        }
    } else if std::env::var("WCE_KEY_SCAN").ok().as_deref() == Some("1") {
        match wechat_data::recover_key_for_db_storage(Path::new(&root), None) {
            Ok(found) => (found.key_hex, found.raw_key, account_from_store()),
            Err(error) => return Some(Err(error)),
        }
    } else {
        return None;
    };
    let params = RealtimeParams {
        broker: PathBuf::from(&broker),
        root: PathBuf::from(&root),
        key: key.clone(),
        raw_key,
        account: account.clone(),
    };
    Some(
        RealtimeAccountStore::open_with_account(
            Path::new(&broker),
            Path::new(&root),
            &key,
            raw_key,
            account.as_deref(),
        )
        .map(|store| (store, params)),
    )
}

fn image_keys_for_store(store: &Store, key_store: Option<&Path>) -> Option<ImageKeys> {
    if let Ok(aes_key) = std::env::var("WCE_IMAGE_AES_KEY") {
        let aes_key = aes_key.trim().to_string();
        if !aes_key.is_empty() {
            return Some(ImageKeys {
                aes_key,
                xor_key: std::env::var("WCE_IMAGE_XOR_KEY")
                    .ok()
                    .and_then(|value| parse_xor_key(&value)),
            });
        }
    }
    let store_path = key_store
        .map(Path::to_path_buf)
        .or_else(|| std::env::var("WECHAT_KEY_STORE").ok().map(PathBuf::from))?;
    match store {
        Store::Realtime(realtime) => {
            find_image_keys_for_db_storage(&store_path, realtime.account_dir()).ok()?
        }
        Store::Decrypted(decrypted) => {
            let keys = KeyStore::load(&store_path).ok()?;
            keys.image_keys_for_account(decrypted.account())
        }
    }
}

fn parse_xor_key(value: &str) -> Option<u8> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u8::from_str_radix(hex, 16).ok();
    }
    if value.chars().any(|ch| ch.is_ascii_alphabetic()) {
        u8::from_str_radix(value, 16).ok()
    } else {
        value.parse().ok()
    }
}

pub struct WeChatApp {
    view: View,
    accounts: Vec<AccountInfo>,
    store: Option<Store>,
    sessions: Vec<Session>,
    session_cache: HashMap<String, CachedSession>,
    avatars: HashMap<String, Arc<Image>>,
    image_keys: Option<ImageKeys>,
    message_images: HashMap<String, Option<Arc<Image>>>,
    message_media_pending: HashSet<String>,
    contacts: Vec<ContactItem>,
    contact_query: String,
    selected_contact: Option<String>,
    contact_input: Option<Entity<InputState>>,
    _contact_subscription: Option<Subscription>,
    contacts_scroll: gpui_kit::base::VirtualListScrollHandle,
    sns_posts: Vec<SnsPost>,
    sns_before: Option<i64>,
    sns_author: Option<String>,
    sns_query: String,
    sns_input: Option<Entity<InputState>>,
    _sns_subscription: Option<Subscription>,
    sns_scroll: ScrollHandle,
    sns_author_scroll: ScrollHandle,
    sns_images: HashMap<String, Option<Arc<Image>>>,
    remote_images: HashMap<String, Option<Arc<Image>>>,
    remote_pending: HashSet<String>,
    remote_tasks: Vec<Task<()>>,
    remote_cdn_enabled: bool,
    smooth_target: Option<(&'static str, SmoothTarget)>,
    smooth_pending: gpui_kit::Pixels,
    smooth_animating: bool,
    smooth_scroll_task: Option<Task<()>>,
    finder_scroll: ScrollHandle,
    mini_scroll: ScrollHandle,
    payments_scroll: ScrollHandle,
    search_scroll: ScrollHandle,
    sns_loaded: bool,
    finder: Vec<FinderLive>,
    finder_loaded: bool,
    mini_programs: Vec<MiniProgram>,
    mini_loaded: bool,
    payments: Vec<PaymentItem>,
    payments_loaded: bool,
    contacts_loaded: bool,
    realtime_params: Option<RealtimeParams>,
    search_task: Option<Task<()>>,
    search_seq: u64,
    messages_list: ListState,
    message_items: Vec<MessageListItem>,
    message_list_handler_installed: bool,
    pending_older_load: bool,
    pending_scroll_bottom: bool,
    loading_older: bool,
    selected: Option<String>,
    messages: Vec<Message>,
    older_cursor: Option<MessageCursor>,
    has_more_older: bool,
    sessions_scroll: gpui_kit::base::VirtualListScrollHandle,
    search_input: Option<Entity<InputState>>,
    _search_subscription: Option<Subscription>,
    query: String,
    hits: Vec<SearchHit>,
    search_note: String,
    pending_search: Option<String>,
    index_building: bool,
    _index_task: Option<Task<()>>,
    status: String,
    settings: AppSettings,
    settings_draft: AppSettings,
    settings_open: bool,
    setup_mode: bool,
    init_busy: bool,
    monitor_started: bool,
}

impl WeChatApp {
    pub fn new(account_path: Option<PathBuf>) -> Self {
        let mut app = Self::blank();
        app.settings = load_settings().unwrap_or_default();
        if let Some(path) = account_path {
            app.settings.account_path = Some(path);
        }
        app.settings_draft = app.settings.clone();
        app.remote_cdn_enabled = app.settings.remote_cdn && remote_cdn_allowed();
        set_dark_mode(app.settings.theme == "dark");
        set_minimize_to_tray(app.settings.minimize_to_tray);
        if app.settings.account_path.is_some() {
            let settings = app.settings.clone();
            app.apply_settings(settings, false);
            if app.store.is_some() || !app.accounts.is_empty() {
                return app;
            }
        }
        if let Some(result) = realtime_store_from_env() {
            match result {
                Ok((store, params)) => {
                    app.realtime_params = Some(params);
                    app.adopt_store(Store::Realtime(Box::new(store)));
                    app.settings.account_path =
                        std::env::var_os("WECHAT_REALTIME_ROOT").map(PathBuf::from);
                    app.settings.media_root = std::env::var_os("WCE_MEDIA_ROOT").map(PathBuf::from);
                    app.settings.key_store =
                        std::env::var_os("WECHAT_KEY_STORE").map(PathBuf::from);
                    app.settings.broker = std::env::var_os("WCE_SELF_BROKER").map(PathBuf::from);
                    app.settings_draft = app.settings.clone();
                }
                Err(error) => {
                    app.status = format!("实时模式打开失败: {error}");
                }
            }
            return app;
        }
        app.settings_open = true;
        app.setup_mode = true;
        if app.status.is_empty() {
            app.status = "请先选择微信数据目录".to_string();
        }
        app
    }

    fn apply_settings(&mut self, mut settings: AppSettings, persist: bool) {
        if settings.media_root.is_none() {
            settings.media_root = settings.storage_dir.clone();
        }
        if settings.key_store.is_none()
            && let Some(storage) = settings.storage_dir.as_ref()
        {
            let candidate = storage.join("account_keys.json");
            if candidate.is_file() {
                settings.key_store = Some(candidate);
            }
        }
        if let Some(storage) = settings.storage_dir.as_deref()
            && let Some(broker) = find_broker(storage, &settings)
        {
            settings.broker = Some(broker);
        }
        self.settings = settings.clone();
        self.settings_draft = settings.clone();
        self.remote_cdn_enabled = settings.remote_cdn && remote_cdn_allowed();
        set_dark_mode(settings.theme == "dark");
        set_minimize_to_tray(settings.minimize_to_tray);
        set_media_root(settings.media_root.clone());
        if persist && let Err(error) = save_settings(&settings) {
            self.status = format!("保存设置失败: {error}");
            return;
        }
        let Some(path) = settings.account_path.clone() else {
            self.status = "请选择数据目录".to_string();
            return;
        };

        if has_database_file(&path)
            && let Ok(store) = AccountStore::open(&path)
            && self.adopt_store(Store::Decrypted(Box::new(store)))
        {
            self.settings_open = false;
            self.setup_mode = false;
            return;
        }
        match discover_accounts(&path) {
            Ok(accounts) if accounts.len() == 1 => {
                if let Ok(store) = AccountStore::open(Path::new(&accounts[0].account_dir))
                    && self.adopt_store(Store::Decrypted(Box::new(store)))
                {
                    self.settings_open = false;
                    self.setup_mode = false;
                    return;
                }
                self.accounts = accounts;
                self.status = "打开账号失败".to_string();
                return;
            }
            Ok(accounts) if !accounts.is_empty() => {
                self.status = format!("发现 {} 个账号，请选择", accounts.len());
                self.accounts = accounts;
                return;
            }
            _ => {}
        }

        let db_storage = locate_db_storage(&path);
        let (Some(broker), Some(key_store)) =
            (settings.broker.as_ref(), settings.key_store.as_ref())
        else {
            self.status = "未找到可读账号；实时模式需要设置 Broker 与密钥文件".to_string();
            self.settings_open = true;
            return;
        };
        match find_key_for_db_storage_with_account(key_store, &db_storage) {
            Ok((key, raw_key, account)) => {
                match RealtimeAccountStore::open_with_account(
                    broker,
                    &db_storage,
                    &key,
                    raw_key,
                    account.as_deref(),
                ) {
                    Ok(store) => {
                        self.realtime_params = Some(RealtimeParams {
                            broker: broker.clone(),
                            root: db_storage,
                            key,
                            raw_key,
                            account,
                        });
                        if self.adopt_store(Store::Realtime(Box::new(store))) {
                            self.settings_open = false;
                            self.setup_mode = false;
                        }
                    }
                    Err(error) => {
                        self.status = format!("实时模式打开失败: {error}");
                        self.settings_open = true;
                    }
                }
            }
            Err(error) => {
                self.status = format!("读取密钥失败: {error}");
                self.settings_open = true;
            }
        }
    }

    pub fn open_realtime(broker_exe: &Path, root: &Path, key_hex: &str) -> Self {
        let mut app = Self::blank();
        app.realtime_params = Some(RealtimeParams {
            broker: broker_exe.to_path_buf(),
            root: root.to_path_buf(),
            key: key_hex.to_string(),
            raw_key: false,
            account: None,
        });
        match RealtimeAccountStore::open(broker_exe, root, key_hex, false) {
            Ok(store) => {
                app.adopt_store(Store::Realtime(Box::new(store)));
            }
            Err(error) => {
                app.status = format!("实时模式打开失败: {error}");
            }
        }
        app
    }

    fn start_background_monitor(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let mut was_hidden = false;
            let mut last_refresh = std::time::Instant::now();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let hidden = tray::is_hidden();
                let restored = was_hidden && !hidden;
                was_hidden = hidden;
                let refresh = hidden && last_refresh.elapsed() >= Duration::from_secs(60);
                if refresh {
                    last_refresh = std::time::Instant::now();
                }
                if !restored && !refresh {
                    continue;
                }
                let updated = this.update(cx, |this, cx| {
                    if restored {
                        cx.notify();
                    }
                    if !refresh {
                        return;
                    }
                    let Some(store) = this.store.as_ref() else {
                        return;
                    };
                    if !matches!(store, Store::Realtime(_)) {
                        return;
                    }
                    let Ok(sessions) = store.list_sessions(false, false) else {
                        return;
                    };
                    let changed = sessions.len() != this.sessions.len()
                        || sessions.iter().zip(this.sessions.iter()).any(|(new, old)| {
                            new.username != old.username
                                || new.last_timestamp != old.last_timestamp
                                || new.summary != old.summary
                        });
                    if changed {
                        this.sessions = sessions;
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        self.remote_tasks.push(task);
    }

    fn begin_initialize(&mut self, cx: &mut Context<Self>) {
        if self.init_busy {
            return;
        }
        self.init_busy = true;
        self.settings_open = true;
        self.status = "正在初始化：准备目录与密钥…".to_string();
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.initialize_now(cx);
                cx.notify();
            });
        });
        self.remote_tasks.push(task);
    }

    fn initialize_now(&mut self, _cx: &mut Context<Self>) {
        let mut settings = self.settings_draft.clone();
        let Some(wechat) = settings.account_path.clone() else {
            self.status = "请选择微信数据目录".to_string();
            self.init_busy = false;
            return;
        };
        let Some(storage) = settings.storage_dir.clone() else {
            self.status = "请选择存储目录".to_string();
            self.init_busy = false;
            return;
        };
        if let Err(error) = std::fs::create_dir_all(&storage) {
            self.status = format!("创建存储目录失败: {error}");
            self.init_busy = false;
            return;
        }
        settings.media_root = Some(storage.clone());
        let db_storage = locate_db_storage(&wechat);
        let key_path = storage.join("account_keys.json");
        if !key_path.exists()
            && let Some(found) = find_existing_key_store(&storage, &wechat, &settings)
            && found != key_path
            && let Err(error) = std::fs::copy(&found, &key_path)
        {
            self.status = format!("复制密钥文件失败: {error}");
            self.init_busy = false;
            return;
        }
        let key_valid = key_path.is_file()
            && find_key_for_db_storage_with_account(&key_path, &db_storage).is_ok();
        if !key_valid {
            self.status = "正在自动获取数据库/图片密钥…".to_string();
            match recover_key_for_db_storage(&db_storage, Some(&wechat)) {
                Ok(key) => {
                    if let Err(error) = write_recovered_key_store(&key_path, &db_storage, &key) {
                        self.status = format!("保存密钥失败: {error}");
                        self.init_busy = false;
                        return;
                    }
                }
                Err(error) => {
                    self.status =
                        format!("自动获取密钥失败: {error}。请先登录微信并保持运行后重试。");
                    self.init_busy = false;
                    return;
                }
            }
        }
        settings.key_store = Some(key_path);
        let Some(broker) = find_broker(&storage, &settings) else {
            self.status = "未找到 wce-self-broker.exe；请与 wechat-ui.exe 放在同一目录，或先执行 cargo build --workspace。".to_string();
            self.init_busy = false;
            return;
        };
        settings.broker = Some(broker);
        self.settings_draft = settings.clone();
        self.apply_settings(settings, true);
        self.init_busy = false;
    }

    fn blank() -> Self {
        Self {
            view: View::Chat,
            accounts: Vec::new(),
            store: None,
            sessions: Vec::new(),
            session_cache: HashMap::new(),
            avatars: HashMap::new(),
            image_keys: None,
            message_images: HashMap::new(),
            message_media_pending: HashSet::new(),
            contacts: Vec::new(),
            contact_query: String::new(),
            selected_contact: None,
            contact_input: None,
            _contact_subscription: None,
            contacts_scroll: gpui_kit::base::VirtualListScrollHandle::new(),
            sns_posts: Vec::new(),
            sns_before: None,
            sns_author: None,
            sns_query: String::new(),
            sns_input: None,
            _sns_subscription: None,
            sns_scroll: ScrollHandle::new(),
            sns_author_scroll: ScrollHandle::new(),
            sns_images: HashMap::new(),
            remote_images: HashMap::new(),
            remote_pending: HashSet::new(),
            remote_tasks: Vec::new(),
            remote_cdn_enabled: std::env::var("WCE_REMOTE_CDN").ok().as_deref() != Some("0"),
            smooth_target: None,
            smooth_pending: px(0.),
            smooth_animating: false,
            smooth_scroll_task: None,
            finder_scroll: ScrollHandle::new(),
            mini_scroll: ScrollHandle::new(),
            payments_scroll: ScrollHandle::new(),
            search_scroll: ScrollHandle::new(),
            sns_loaded: false,
            finder: Vec::new(),
            finder_loaded: false,
            mini_programs: Vec::new(),
            mini_loaded: false,
            payments: Vec::new(),
            payments_loaded: false,
            contacts_loaded: false,
            realtime_params: None,
            search_task: None,
            search_seq: 0,
            messages_list: ListState::new(0, ListAlignment::Bottom, px(800.)),
            message_items: Vec::new(),
            message_list_handler_installed: false,
            pending_older_load: false,
            pending_scroll_bottom: false,
            loading_older: false,
            selected: None,
            messages: Vec::new(),
            older_cursor: None,
            has_more_older: false,
            sessions_scroll: gpui_kit::base::VirtualListScrollHandle::new(),
            search_input: None,
            _search_subscription: None,
            query: String::new(),
            hits: Vec::new(),
            search_note: String::new(),
            pending_search: None,
            index_building: false,
            _index_task: None,
            status: String::new(),
            settings: AppSettings::default(),
            settings_draft: AppSettings::default(),
            settings_open: false,
            setup_mode: false,
            init_busy: false,
            monitor_started: false,
        }
    }

    fn adopt_store(&mut self, store: Store) -> bool {
        match store.list_sessions(false, false) {
            Ok(sessions) => {
                self.status.clear();
                self.view = View::Chat;
                self.image_keys = image_keys_for_store(&store, self.settings.key_store.as_deref());
                self.message_images.clear();
                self.message_media_pending.clear();
                self.session_cache.clear();
                self.contacts.clear();
                self.selected_contact = None;
                self.contacts_loaded = false;
                self.sns_posts.clear();
                self.sns_before = None;
                self.sns_author = None;
                self.sns_images.clear();
                self.sns_loaded = false;
                self.finder.clear();
                self.finder_loaded = false;
                self.mini_programs.clear();
                self.mini_loaded = false;
                self.payments.clear();
                self.payments_loaded = false;
                self.store = Some(store);
                self.sessions = sessions;
                self.selected = None;
                self.messages.clear();
                self.message_items.clear();
                self.messages_list.reset(0);
                self.pending_older_load = false;
                self.older_cursor = None;
                self.has_more_older = false;
                self.load_session_avatars();
                true
            }
            Err(error) => {
                self.status = format!("读取会话失败: {error}");
                false
            }
        }
    }

    fn load_avatars_for(&mut self, names: &[String]) {
        let mut missing: Vec<String> = names
            .iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty() && !self.avatars.contains_key(name))
            .collect();
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            return;
        }
        let loaded = self
            .store
            .as_ref()
            .and_then(|store| store.load_avatars(&missing).ok())
            .unwrap_or_default();
        for (username, avatar) in loaded {
            if let Some(image) = decode_avatar_image(&avatar) {
                self.avatars.insert(username, image);
            }
        }
    }

    fn load_contacts(&mut self) {
        if self.contacts_loaded {
            return;
        }
        self.contacts_loaded = true;
        let Some(store) = &self.store else {
            return;
        };
        match store.list_contact_directory("", 5000) {
            Ok(items) => {
                let names: Vec<String> = items.iter().map(|item| item.username.clone()).collect();
                self.status = format!("{} · {} 个联系人", store.account(), items.len());
                self.contacts = items;
                self.selected_contact = self.contacts.first().map(|item| item.username.clone());
                self.load_avatars_for(&names);
            }
            Err(error) => self.status = format!("读取联系人失败: {error}"),
        }
    }

    fn load_sns_page(&mut self, reset: bool, cx: &mut Context<Self>) {
        let Some(store) = &self.store else {
            return;
        };
        if reset {
            self.sns_posts.clear();
            self.sns_before = None;
            self.sns_images.clear();
        }
        let result = store.list_sns_posts(self.sns_before, 20);
        match result {
            Ok(posts) => {
                if posts.is_empty() {
                    self.status = "没有更多朋友圈".to_string();
                } else {
                    self.sns_before = posts.last().map(|post| post.tid_i64);
                    self.sns_posts.extend(posts);
                }
                self.sns_loaded = true;
                self.load_sns_media(cx);
                let mut names: Vec<String> = Vec::new();
                for post in &self.sns_posts {
                    names.push(post.username.clone());
                    names.extend(post.comments.iter().map(|comment| comment.username.clone()));
                    names.extend(post.likes.iter().map(|like| like.username.clone()));
                }
                self.load_avatars_for(&names);
            }
            Err(error) => self.status = format!("读取朋友圈失败: {error}"),
        }
    }

    fn load_sns_media(&mut self, cx: &mut Context<Self>) {
        let (Some(root), Some(keys)) = (
            self.store
                .as_ref()
                .and_then(|store| store.account_root())
                .map(Path::to_path_buf),
            self.image_keys.clone(),
        ) else {
            return;
        };
        let mut remote_candidates: Vec<(String, String, Option<String>, Option<String>)> =
            Vec::new();
        for post in &self.sns_posts {
            for media in &post.media {
                if media.id.is_empty() {
                    continue;
                }
                let key = format!("{}:{}", post.tid, media.id);
                if self.sns_images.contains_key(&key) || self.remote_pending.contains(&key) {
                    continue;
                }
                let use_thumb = !media.thumb_url.trim().is_empty();
                let media_type = if media.kind == "video" { 4 } else { 2 };
                let image = load_sns_image(
                    &root,
                    post.tid_i64,
                    &media.id,
                    media_type,
                    &media.md5,
                    &keys,
                )
                .and_then(|media| decode_image_bytes(&media.data, &media.media_type));
                if image.is_some() {
                    self.sns_images.insert(key, image);
                    continue;
                }
                self.sns_images.insert(key.clone(), None);
                let (url, token, decrypt_key) = if use_thumb {
                    (
                        media.thumb_url.as_str(),
                        media.thumb_token.as_str(),
                        media.thumb_key.as_str(),
                    )
                } else {
                    (media.url.as_str(), media.token.as_str(), media.key.as_str())
                };
                if url.starts_with("http")
                    && normalize_tencent_url(url, Some(token), false, false).is_some()
                {
                    remote_candidates.push((
                        key,
                        url.to_string(),
                        (!token.is_empty()).then(|| token.to_string()),
                        (!decrypt_key.is_empty()).then(|| decrypt_key.to_string()),
                    ));
                }
            }
        }
        for (key, url, token, decrypt_key) in remote_candidates {
            self.request_remote_image(key, url, token, decrypt_key, cx);
        }
    }

    fn smooth_viewport(
        &self,
        id: &'static str,
        target: SmoothTarget,
        viewport: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity();
        SmoothWheel {
            child: div().size_full().child(viewport),
            on_wheel: Rc::new(move |event, _, cx| {
                let target = target.clone();
                view.update(cx, |this, cx| this.smooth_wheel(id, target, event, cx));
            }),
        }
        .into_any_element()
    }

    fn smooth_wheel(
        &mut self,
        id: &'static str,
        target: SmoothTarget,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(18.)).y;
        let amount = -delta;
        if amount.abs() < px(0.01) {
            return;
        }
        let same = self
            .smooth_target
            .as_ref()
            .map(|(current, _)| *current == id)
            .unwrap_or(false);
        if !same {
            self.smooth_target = Some((id, target));
            self.smooth_pending = px(0.);
        }
        self.smooth_pending += amount;
        self.start_smooth_scroll(cx);
    }

    fn start_smooth_scroll(&mut self, cx: &mut Context<Self>) {
        if self.smooth_animating {
            return;
        }
        self.smooth_animating = true;
        let task = cx.spawn(async move |this, cx| {
            loop {
                let done = this
                    .update(cx, |this, cx| {
                        let Some((_, target)) = this.smooth_target.clone() else {
                            return true;
                        };
                        if this.smooth_pending.abs() < px(0.5) {
                            this.smooth_pending = px(0.);
                            this.smooth_target = None;
                            return true;
                        }
                        let step = this.smooth_pending * 0.3;
                        if target.scroll_down(step) {
                            this.smooth_pending -= step;
                            cx.notify();
                            false
                        } else {
                            this.smooth_pending = px(0.);
                            this.smooth_target = None;
                            true
                        }
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
            }
            let _ = this.update(cx, |this, cx| {
                this.smooth_animating = false;

                if this.smooth_pending.abs() >= px(0.5) && this.smooth_target.is_some() {
                    this.start_smooth_scroll(cx);
                }
            });
        });
        self.smooth_scroll_task = Some(task);
    }

    fn scroll_rail<H>(id: &'static str, handle: &H) -> AnyElement
    where
        H: ScrollbarHandle + Clone + 'static,
    {
        div()
            .relative()
            .flex_none()
            .w(Scrollbar::width())
            .min_h_0()
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::vertical(handle)
                        .id(id)
                        .viewport_from_layout()
                        .mode(ScrollbarMode::Always),
                ),
            )
            .into_any_element()
    }

    fn request_remote_image(
        &mut self,
        key: String,
        url: String,
        token: Option<String>,
        decrypt_key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !self.remote_cdn_enabled
            || self.remote_pending.contains(&key)
            || self.remote_images.contains_key(&key)
            || normalize_tencent_url(&url, token.as_deref(), false, false).is_none()
        {
            return;
        }
        self.remote_pending.insert(key.clone());
        let task_url = url.clone();
        let task_token = token.clone();
        let task_key = decrypt_key.clone();
        let background = cx.background_executor().spawn(async move {
            fetch_tencent_media(
                &task_url,
                task_token.as_deref(),
                task_key.as_deref(),
                false,
                30 * 1024 * 1024,
            )
        });
        let task = cx.spawn(async move |this, cx| {
            let result = background.await;
            let _ = this.update(cx, |this, cx| {
                this.remote_pending.remove(&key);
                let image = result
                    .ok()
                    .and_then(|media| decode_image_bytes(&media.data, &media.media_type));
                this.remote_images.insert(key, image);
                cx.notify();
            });
        });
        self.remote_tasks.push(task);
    }

    fn load_finder(&mut self) {
        if self.finder_loaded {
            return;
        }
        self.finder_loaded = true;
        let Some(store) = &self.store else {
            return;
        };
        match store.list_finder_lives() {
            Ok(items) => {
                self.status = format!("视频号 · {} 条直播记录", items.len());
                self.finder = items;
            }
            Err(error) => self.status = format!("读取视频号失败: {error}"),
        }
    }

    fn load_mini_programs(&mut self) {
        if self.mini_loaded {
            return;
        }
        self.mini_loaded = true;
        let Some(store) = &self.store else {
            return;
        };
        match store.list_mini_programs() {
            Ok(items) => {
                self.status = format!("小程序 · {} 个", items.len());
                self.mini_programs = items;
            }
            Err(error) => self.status = format!("读取小程序失败: {error}"),
        }
    }

    fn load_payments(&mut self) {
        if self.payments_loaded {
            return;
        }
        self.payments_loaded = true;
        let Some(store) = &self.store else {
            return;
        };
        match store.list_payments() {
            Ok(items) => {
                self.status = format!("转账/红包 · {} 条", items.len());
                self.payments = items;
            }
            Err(error) => self.status = format!("读取支付记录失败: {error}"),
        }
    }

    fn switch_view(&mut self, view: View, cx: &mut Context<Self>) {
        self.view = view;
        match view {
            View::Chat | View::Search => {}
            View::Contacts => self.load_contacts(),
            View::Moments => {
                if !self.sns_loaded {
                    self.load_sns_page(true, cx);
                }
            }
            View::Finder => self.load_finder(),
            View::MiniPrograms => self.load_mini_programs(),
            View::Payments => self.load_payments(),
        }
        cx.notify();
    }

    fn open_account(&mut self, account_dir: &Path) {
        match AccountStore::open(account_dir) {
            Ok(store) => {
                self.query.clear();
                self.hits.clear();
                self.search_note.clear();
                self.pending_search = None;
                let _ = self.adopt_store(Store::Decrypted(Box::new(store)));
            }
            Err(error) => self.status = format!("打开账号失败: {error}"),
        }
    }

    fn cache_current_session(&mut self, username: &str) {
        self.session_cache.insert(
            username.to_string(),
            CachedSession {
                messages: self.messages.clone(),
                older_cursor: self.older_cursor.clone(),
                has_more_older: self.has_more_older,
                loaded_at: std::time::Instant::now(),
            },
        );
        while self.session_cache.len() > 12 {
            let oldest = self
                .session_cache
                .iter()
                .min_by_key(|(_, cached)| cached.loaded_at)
                .map(|(username, _)| username.clone());
            match oldest {
                Some(username) => {
                    self.session_cache.remove(&username);
                }
                None => break,
            }
        }
    }

    fn select_session(&mut self, username: String) {
        let timing = std::env::var_os("WCE_TIMING").is_some();
        let started = std::time::Instant::now();
        self.selected = Some(username.clone());
        self.messages.clear();
        self.message_items.clear();
        self.messages_list.reset(0);
        self.pending_older_load = false;
        self.older_cursor = None;
        self.has_more_older = false;
        if let Some(cached) = self
            .session_cache
            .get(&username)
            .filter(|cached| cached.loaded_at.elapsed() < Duration::from_secs(60))
            .cloned()
        {
            self.messages = cached.messages;
            self.older_cursor = cached.older_cursor;
            self.has_more_older = cached.has_more_older;
            self.rebuild_message_items();
            self.messages_list.reset(self.message_items.len());
            self.pending_scroll_bottom = true;
            self.load_message_avatars();
            if timing {
                eprintln!(
                    "select_session {username}: cache hit total={:?}",
                    started.elapsed()
                );
            }
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        match store.load_messages(
            &username,
            &MessageQuery {
                limit: 40,
                before: None,
            },
        ) {
            Ok(page) => {
                let after_db = started.elapsed();
                self.older_cursor = page.next_before;
                self.has_more_older = page.has_more;
                self.messages = page.messages;
                self.rebuild_message_items();
                self.messages_list.reset(self.message_items.len());
                self.pending_scroll_bottom = true;
                let after_items = started.elapsed();
                self.load_message_avatars();
                let after_avatars = started.elapsed();
                self.cache_current_session(&username);
                if timing {
                    eprintln!(
                        "select_session {username}: db={after_db:?} items={:?} avatars={:?} total={:?}",
                        after_items - after_db,
                        after_avatars - after_items,
                        after_avatars
                    );
                }
            }
            Err(error) => self.status = format!("读取消息失败: {error}"),
        }
    }

    fn load_older(&mut self) {
        if self.loading_older || !self.has_more_older {
            return;
        }
        let (Some(username), Some(cursor)) = (self.selected.clone(), self.older_cursor.clone())
        else {
            return;
        };
        self.loading_older = true;
        let old_offset = self.messages_list.logical_scroll_top();
        let old_count = self.message_items.len();
        let result = self.store.as_ref().map(|store| {
            store.load_messages(
                &username,
                &MessageQuery {
                    limit: 40,
                    before: Some(cursor),
                },
            )
        });
        self.loading_older = false;
        match result {
            Some(Ok(page)) => {
                let mut merged = page.messages;
                merged.append(&mut self.messages);
                self.messages = merged;
                self.older_cursor = page.next_before;
                self.has_more_older = page.has_more;
                self.rebuild_message_items();
                let new_count = self.message_items.len();
                self.messages_list.reset(new_count);
                let added = new_count.saturating_sub(old_count);
                self.messages_list.scroll_to(ListOffset {
                    item_ix: (old_offset.item_ix + added).min(new_count),
                    offset_in_item: old_offset.offset_in_item,
                });
                self.load_message_avatars();
                self.cache_current_session(&username);
            }
            Some(Err(error)) => self.status = format!("加载更早消息失败: {error}"),
            None => {}
        }
    }

    fn load_message_avatars(&mut self) {
        let mut names: Vec<String> = self
            .messages
            .iter()
            .map(|message| message.sender_username.trim().to_string())
            .filter(|username| !username.is_empty())
            .collect();
        if self.messages.iter().any(|message| message.is_sent)
            && let Some(store) = &self.store
        {
            names.extend(store.self_names());
        }
        names.sort();
        names.dedup();
        names.retain(|username| !self.avatars.contains_key(username));
        if names.is_empty() {
            return;
        }
        let loaded = self
            .store
            .as_ref()
            .and_then(|store| store.load_avatars(&names).ok())
            .unwrap_or_default();
        for (username, avatar) in loaded {
            if let Some(image) = decode_avatar_image(&avatar) {
                self.avatars.insert(username, image);
            }
        }
    }

    fn rebuild_message_items(&mut self) {
        let mut items = Vec::new();
        if self.has_more_older {
            items.push(MessageListItem::OlderButton);
        }
        let mut last_day: Option<String> = None;
        for (index, message) in self.messages.iter().enumerate() {
            let day = day_key(message.create_time);
            if last_day.as_deref() != Some(day.as_str()) {
                items.push(MessageListItem::Day(message.create_time));
                last_day = Some(day);
            }
            items.push(MessageListItem::Message(index));
        }
        self.message_items = items;
    }

    #[cfg(test)]
    fn load_message_images(&mut self) {
        let (Some(store), Some(username)) = (self.store.as_ref(), self.selected.clone()) else {
            return;
        };
        let Some(root) = store.account_root().map(Path::to_path_buf) else {
            return;
        };
        let keys = self.image_keys.clone();
        let mut pending: Vec<(String, String, i64, i64)> = Vec::new();
        for message in &self.messages {
            if !matches!(message.kind, MessageKind::Image | MessageKind::Video) {
                continue;
            }
            let key = message_media_key(message);
            if self.message_images.contains_key(&key) {
                continue;
            }
            self.message_images.insert(key.clone(), None);
            let md5 = message
                .media_md5
                .clone()
                .or_else(|| extract_image_md5(&message.content))
                .unwrap_or_default();
            pending.push((key, md5, message.local_id, message.create_time));
        }
        for (key, md5, local_id, create_time) in pending {
            let media = match keys.as_ref() {
                Some(keys) => {
                    load_message_image(&root, &username, &md5, local_id, create_time, keys)
                }
                None => load_message_image_cached(&root, &username, &md5, local_id, create_time),
            };
            let image = media.and_then(|media| decode_image_bytes(&media.data, &media.media_type));
            self.message_images.insert(key, image);
        }
    }

    #[cfg(test)]
    fn load_message_emojis(&mut self, cx: &mut Context<Self>) {
        let messages = self.messages.clone();
        for message in &messages {
            self.ensure_emoji_image(message, cx);
        }
    }

    fn ensure_emoji_image(&mut self, message: &Message, cx: &mut Context<Self>) {
        let key = message_media_key(message);
        if self.message_images.contains_key(&key) {
            return;
        }
        let Some(reference) = parse_emoji_reference(&message.content) else {
            self.message_images.insert(key, None);
            return;
        };
        let local = self
            .store
            .as_ref()
            .and_then(Store::account_root)
            .map(Path::to_path_buf)
            .and_then(|root| match self.image_keys.as_ref() {
                Some(keys) => load_emoji_image(&root, &reference.md5, keys),
                None => find_resource_image(&root, &reference.md5),
            });
        if let Some(media) = local.as_ref() {
            cache_media_bytes(&reference.md5, &media.data, &media.media_type);
        }
        if let Some(image) =
            local.and_then(|media| decode_image_bytes(&media.data, &media.media_type))
        {
            self.message_images.insert(key, Some(image));
            return;
        }
        let (url, aes_key) = reference.best_url();
        let emoji_md5 = reference.md5.clone();
        let pending_key = format!("emoji:{key}");
        if self.remote_pending.contains(&pending_key) {
            return;
        }
        if self.remote_cdn_enabled && !url.is_empty() {
            self.remote_pending.insert(pending_key.clone());
            let keys = self.image_keys.clone();
            let url = url.to_string();
            let aes_key = aes_key.to_string();
            let background = cx.background_executor().spawn(async move {
                let result = fetch_emoticon_media(&url, &aes_key, keys.as_ref(), 30 * 1024 * 1024);
                if let Ok(media) = result.as_ref() {
                    cache_media_bytes(&emoji_md5, &media.data, &media.media_type);
                }
                result
            });
            let task = cx.spawn(async move |this, cx| {
                let result = background.await;
                let _ = this.update(cx, |this, cx| {
                    this.remote_pending.remove(&pending_key);
                    let image = result
                        .ok()
                        .and_then(|media| decode_image_bytes(&media.data, &media.media_type));
                    this.message_images.insert(key, image);
                    cx.notify();
                });
            });
            self.remote_tasks.push(task);
        } else {
            self.message_images.insert(key, None);
        }
    }

    fn ensure_message_image(&mut self, message: &Message, cx: &mut Context<Self>) {
        let key = message_media_key(message);
        if self.message_images.contains_key(&key) || self.message_media_pending.contains(&key) {
            return;
        }
        let Some(root) = self
            .store
            .as_ref()
            .and_then(Store::account_root)
            .map(Path::to_path_buf)
        else {
            self.message_images.insert(key, None);
            return;
        };
        let username = self.selected.clone().unwrap_or_default();
        let md5 = message
            .media_md5
            .clone()
            .or_else(|| extract_image_md5(&message.content))
            .unwrap_or_default();
        self.message_media_pending.insert(key.clone());
        let keys = self.image_keys.clone();
        let local_id = message.local_id;
        let create_time = message.create_time;
        let started = std::time::Instant::now();
        let timing = std::env::var_os("WCE_TIMING").is_some();
        let background = cx.background_executor().spawn(async move {
            let media = match keys {
                Some(keys) => {
                    load_message_image(&root, &username, &md5, local_id, create_time, &keys)
                }
                None => load_message_image_cached(&root, &username, &md5, local_id, create_time),
            };
            if let Some(media) = media.as_ref() {
                cache_media_bytes(&md5, &media.data, &media.media_type);
            }
            media.and_then(|media| decode_image_bytes(&media.data, &media.media_type))
        });
        let task = cx.spawn(async move |this, cx| {
            let image = background.await;
            let _ = this.update(cx, |this, cx| {
                this.message_media_pending.remove(&key);
                this.message_images.insert(key.clone(), image);
                if timing {
                    eprintln!("ensure_image {key}: {:?}", started.elapsed());
                }
                cx.notify();
            });
        });
        self.remote_tasks.push(task);
    }

    fn ensure_contact_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.contact_input.is_some() {
            return;
        }
        let state = cx.new(|cx| InputState::new(window, cx).placeholder("搜索联系人"));
        let subscription = cx.subscribe(&state, |this, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.contact_query = state.read(cx).value().to_string();
                cx.notify();
            }
        });
        self.contact_input = Some(state);
        self._contact_subscription = Some(subscription);
    }

    fn ensure_sns_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sns_input.is_some() {
            return;
        }
        let state = cx.new(|cx| InputState::new(window, cx).placeholder("搜索朋友圈"));
        let subscription = cx.subscribe(&state, |this, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.sns_query = state.read(cx).value().to_string();
                cx.notify();
            }
        });
        self.sns_input = Some(state);
        self._sns_subscription = Some(subscription);
    }

    fn filtered_contacts(&self) -> Vec<ContactItem> {
        self.contacts
            .iter()
            .filter(|contact| {
                wechat_data::contacts::contact_matches_keyword(contact, &self.contact_query)
            })
            .cloned()
            .collect()
    }

    fn sns_authors(&self) -> Vec<(String, String, usize)> {
        let mut authors: Vec<(String, String, usize)> = Vec::new();
        for post in &self.sns_posts {
            let name = if post.display_name.trim().is_empty() {
                post.username.clone()
            } else {
                post.display_name.clone()
            };
            if let Some(entry) = authors
                .iter_mut()
                .find(|(username, _, _)| username == &post.username)
            {
                entry.2 += 1;
            } else {
                authors.push((post.username.clone(), name, 1));
            }
        }
        authors.sort_by_key(|left| std::cmp::Reverse(left.2));
        authors
    }

    fn filtered_sns_posts(&self) -> Vec<&SnsPost> {
        let query = self.sns_query.trim().to_lowercase();
        self.sns_posts
            .iter()
            .filter(|post| {
                if let Some(author) = self.sns_author.as_deref()
                    && post.username != author
                {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                post.text.to_lowercase().contains(&query)
                    || post.display_name.to_lowercase().contains(&query)
                    || post.title.to_lowercase().contains(&query)
            })
            .collect()
    }

    fn ensure_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_input.is_some() {
            return;
        }
        let state = cx.new(|cx| InputState::new(window, cx).placeholder("搜索聊天记录"));
        let subscription = cx.subscribe(&state, |this, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let query = state.read(cx).value().to_string();
                this.run_search(&query, cx);
            }
        });
        self.search_input = Some(state);
        self._search_subscription = Some(subscription);
    }

    fn run_search(&mut self, raw_query: &str, cx: &mut Context<Self>) {
        let query = raw_query.trim().to_string();
        self.query = query.clone();
        self.hits.clear();
        self.search_note.clear();
        self.search_seq = self.search_seq.wrapping_add(1);
        let seq = self.search_seq;
        if query.is_empty() {
            cx.notify();
            return;
        }
        if self.store.is_none() {
            self.search_note = "未打开账号".to_string();
            cx.notify();
            return;
        }
        let account_dir = self
            .store
            .as_ref()
            .and_then(|store| store.account_dir())
            .map(Path::to_path_buf);
        let Some(account_dir) = account_dir else {
            let Some(params) = self.realtime_params.clone() else {
                self.search_note = "实时搜索参数缺失".to_string();
                cx.notify();
                return;
            };
            self.search_note = "正在搜索…".to_string();
            let task_query = query.clone();
            let background = cx.background_executor().spawn(async move {
                RealtimeAccountStore::open_with_account(
                    &params.broker,
                    &params.root,
                    &params.key,
                    params.raw_key,
                    params.account.as_deref(),
                )
                .and_then(|store| store.search_messages(&task_query, 50))
            });
            let task = cx.spawn(async move |this, cx| {
                let result = background.await;
                this.update(cx, |this, cx| {
                    if this.search_seq != seq {
                        return;
                    }
                    match result {
                        Ok(page) => {
                            this.hits = page.hits;
                            this.search_note = if this.hits.is_empty() {
                                format!("没有找到“{query}”")
                            } else if page.has_more {
                                format!("实时扫描到 {} 条（可能还有更多）", this.hits.len())
                            } else {
                                format!("实时扫描到 {} 条", this.hits.len())
                            };
                        }
                        Err(error) => this.search_note = format!("搜索失败: {error}"),
                    }
                    cx.notify();
                })
                .ok();
            });
            self.search_task = Some(task);
            cx.notify();
            return;
        };
        if !chat_search_index_path(&account_dir).exists() {
            self.pending_search = Some(query);
            self.search_note = "正在建立搜索索引…".to_string();
            self.start_index_build(cx);
            cx.notify();
            return;
        }
        let result = search_index(
            &account_dir,
            &SearchQuery {
                q: query.clone(),
                limit: 50,
                ..SearchQuery::default()
            },
        );
        match result {
            Ok(page) => {
                self.hits = page.hits;
                if self.hits.is_empty() {
                    self.search_note = format!("没有找到“{query}”");
                } else if page.has_more {
                    self.search_note = format!("显示前 {} 条结果", self.hits.len());
                }
            }
            Err(error) => self.search_note = format!("搜索失败: {error}"),
        }
        cx.notify();
    }

    fn start_index_build(&mut self, cx: &mut Context<Self>) {
        if self.index_building {
            return;
        }
        let Some(account_dir) = self
            .store
            .as_ref()
            .and_then(|store| store.account_dir())
            .map(Path::to_path_buf)
        else {
            return;
        };
        self.index_building = true;
        self.status = "正在建立搜索索引…".to_string();
        let background = cx
            .background_executor()
            .spawn(async move { build_search_index(&account_dir) });
        let task: Task<()> = cx.spawn(async move |this, cx| {
            let result = background.await;
            this.update(cx, |this, cx| {
                this.index_building = false;
                match result {
                    Ok(stats) => {
                        this.status = format!("索引就绪 · {} 条消息", stats.indexed_messages);
                        if let Some(query) = this.pending_search.take() {
                            this.run_search(&query, cx);
                            return;
                        }
                    }
                    Err(error) => {
                        this.pending_search = None;
                        this.search_note = format!("建立搜索索引失败: {error}");
                        this.status = this.search_note.clone();
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self._index_task = Some(task);
    }

    fn session_display_name(&self, username: &str) -> String {
        self.sessions
            .iter()
            .find(|session| session.username == username)
            .map(|session| {
                session_label(&session.username, &session.display_name, session.is_group)
            })
            .unwrap_or_else(|| username.to_string())
    }

    fn account_row(&self, account: &AccountInfo, cx: &mut Context<Self>) -> AnyElement {
        let account_dir = PathBuf::from(&account.account_dir);
        div()
            .id(ElementId::Name(
                format!("account:{}", account.account).into(),
            ))
            .role(gpui_kit::Role::ListItem)
            .aria_label(account.account.clone())
            .test_support()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_3()
            .cursor_pointer()
            .hover(|style| style.bg(ui_color(0xeaeaea)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_account(&account_dir);
                if let Some(state) = &this.search_input {
                    state.update(cx, |state, cx| state.set_value("", window, cx));
                }
                cx.notify();
            }))
            .child(div().text_sm().child(account.account.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(ui_color(0x6b7280))
                    .child(format!("{} 个消息库", account.message_shards.len())),
            )
            .into_any_element()
    }

    fn load_session_avatars(&mut self) {
        self.avatars.clear();
        let mut names: Vec<String> = self
            .sessions
            .iter()
            .map(|session| session.username.clone())
            .collect();
        if let Some(store) = &self.store {
            names.extend(store.self_names());
        }
        let loaded = self
            .store
            .as_ref()
            .and_then(|store| store.load_avatars(&names).ok())
            .unwrap_or_default();
        for (username, avatar) in loaded {
            if let Some(image) = decode_avatar_image(&avatar) {
                self.avatars.insert(username, image);
            }
        }
    }

    fn session_row(&self, session: &Session, cx: &mut Context<Self>) -> AnyElement {
        let active = self.selected.as_deref() == Some(session.username.as_str());
        let username = session.username.clone();
        let badge = if session.is_group {
            "群"
        } else if session.is_official {
            "号"
        } else {
            "友"
        };
        let display_name =
            session_label(&session.username, &session.display_name, session.is_group);
        div()
            .id(ElementId::Name(
                format!("session:{}", session.username).into(),
            ))
            .role(gpui_kit::Role::ListItem)
            .aria_label(display_name.clone())
            .test_support()
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .h(px(56.))
            .px_3()
            .cursor_pointer()
            .when(active, |this| this.bg(ui_color(0xdedede)))
            .hover(|style| style.bg(ui_color(0xeaeaea)))
            .on_click({
                let username = username.clone();
                cx.listener(move |this, _, _, cx| {
                    this.select_session(username.clone());
                    cx.notify();
                })
            })
            .child(match self.avatars.get(&session.username) {
                Some(image) => img(image.clone())
                    .flex_none()
                    .w(px(34.))
                    .h(px(34.))
                    .rounded_md()
                    .object_fit(ObjectFit::Cover)
                    .into_any_element(),
                None => div()
                    .flex_none()
                    .w(px(34.))
                    .h(px(34.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if session.is_group {
                        ui_color(0x2f6f4e)
                    } else if session.is_official {
                        ui_color(0x5a4a8a)
                    } else {
                        ui_color(0x2c5a7a)
                    })
                    .text_sm()
                    .child(badge)
                    .into_any_element(),
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_between()
                            .gap_2()
                            .child(div().flex_1().min_w(px(0.)).truncate().child(display_name))
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(ui_color(0x6b7280))
                                    .child(session.last_time_text.clone()),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_xs()
                            .text_color(ui_color(0x6b7280))
                            .child(rich_text_preview(
                                &session.summary.replace('\n', " "),
                                &format!("session:{username}"),
                                12.,
                                0x6b7280,
                            )),
                    ),
            )
            .into_any_element()
    }

    fn hit_row(&self, hit: &SearchHit, cx: &mut Context<Self>) -> AnyElement {
        let username = hit.username.clone();
        let name = self.session_display_name(&hit.username);
        let text = hit_text(hit);
        let label = if text.is_empty() {
            name.clone()
        } else {
            format!("{name}: {text}")
        };
        div()
            .id(ElementId::Name(
                format!("search:{}:{}:{}", hit.username, hit.db_stem, hit.local_id).into(),
            ))
            .role(gpui_kit::Role::ListItem)
            .aria_label(label)
            .test_support()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_2()
            .cursor_pointer()
            .hover(|style| style.bg(ui_color(0xeaeaea)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.view = View::Chat;
                this.select_session(username.clone());
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .child(div().truncate().text_sm().child(name))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(ui_color(0x6b7280))
                            .child(format_clock(hit.create_time)),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(ui_color(0x6b7280))
                    .child(rich_text_preview(
                        &text,
                        &format!("search:{}:{}:{}", hit.username, hit.db_stem, hit.local_id),
                        14.,
                        0x6b7280,
                    )),
            )
            .into_any_element()
    }

    fn search_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut items: Vec<AnyElement> = Vec::new();
        if !self.search_note.is_empty() {
            items.push(
                div()
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(ui_color(0x6b7280))
                    .child(self.search_note.clone())
                    .into_any_element(),
            );
        }
        for hit in &self.hits {
            items.push(self.hit_row(hit, cx));
        }
        div().flex().flex_col().children(items).into_any_element()
    }

    fn day_separator(&self, create_time: i64) -> AnyElement {
        let key = day_key(create_time);
        let label = format_day_label(create_time);
        div()
            .id(ElementId::Name(format!("day:{key}").into()))
            .aria_label(label.clone())
            .test_support()
            .w_full()
            .flex()
            .justify_center()
            .py_2()
            .child(
                div()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .bg(ui_color(0xe5e5e5))
                    .text_xs()
                    .text_color(ui_color(0x6b7280))
                    .child(label),
            )
            .into_any_element()
    }

    fn older_button(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("load-older")
            .aria_label("加载更早的消息")
            .test_support()
            .w_full()
            .flex()
            .justify_center()
            .py_2()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.load_older();
                cx.notify();
            }))
            .child(
                div()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .bg(ui_color(0xe5e5e5))
                    .text_xs()
                    .text_color(ui_color(0x606060))
                    .hover(|style| style.bg(ui_color(0xd5d5d5)))
                    .child("加载更早的消息"),
            )
            .into_any_element()
    }

    fn avatar_chip(&self, username: &str, display: &str, size: f32) -> AnyElement {
        match self.avatars.get(username) {
            Some(image) => img(image.clone())
                .flex_none()
                .w(px(size))
                .h(px(size))
                .rounded_md()
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            None => div()
                .flex_none()
                .w(px(size))
                .h(px(size))
                .rounded_md()
                .flex()
                .items_center()
                .justify_center()
                .text_color(ui_color(0xffffff))
                .text_sm()
                .bg(ui_color(avatar_color(username)))
                .child(initial_of(display))
                .into_any_element(),
        }
    }

    fn message_row(&mut self, message: &Message, cx: &mut Context<Self>) -> AnyElement {
        if matches!(message.body, Some(MessageBody::System { .. }))
            || matches!(message.kind, MessageKind::System | MessageKind::Pat)
        {
            return self.system_row(message, cx);
        }

        let sender_key = if message.sender_username.trim().is_empty() {
            self.store
                .as_ref()
                .map(Store::account)
                .unwrap_or_default()
                .to_string()
        } else {
            message.sender_username.clone()
        };
        let sender = if message.sender_display_name.trim().is_empty() {
            if message.is_sent {
                "我".to_string()
            } else {
                sender_key.clone()
            }
        } else {
            message.sender_display_name.clone()
        };
        let is_group = self
            .selected
            .as_deref()
            .is_some_and(|username| username.ends_with("@chatroom"));

        let mut column = div()
            .flex()
            .flex_col()
            .gap_1()
            .min_w(px(0.))
            .when(message.is_sent, |this| this.items_end())
            .when(!message.is_sent, |this| this.items_start());
        if is_group && !message.is_sent && !sender.trim().is_empty() {
            column = column.child(
                div()
                    .text_xs()
                    .text_color(ui_color(0x8a8a8a))
                    .child(sender.clone()),
            );
        }
        column = column.child(self.message_content_view(message, cx));

        let group_id = format!("msg-group:{}:{}", message.shard, message.local_id);
        let copy_text = message_copy_text(message);
        let avatar = self.avatar_chip(&sender_key, &sender, 34.);
        let content_row = if message.is_sent {
            div()
                .flex()
                .w_full()
                .items_start()
                .justify_end()
                .gap_2()
                .child(column)
                .child(avatar)
        } else {
            div()
                .flex()
                .w_full()
                .items_start()
                .gap_2()
                .child(avatar)
                .child(column)
        };
        div()
            .id(ElementId::Name(
                format!("message:{}:{}", message.shard, message.local_id).into(),
            ))
            .aria_label(message_aria(message))
            .test_support()
            .group(group_id)
            .w_full()
            .py(px(5.))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    this.status = "已复制消息".to_string();
                    cx.notify();
                }),
            )
            .child(content_row)
            .into_any_element()
    }

    fn system_row(&self, message: &Message, cx: &mut Context<Self>) -> AnyElement {
        let text = match &message.body {
            Some(MessageBody::System { content }) => content.clone(),
            _ => sanitize_text(&message.content),
        };
        let copy_text = text.clone();
        div()
            .id(ElementId::Name(
                format!("message:{}:{}", message.shard, message.local_id).into(),
            ))
            .aria_label(message_aria(message))
            .test_support()
            .w_full()
            .flex()
            .justify_center()
            .py_1()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    this.status = "已复制消息".to_string();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .bg(ui_color(0xe5e5e5))
                    .text_xs()
                    .text_color(ui_color(0x7a7a7a))
                    .whitespace_normal()
                    .child(rich_text(
                        &text,
                        &format!("system:{}:{}", message.shard, message.local_id),
                        12.,
                        0x7a7a7a,
                    )),
            )
            .into_any_element()
    }

    fn text_bubble(&self, sent: bool, content: AnyElement) -> AnyElement {
        div()
            .max_w(px(420.))
            .rounded_md()
            .px_3()
            .py_2()
            .bg(if sent {
                ui_color(0x95ec69)
            } else {
                ui_color(0xffffff)
            })
            .text_color(ui_color(0x1f1f1f))
            .whitespace_normal()
            .child(content)
            .into_any_element()
    }

    fn voip_message_view(
        &self,
        sent: bool,
        card_id: &str,
        content: &str,
        voip_type: &str,
    ) -> AnyElement {
        let label = match voip_type {
            "video" => "视频通话",
            "audio" => "语音通话",
            _ => "通话",
        };
        let accent = if voip_type == "video" {
            0x2f80ed
        } else {
            0x07c160
        };
        let icon_bg = if sent { 0xd7f3c3 } else { 0xe9f6ee };
        let sub_color = if sent { 0x5f6b57 } else { 0x8a8a8a };
        div()
            .id(ElementId::Name(format!("voip:{card_id}").into()))
            .max_w(px(280.))
            .rounded_md()
            .px_3()
            .py_2()
            .bg(if sent {
                ui_color(0x95ec69)
            } else {
                ui_color(0xffffff)
            })
            .text_color(ui_color(0x1f1f1f))
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(px(28.))
                    .h(px(28.))
                    .flex_none()
                    .rounded_full()
                    .bg(ui_color(icon_bg))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .w(px(9.))
                            .h(px(14.))
                            .rounded_sm()
                            .border_1()
                            .border_color(ui_color(accent)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().child(content.to_string()))
                    .child(div().text_xs().text_color(ui_color(sub_color)).child(label)),
            )
            .into_any_element()
    }

    fn message_content_view(&mut self, message: &Message, cx: &mut Context<Self>) -> AnyElement {
        let sent = message.is_sent;
        let card_id = format!("{}:{}", message.shard, message.local_id);
        match &message.body {
            Some(MessageBody::Text { content }) | Some(MessageBody::System { content }) => self
                .text_bubble(
                    sent,
                    rich_text(content, &format!("body:{card_id}"), 14., 0x1f1f1f),
                ),
            Some(MessageBody::Link {
                content,
                title,
                url,
                from,
                link_type,
                thumb_url,
                ..
            }) => match link_type.as_str() {
                "finder" => {
                    self.finder_message_card(&card_id, title, content, from, url, thumb_url, cx)
                }
                "mini_program" => {
                    self.mini_program_card(&card_id, title, content, from, url, thumb_url, cx)
                }
                _ => self.link_card_view(&card_id, title, content, url, from, thumb_url, cx),
            },
            Some(MessageBody::ChatHistory { content, title, .. }) => {
                self.chat_history_card(title, content)
            }
            Some(MessageBody::File {
                content,
                title,
                size,
                ..
            }) => {
                let heading = if title.trim().is_empty() {
                    content
                } else {
                    title
                };
                self.file_message_card(heading, size)
            }
            Some(MessageBody::Quote {
                content,
                quote_title,
                quote_content,
                ..
            }) => self.quote_view(sent, &card_id, content, quote_title, quote_content),
            Some(MessageBody::Transfer {
                title,
                amount,
                pay_sub_type,
                receive_status,
                sender_title,
                ..
            }) => self.transfer_message_card(
                title,
                amount,
                pay_sub_type,
                receive_status,
                sender_title,
            ),
            Some(MessageBody::RedPacket { title, .. }) => self.red_packet_message_card(title),
            Some(MessageBody::Location {
                location_poiname,
                location_label,
                ..
            }) => {
                let title = if location_poiname.trim().is_empty() {
                    location_label
                } else {
                    location_poiname
                };
                self.info_card("位置", title, location_label, 0x4a90c4)
            }
            Some(MessageBody::Voip { content, voip_type }) => {
                self.voip_message_view(sent, &card_id, content, voip_type)
            }
            None => match message.kind {
                MessageKind::Image => self.media_message_view(
                    message,
                    MediaViewStyle {
                        fallback_label: "图片未缓存",
                        background: 0xe4e6e8,
                        foreground: 0x8a919b,
                    },
                    cx,
                ),
                MessageKind::Video => self.media_message_view(
                    message,
                    MediaViewStyle {
                        fallback_label: "视频未缓存",
                        background: 0x34383d,
                        foreground: 0xd1d5db,
                    },
                    cx,
                ),
                MessageKind::Voice => self.text_bubble(sent, text_view("语音")),
                MessageKind::Emoji => self.emoji_message_view(message, cx),
                MessageKind::File => {
                    let title = sanitize_text(&message.content);
                    self.info_card("文件", &title, "", 0x576b95)
                }
                _ => self.text_bubble(
                    sent,
                    rich_text(
                        &sanitize_text(&message.content),
                        &format!("body:{card_id}"),
                        14.,
                        0x1f1f1f,
                    ),
                ),
            },
        }
    }

    fn transfer_message_card(
        &self,
        title: &str,
        amount: &str,
        pay_sub_type: &str,
        receive_status: &str,
        sender_title: &str,
    ) -> AnyElement {
        let returned = matches!(pay_sub_type, "4" | "9");
        let overdue = pay_sub_type == "10";
        let received = matches!(pay_sub_type, "3") || matches!(receive_status, "1" | "3");
        let background = if returned {
            0xfde1c3
        } else if overdue {
            0xe9cfb3
        } else if received {
            0xfdce9d
        } else {
            0xf79c46
        };
        let status = if returned {
            "已退还"
        } else if overdue {
            "已过期"
        } else if received {
            "已收款"
        } else if pay_sub_type == "8" {
            "发起转账"
        } else if !sender_title.trim().is_empty() {
            sender_title
        } else {
            "转账"
        };
        let amount = if amount.trim().is_empty() {
            title.trim()
        } else {
            amount.trim()
        };
        let amount = if amount.starts_with('¥') {
            amount.to_string()
        } else {
            format!("¥{amount}")
        };
        div()
            .w(px(210.))
            .rounded_md()
            .overflow_hidden()
            .bg(ui_color(background))
            .text_color(ui_color(0xffffff))
            .flex()
            .flex_col()
            .child(
                div()
                    .px_3()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_base().child(amount))
                    .child(div().text_xs().child(status.to_string())),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(ui_color(0xffffff))
                    .text_xs()
                    .child("微信转账"),
            )
            .into_any_element()
    }

    fn red_packet_message_card(&self, title: &str) -> AnyElement {
        let title = if title.trim().is_empty() {
            "恭喜发财，大吉大利"
        } else {
            title.trim()
        };
        div()
            .w(px(210.))
            .rounded_md()
            .overflow_hidden()
            .bg(ui_color(0xfa9d3b))
            .text_color(ui_color(0xffffff))
            .flex()
            .flex_col()
            .child(
                div()
                    .px_3()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_sm().whitespace_normal().child(title.to_string()))
                    .child(div().text_xs().child("微信红包")),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(ui_color(0xfaecda))
                    .text_xs()
                    .text_color(ui_color(0xfaecda))
                    .child("微信红包"),
            )
            .into_any_element()
    }

    fn file_message_card(&self, title: &str, size: &str) -> AnyElement {
        self.stack_message_card("文件", title, size, "微信电脑版", 0x576b95)
    }

    fn chat_history_card(&self, title: &str, preview: &str) -> AnyElement {
        self.stack_message_card("聊天记录", title, preview, "聊天记录", 0x576b95)
    }

    fn stack_message_card(
        &self,
        label: &str,
        title: &str,
        detail: &str,
        footer: &str,
        accent: u32,
    ) -> AnyElement {
        let mut card = div()
            .w(px(230.))
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(ui_color(0xe3e3e3))
            .bg(ui_color(0xffffff))
            .flex()
            .flex_col()
            .child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui_color(accent))
                            .child(label.to_string()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(ui_color(0x1f1f1f))
                            .line_clamp(2)
                            .whitespace_normal()
                            .child(title.to_string()),
                    ),
            );
        if !detail.trim().is_empty() {
            card = card.child(
                div()
                    .px_3()
                    .pb_2()
                    .text_xs()
                    .text_color(ui_color(0x8a8a8a))
                    .line_clamp(3)
                    .whitespace_normal()
                    .child(detail.to_string()),
            );
        }
        card.child(
            div()
                .px_3()
                .py_1()
                .border_t_1()
                .border_color(ui_color(0xefefef))
                .text_xs()
                .text_color(ui_color(0x9e9e9e))
                .child(footer.to_string()),
        )
        .into_any_element()
    }

    fn remote_thumb(&mut self, key: &str, url: &str, cx: &mut Context<Self>) -> Option<Arc<Image>> {
        let url = url.trim();
        if url.is_empty() || normalize_tencent_url(url, None, false, false).is_none() {
            return None;
        }
        self.request_remote_image(key.to_string(), url.to_string(), None, None, cx);
        self.remote_images.get(key).and_then(|image| image.clone())
    }

    #[allow(clippy::too_many_arguments)]
    fn finder_message_card(
        &mut self,
        id: &str,
        title: &str,
        desc: &str,
        from: &str,
        url: &str,
        thumb_url: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let caption = if !title.trim().is_empty() && !title.contains("不支持") {
            title.trim()
        } else {
            desc.trim()
        };
        let thumb = self.remote_thumb(&format!("finder-thumb:{id}"), thumb_url, cx);
        let mut card = div()
            .id(ElementId::Name(format!("finder-card:{id}").into()))
            .w(px(150.))
            .rounded_md()
            .overflow_hidden()
            .bg(ui_color(0x333333))
            .text_color(ui_color(0xffffff))
            .flex()
            .flex_col()
            .child(match thumb {
                Some(image) => div()
                    .h(px(150.))
                    .w_full()
                    .overflow_hidden()
                    .child(img(image).w_full().h_full().object_fit(ObjectFit::Cover))
                    .into_any_element(),
                None => div()
                    .h(px(150.))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .child("视频号")
                    .into_any_element(),
            })
            .child(
                div()
                    .px_2()
                    .py_1()
                    .bg(ui_color(0x000000))
                    .text_xs()
                    .truncate()
                    .child(if from.trim().is_empty() {
                        "视频号".to_string()
                    } else {
                        from.to_string()
                    }),
            )
            .child(
                div()
                    .px_2()
                    .py_2()
                    .text_xs()
                    .line_clamp(2)
                    .whitespace_normal()
                    .child(if caption.is_empty() {
                        url.to_string()
                    } else {
                        caption.to_string()
                    }),
            );
        if is_http_url(url) {
            let url = url.to_string();
            card = card
                .cursor_pointer()
                .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)));
        }
        card.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn mini_program_card(
        &mut self,
        id: &str,
        title: &str,
        desc: &str,
        from: &str,
        url: &str,
        thumb_url: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let thumb = self.remote_thumb(&format!("mini-thumb:{id}"), thumb_url, cx);
        let mut card = div()
            .id(ElementId::Name(format!("mini-card:{id}").into()))
            .w(px(210.))
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(ui_color(0xe5e5e5))
            .bg(ui_color(0xffffff))
            .flex()
            .flex_col()
            .child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when_some(thumb, |this, image| {
                        this.child(
                            img(image)
                                .w(px(40.))
                                .h(px(40.))
                                .rounded_sm()
                                .object_fit(ObjectFit::Cover),
                        )
                    })
                    .when(!from.trim().is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(ui_color(0x576b95))
                                .truncate()
                                .child(from.to_string()),
                        )
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_color(ui_color(0x111827))
                            .line_clamp(2)
                            .whitespace_normal()
                            .child(title.to_string()),
                    )
                    .when(
                        !desc.trim().is_empty() && desc.trim() != title.trim(),
                        |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x8a8a8a))
                                    .line_clamp(2)
                                    .whitespace_normal()
                                    .child(desc.to_string()),
                            )
                        },
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(ui_color(0xefefef))
                    .text_xs()
                    .text_color(ui_color(0x9e9e9e))
                    .child("小程序"),
            );
        if is_http_url(url) {
            let url = url.to_string();
            card = card
                .cursor_pointer()
                .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)));
        }
        card.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn link_card_view(
        &mut self,
        id: &str,
        title: &str,
        desc: &str,
        url: &str,
        from: &str,
        thumb_url: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let heading = if title.trim().is_empty() { url } else { title };
        let source = if from.trim().is_empty() { url } else { from };
        let thumb = self.remote_thumb(&format!("link-thumb:{id}"), thumb_url, cx);
        let mut card = div()
            .id(ElementId::Name(format!("link-card:{id}").into()))
            .w(px(230.))
            .rounded_md()
            .border_1()
            .border_color(ui_color(0xe3e3e3))
            .bg(ui_color(0xffffff))
            .p_3()
            .flex()
            .flex_col()
            .gap_1()
            .when_some(thumb, |this, image| {
                this.child(
                    div()
                        .w(px(80.))
                        .h(px(60.))
                        .rounded_sm()
                        .overflow_hidden()
                        .child(img(image).w_full().h_full().object_fit(ObjectFit::Cover)),
                )
            })
            .child(
                div()
                    .text_sm()
                    .text_color(ui_color(0x1f1f1f))
                    .line_clamp(2)
                    .whitespace_normal()
                    .child(heading.to_string()),
            );
        let desc = desc.trim();
        if !desc.is_empty() && desc != heading.trim() {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(ui_color(0x8a8a8a))
                    .line_clamp(2)
                    .whitespace_normal()
                    .child(desc.to_string()),
            );
        }
        if !source.trim().is_empty() {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(ui_color(0x576b95))
                    .truncate()
                    .child(source.to_string()),
            );
        }
        if is_http_url(url) {
            let url = url.to_string();
            card = card
                .cursor_pointer()
                .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)));
        }
        card.into_any_element()
    }

    fn quote_view(
        &self,
        sent: bool,
        id: &str,
        content: &str,
        quote_title: &str,
        quote_content: &str,
    ) -> AnyElement {
        let mut stack = div()
            .flex()
            .flex_col()
            .gap_1()
            .max_w(px(420.))
            .when(sent, |this| this.items_end())
            .when(!sent, |this| this.items_start())
            .child(self.text_bubble(
                sent,
                rich_text(content, &format!("quote:{id}"), 14., 0x1f1f1f),
            ));
        if !quote_title.trim().is_empty() || !quote_content.trim().is_empty() {
            stack = stack.child(self.quote_preview(id, quote_title, quote_content));
        }
        stack.into_any_element()
    }

    fn quote_preview(&self, id: &str, quote_title: &str, quote_content: &str) -> AnyElement {
        let joined = match (
            quote_title.trim().is_empty(),
            quote_content.trim().is_empty(),
        ) {
            (false, false) => format!("{}：{}", quote_title.trim(), quote_content.trim()),
            (false, true) => quote_title.trim().to_string(),
            (true, false) => quote_content.trim().to_string(),
            (true, true) => String::new(),
        };
        let joined = joined.replace(['\n', '\r'], " ");
        div()
            .max_w(px(360.))
            .overflow_hidden()
            .rounded_sm()
            .bg(ui_color(0xf0f0f0))
            .px_2()
            .py_1()
            .child(
                div()
                    .text_xs()
                    .text_color(ui_color(0x7a7a7a))
                    .child(rich_text_preview(
                        &joined,
                        &format!("quote-preview:{id}"),
                        12.,
                        0x7a7a7a,
                    )),
            )
            .into_any_element()
    }

    fn info_card(&self, label: &str, title: &str, detail: &str, accent: u32) -> AnyElement {
        let mut card = div()
            .w(px(240.))
            .rounded_md()
            .border_1()
            .border_color(ui_color(0xe3e3e3))
            .bg(ui_color(0xffffff))
            .p_3()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(ui_color(accent))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(ui_color(0x1f1f1f))
                    .line_clamp(2)
                    .whitespace_normal()
                    .child(title.to_string()),
            );
        if !detail.trim().is_empty() {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(ui_color(0x8a8a8a))
                    .line_clamp(2)
                    .whitespace_normal()
                    .child(detail.to_string()),
            );
        }
        card.into_any_element()
    }

    fn media_message_view(
        &mut self,
        message: &Message,
        style: MediaViewStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_message_image(message, cx);
        let MediaViewStyle {
            fallback_label,
            background,
            foreground,
        } = style;
        let (width, height) = media_display_size(message);
        let image = self
            .message_images
            .get(&message_media_key(message))
            .cloned()
            .flatten();
        if let Some(image) = image {
            let mut container = div()
                .id(ElementId::Name(
                    format!("open-image:{}:{}", message.shard, message.local_id).into(),
                ))
                .rounded_md()
                .overflow_hidden()
                .w(px(width))
                .h(px(height))
                .flex()
                .items_start()
                .justify_start();
            if matches!(message.kind, MessageKind::Image) {
                let message = message.clone();
                let viewer_image = image.clone();
                container =
                    container
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_message_original(&message, viewer_image.clone(), cx);
                        }));
            }
            return container
                .child(
                    img(image)
                        .max_w(px(width))
                        .max_h(px(height))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element();
        }
        self.media_placeholder(fallback_label, width, height, background, foreground)
    }

    fn open_message_original(
        &self,
        message: &Message,
        fallback: Arc<Image>,
        cx: &mut Context<Self>,
    ) {
        let caption = image_caption(message);
        let original = self
            .store
            .as_ref()
            .and_then(Store::account_root)
            .and_then(|root| {
                let md5 = message
                    .media_md5
                    .clone()
                    .or_else(|| extract_image_md5(&message.content))
                    .unwrap_or_default();
                let username = self.selected.as_deref().unwrap_or("");
                let media = match self.image_keys.as_ref() {
                    Some(keys) => load_message_image_original(
                        root,
                        username,
                        &md5,
                        message.local_id,
                        message.create_time,
                        keys,
                    ),
                    None => find_resource_image(root, &md5).or_else(|| {
                        load_message_image_cached(
                            root,
                            username,
                            &md5,
                            message.local_id,
                            message.create_time,
                        )
                    }),
                };
                media.and_then(|media| decode_image_bytes(&media.data, &media.media_type))
            });
        self.open_image_viewer(original.unwrap_or(fallback), caption, cx);
    }

    fn open_image_viewer(&self, image: Arc<Image>, caption: String, cx: &mut Context<Self>) {
        let caption = if caption.trim().is_empty() {
            "图片预览".to_string()
        } else {
            caption
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(160.), px(110.)),
                size: size(px(1080.), px(800.)),
            })),
            titlebar: Some(TitlebarOptions {
                title: Some("图片预览".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let _ = cx.open_window(options, move |_, cx| {
            cx.new(|_| ImagePreview {
                image: image.clone(),
                caption: caption.clone(),
            })
        });
    }

    fn emoji_message_view(&mut self, message: &Message, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_emoji_image(message, cx);
        let reference = parse_emoji_reference(&message.content);
        let (box_w, box_h) = emoji_box_size(reference.as_ref());
        let image = self
            .message_images
            .get(&message_media_key(message))
            .cloned()
            .flatten();
        let container = div()
            .id(ElementId::Name(
                format!("open-emoji:{}:{}", message.shard, message.local_id).into(),
            ))
            .flex_none()
            .w(px(box_w))
            .h(px(box_h))
            .flex()
            .items_center()
            .justify_center();
        if let Some(image) = image {
            let caption = image_caption(message);
            let viewer_image = image.clone();
            return container
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_image_viewer(viewer_image.clone(), caption.clone(), cx);
                }))
                .child(
                    img(image)
                        .max_w(px(box_w))
                        .max_h(px(box_h))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element();
        }
        container
            .rounded_md()
            .bg(ui_color(0xf2f3f5))
            .text_xs()
            .text_color(ui_color(0x8a919b))
            .child("表情未缓存")
            .into_any_element()
    }

    fn media_placeholder(
        &self,
        label: &str,
        width: f32,
        height: f32,
        background: u32,
        foreground: u32,
    ) -> AnyElement {
        div()
            .w(px(width))
            .h(px(height))
            .rounded_md()
            .overflow_hidden()
            .bg(ui_color(background))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_xs()
                    .text_color(ui_color(foreground))
                    .whitespace_normal()
                    .child(label.to_string()),
            )
            .into_any_element()
    }
}

impl WeChatApp {
    fn render_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.message_list_handler_installed {
            let view = cx.entity();
            self.messages_list.set_scroll_handler(move |event, _, cx| {
                if event.is_scrolled && event.visible_range.start == 0 {
                    view.update(cx, |this, _| {
                        if this.has_more_older && !this.loading_older {
                            this.pending_older_load = true;
                        }
                    });
                }
            });
            self.message_list_handler_installed = true;
        }
        if self.pending_older_load {
            self.pending_older_load = false;
            self.load_older();
        }
        if self.pending_scroll_bottom {
            self.messages_list.scroll_to(ListOffset {
                item_ix: self.message_items.len(),
                offset_in_item: px(0.),
            });
            self.pending_scroll_bottom = false;
        }
        if self.store.is_some() {
            self.ensure_search_input(window, cx);
        }
        if self.view == View::Contacts {
            self.ensure_contact_input(window, cx);
        }
        if self.view == View::Moments {
            self.ensure_sns_input(window, cx);
        }

        let search_input = self.store.as_ref().and_then(|_| {
            self.search_input.clone().map(|state| {
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(ui_color(0xe5e7eb))
                    .child(
                        Input::new(&state)
                            .cleanable(true)
                            .aria_label("搜索聊天记录"),
                    )
                    .into_any_element()
            })
        });

        let sidebar_body = if self.store.is_none() && !self.accounts.is_empty() {
            let rows = self
                .accounts
                .iter()
                .map(|account| self.account_row(account, cx))
                .collect::<Vec<_>>();
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .px_4()
                        .py_2()
                        .text_xs()
                        .text_color(ui_color(0x6b7280))
                        .child("选择账号"),
                )
                .children(rows)
                .into_any_element()
        } else if !self.query.is_empty() {
            self.search_body(cx)
        } else if self.sessions.is_empty() {
            div().into_any_element()
        } else {
            let sizes = Rc::new(vec![size(px(320.), px(56.)); self.sessions.len()]);
            let scroll = self.sessions_scroll.clone();
            let list = gpui_kit::base::v_virtual_list(
                cx.entity(),
                "sessions",
                sizes,
                |this, range, _, cx| {
                    range
                        .map(|index| this.session_row(&this.sessions[index], cx))
                        .collect()
                },
            )
            .track_scroll(&scroll)
            .into_any_element();
            let viewport = self.smooth_viewport(
                "sessions",
                SmoothTarget::Handle(scroll.as_ref().clone()),
                list,
                cx,
            );
            div()
                .size_full()
                .flex()
                .overflow_hidden()
                .child(div().flex_1().h_full().overflow_hidden().child(viewport))
                .child(Self::scroll_rail("sessions-rail", &scroll))
                .into_any_element()
        };

        let state = self.messages_list.clone();
        let target_state = state.clone();
        let view = cx.entity();
        let items = Rc::new(self.message_items.clone());
        let list = gpui_kit::list(state, move |index, _, cx| {
            view.update(cx, |this, cx| this.message_list_item(index, &items, cx))
        })
        .size_full()
        .into_any_element();
        let message_list = {
            let view = cx.entity();
            let target = SmoothTarget::List(target_state.clone());
            SmoothWheel {
                child: div().size_full().px_6().py_4().child(list),
                on_wheel: Rc::new(move |event, _, cx| {
                    let target = target.clone();
                    view.update(cx, |this, cx| {
                        let delta = event.delta.pixel_delta(px(18.)).y;
                        if delta > px(0.) {
                            let top = this.messages_list.logical_scroll_top();
                            if top.item_ix == 0 && top.offset_in_item == px(0.) {
                                if this.has_more_older && !this.loading_older {
                                    this.load_older();
                                    cx.notify();
                                }
                                return;
                            }
                        }
                        this.smooth_wheel("messages", target, event, cx);
                    });
                }),
            }
            .into_any_element()
        };
        let message_rail = Self::scroll_rail("message-rail", &target_state);

        let selected_name = self
            .selected
            .as_deref()
            .map(|username| self.session_display_name(username))
            .unwrap_or_else(|| "微信".to_string());
        let header_label = self
            .store
            .as_ref()
            .map(|store| format!("微信 · {}", store.account()))
            .unwrap_or_else(|| "微信数据".to_string());
        let empty_hint = if self.store.is_none() {
            if self.accounts.is_empty() {
                Some("未找到账号数据")
            } else {
                Some("从左侧选择账号")
            }
        } else if self.selected.is_none() {
            Some("选择左侧会话开始阅读")
        } else if self.messages.is_empty() {
            Some("没有消息")
        } else {
            None
        };

        div()
            .flex_1()
            .h_full()
            .flex()
            .bg(ui_color(0xededed))
            .text_color(ui_color(0x191919))
            .child(
                div()
                    .w(px(320.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(ui_color(0xf7f7f7))
                    .border_r_1()
                    .border_color(ui_color(0xe5e7eb))
                    .child(
                        div()
                            .h(px(44.))
                            .px_4()
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(ui_color(0xe5e7eb))
                            .text_sm()
                            .truncate()
                            .child(header_label),
                    )
                    .children(search_input)
                    .child(div().flex_1().overflow_hidden().child(sidebar_body))
                    .when(!self.status.trim().is_empty(), |this| {
                        this.child(
                            div()
                                .px_4()
                                .py_2()
                                .border_t_1()
                                .border_color(ui_color(0xe5e7eb))
                                .text_xs()
                                .text_color(ui_color(0x6b7280))
                                .truncate()
                                .child(self.status.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(44.))
                            .px_4()
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(ui_color(0xe5e7eb))
                            .child(selected_name),
                    )
                    .child(
                        div()
                            .id("message-list")
                            .aria_label("消息列表")
                            .test_support()
                            .flex_1()
                            .overflow_hidden()
                            .when_some(empty_hint, |this, hint| {
                                this.px_6()
                                    .py_4()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(div().text_color(ui_color(0x6b7280)).child(hint))
                            })
                            .when(empty_hint.is_none(), |this| {
                                this.flex()
                                    .child(
                                        div()
                                            .flex_1()
                                            .h_full()
                                            .overflow_hidden()
                                            .child(message_list),
                                    )
                                    .child(message_rail)
                            }),
                    ),
            )
            .into_any_element()
    }

    fn message_list_item(
        &mut self,
        index: usize,
        items: &[MessageListItem],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match items.get(index) {
            Some(MessageListItem::OlderButton) => self.older_button(cx),
            Some(MessageListItem::Day(timestamp)) => self.day_separator(*timestamp),
            Some(MessageListItem::Message(message_index)) => {
                match self.messages.get(*message_index).cloned() {
                    Some(message) => self.message_row(&message, cx),
                    None => div().into_any_element(),
                }
            }
            None => div().into_any_element(),
        }
    }
}

impl WeChatApp {
    fn nav_item(&self, view: View, label: &str, cx: &mut Context<Self>) -> AnyElement {
        let active = self.view == view;
        div()
            .id(ElementId::Name(format!("nav:{label}").into()))
            .aria_label(label.to_string())
            .test_support()
            .w_full()
            .h(px(46.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(active, |this| this.bg(ui_color(0xe4f6ec)))
            .hover(|style| style.bg(ui_color(0xebebeb)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.switch_view(view, cx);
            }))
            .child(self.nav_icon(view, if active { 0x07964c } else { 0x4b5563 }))
            .into_any_element()
    }

    fn nav_icon(&self, view: View, color: u32) -> AnyElement {
        let data: &'static [u8] = match view {
            View::Chat => ICON_CHAT,
            View::Moments => ICON_MOMENTS,
            View::Contacts => ICON_CONTACTS,
            View::Finder => ICON_FINDER,
            View::MiniPrograms => ICON_MINI,
            View::Payments => ICON_PAYMENTS,
            View::Search => ICON_SEARCH,
        };
        svg()
            .data(data)
            .flex_none()
            .w(px(20.))
            .h(px(20.))
            .text_color(ui_color(color))
            .into_any_element()
    }

    fn nav_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let self_names = self
            .store
            .as_ref()
            .map(Store::self_names)
            .unwrap_or_else(|| vec!["我".to_string()]);
        let avatar_key = self_names
            .iter()
            .find(|name| self.avatars.contains_key(*name))
            .cloned()
            .or_else(|| self_names.first().cloned())
            .unwrap_or_else(|| "我".to_string());
        div()
            .w(px(56.))
            .h_full()
            .flex()
            .flex_col()
            .bg(ui_color(0xf2f2f2))
            .border_r_1()
            .border_color(ui_color(0xe0e0e0))
            .child(
                div()
                    .h(px(56.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.avatar_chip(&avatar_key, "我", 34.)),
            )
            .child(self.nav_item(View::Chat, "聊天", cx))
            .child(self.nav_item(View::Moments, "朋友圈", cx))
            .child(self.nav_item(View::Contacts, "联系人", cx))
            .child(self.nav_item(View::Finder, "视频号", cx))
            .child(self.nav_item(View::MiniPrograms, "小程序", cx))
            .child(self.nav_item(View::Payments, "收付款", cx))
            .child(self.nav_item(View::Search, "搜索", cx))
            .child(div().flex_1())
            .child(self.settings_button(cx))
            .into_any_element()
    }

    fn settings_button(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("nav:设置")
            .aria_label("设置")
            .test_support()
            .w_full()
            .h(px(46.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|style| style.bg(ui_color(0xebebeb)))
            .on_click(cx.listener(|this, _, _, cx| this.open_settings(false, cx)))
            .child(
                svg()
                    .data(ICON_SETTINGS)
                    .flex_none()
                    .w(px(20.))
                    .h(px(20.))
                    .text_color(ui_color(0x4b5563)),
            )
            .into_any_element()
    }

    fn open_settings(&mut self, setup: bool, cx: &mut Context<Self>) {
        self.settings_draft = self.settings.clone();
        self.setup_mode = setup;
        self.settings_open = true;
        cx.notify();
    }

    fn pick_settings_path(
        &mut self,
        field: SettingsField,
        directory: bool,
        prompt: &'static str,
        cx: &mut Context<Self>,
    ) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: !directory,
            directories: directory,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        let task = cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| {
                    match field {
                        SettingsField::Account => this.settings_draft.account_path = Some(path),
                        SettingsField::Storage => this.settings_draft.storage_dir = Some(path),
                    }
                    cx.notify();
                });
            }
        });
        self.remote_tasks.push(task);
    }

    fn settings_path_row(
        &self,
        spec: PathRowSpec,
        value: Option<&PathBuf>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let PathRowSpec {
            label,
            hint,
            field,
            directory,
            prompt,
        } = spec;
        let text = value
            .map(|path| path.display().to_string())
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| "未选择".to_string());
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(ui_color(0x1f1f1f))
                            .child(label.to_string()),
                    )
                    .child(
                        div()
                            .id(ElementId::Name(format!("settings-pick:{label}").into()))
                            .flex_none()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(ui_color(0xd6d6d6))
                            .bg(ui_color(0xffffff))
                            .text_xs()
                            .cursor_pointer()
                            .hover(|style| style.bg(ui_color(0xf5f5f5)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_settings_path(field, directory, prompt, cx);
                            }))
                            .child("选择"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(ui_color(if value.is_some() { 0x4b5563 } else { 0x9aa0a6 }))
                    .truncate()
                    .child(text),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(ui_color(0x8a8a8a))
                    .child(hint.to_string()),
            )
            .into_any_element()
    }

    fn settings_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let draft = self.settings_draft.clone();
        let initialized = self.store.is_some();
        let title = if self.setup_mode {
            "初始化"
        } else {
            "设置"
        };
        let panel = div()
            .w(px(720.))
            .max_h(px(640.))
            .rounded_lg()
            .border_1()
            .border_color(ui_color(0xdcdcdc))
            .bg(ui_color(0xffffff))
            .text_color(ui_color(0x1f1f1f))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(52.))
                    .px_6()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(ui_color(0xeeeeee))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_lg().child(title)),
                    )
                    .when(!self.setup_mode, |this| {
                        this.child(
                            div()
                                .id("settings-close")
                                .test_support()
                                .w(px(26.))
                                .h(px(26.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|style| style.bg(ui_color(0xf0f0f0)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings_open = false;
                                    cx.notify();
                                }))
                                .child("✕"),
                        )
                    }),
            )
            .child(
                div()
                    .id("settings-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .when(self.setup_mode, |this| {
                        this.child(
                            div()
                                .rounded_md()
                                .border_1()
                                .border_color(ui_color(0xd9ecdf))
                                .bg(ui_color(0xf3faf6))
                                .p_3()
                                .text_xs()
                                .text_color(ui_color(0x2f6f4e))
                                .child(
                                    "首次使用：选择微信数据目录与存储目录。程序会自动获取/验证密钥、定位 Broker，并进入实时模式；聊天图片和表情会按需解密后写入存储目录。",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .px_1()
                                .child(settings_step_row(
                                    "1. 选择微信数据目录",
                                    draft.account_path.is_some(),
                                ))
                                .child(settings_step_row(
                                    "2. 选择存储目录",
                                    draft.storage_dir.is_some(),
                                ))
                                .child(settings_step_row(
                                    "3. 自动获取密钥并启动实时模式",
                                    initialized,
                                ))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(ui_color(0x8a8a8a))
                                        .child("图片、表情会在浏览时解密并写入存储目录"),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(div().text_xs().text_color(ui_color(0x8a8a8a)).child("目录"))
                            .child(self.settings_path_row(
                                PathRowSpec {
                                    label: "微信数据目录",
                                    hint: "xwechat_files 根目录、账号目录或 db_storage",
                                    field: SettingsField::Account,
                                    directory: true,
                                    prompt: "选择微信数据目录",
                                },
                                draft.account_path.as_ref(),
                                cx,
                            ))
                            .child(self.settings_path_row(
                                PathRowSpec {
                                    label: "存储目录",
                                    hint: "保存密钥、索引以及解密后的图片/表情",
                                    field: SettingsField::Storage,
                                    directory: true,
                                    prompt: "选择存储目录",
                                },
                                draft.storage_dir.as_ref(),
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(div().text_sm().child("深色模式"))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(ui_color(0x8a8a8a))
                                            .child("切换深色 / 浅色界面"),
                                    ),
                            )
                            .child(
                                div()
                                    .id("settings-theme")
                                    .w(px(42.))
                                    .h(px(24.))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(if draft.theme == "dark" {
                                        ui_color(0x07b75b)
                                    } else {
                                        ui_color(0xd1d5db)
                                    })
                                    .flex()
                                    .items_center()
                                    .when(draft.theme == "dark", |this| this.justify_end())
                                    .when(draft.theme != "dark", |this| this.justify_start())
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings_draft.theme =
                                            if this.settings_draft.theme == "dark" {
                                                "light".to_string()
                                            } else {
                                                "dark".to_string()
                                            };
                                        set_dark_mode(this.settings_draft.theme == "dark");
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .m(px(3.))
                                            .w(px(18.))
                                            .h(px(18.))
                                            .rounded_full()
                                            .bg(ui_color(0xffffff)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(div().text_sm().child("远程 CDN"))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(ui_color(0x8a8a8a))
                                            .child("缺失图片/表情时直连腾讯 CDN（不使用第三方 WxCDN）"),
                                    ),
                            )
                            .child(
                                div()
                                    .id("settings-remote-cdn")
                                    .w(px(42.))
                                    .h(px(24.))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(ui_color(if draft.remote_cdn { 0x07b75b } else { 0xd1d5db }))
                                    .flex()
                                    .items_center()
                                    .when(draft.remote_cdn, |this| this.justify_end())
                                    .when(!draft.remote_cdn, |this| this.justify_start())
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings_draft.remote_cdn = !this.settings_draft.remote_cdn;
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .m(px(3.))
                                            .w(px(18.))
                                            .h(px(18.))
                                            .rounded_full()
                                            .bg(ui_color(0xffffff)),
                                    ),
                            ),
                    )
                    .when(cfg!(target_os = "windows"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_4()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .child(div().text_sm().child("关闭窗口时最小化到托盘"))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(ui_color(0x8a8a8a))
                                                .child("保持实时连接在后台运行；从托盘图标恢复或退出"),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("settings-tray")
                                        .test_support()
                                        .w(px(42.))
                                        .h(px(24.))
                                        .flex_none()
                                        .rounded_full()
                                        .bg(ui_color(if draft.minimize_to_tray {
                                            0x07b75b
                                        } else {
                                            0xd1d5db
                                        }))
                                        .flex()
                                        .items_center()
                                        .when(draft.minimize_to_tray, |this| this.justify_end())
                                        .when(!draft.minimize_to_tray, |this| {
                                            this.justify_start()
                                        })
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.settings_draft.minimize_to_tray =
                                                !this.settings_draft.minimize_to_tray;
                                            set_minimize_to_tray(
                                                this.settings_draft.minimize_to_tray,
                                            );
                                            cx.notify();
                                        }))
                                        .child(
                                            div()
                                                .m(px(3.))
                                                .w(px(18.))
                                                .h(px(18.))
                                                .rounded_full()
                                                .bg(ui_color(0xffffff)),
                                        ),
                                ),
                        )
                    })
                    .when(!self.status.trim().is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(ui_color(0x9a5050))
                                .whitespace_normal()
                                .child(self.status.clone()),
                        )
                    })
            )
            .child(
                div()
                    .h(px(58.))
                    .px_6()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .border_t_1()
                    .border_color(ui_color(0xeeeeee))
                    .when(!self.setup_mode, |this| {
                        this.child(
                            div()
                                .id("settings-cancel")
                                .test_support()
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .cursor_pointer()
                                .text_sm()
                                .text_color(ui_color(0x4b5563))
                                .hover(|style| style.bg(ui_color(0xf3f3f3)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings_open = false;
                                    cx.notify();
                                }))
                                .child("取消"),
                        )
                    })
                    .child(
                        div()
                            .id("settings-save")
                            .test_support()
                            .px_4()
                            .py_2()
                            .rounded_md()
                            .bg(ui_color(0x07b75b))
                            .text_sm()
                            .text_color(ui_color(0xffffff))
                            .cursor_pointer()
                            .hover(|style| style.bg(ui_color(0x06a552)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.begin_initialize(cx);
                            }))
                            .child(if self.init_busy {
                                "正在初始化…"
                            } else if self.setup_mode {
                                "开始初始化"
                            } else {
                                "保存并重新连接"
                            }),
                    ),
            );
        div()
            .id("settings-overlay")
            .test_support()
            .absolute()
            .inset_0()
            .occlude()
            .bg(gpui_kit::rgba(0x00000066))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .on_mouse_down(MouseButton::Left, |_, _, _| {})
            .child(panel)
            .into_any_element()
    }

    fn contact_entry_row(&self, entry: &ContactEntry, cx: &mut Context<Self>) -> AnyElement {
        match entry {
            ContactEntry::Header(letter) => div()
                .w_full()
                .h(px(28.))
                .px_4()
                .flex()
                .items_center()
                .bg(ui_color(0xfafafa))
                .border_b_1()
                .border_color(ui_color(0xf3f4f6))
                .text_xs()
                .text_color(ui_color(0x6b7280))
                .child(letter.clone())
                .into_any_element(),
            ContactEntry::Item(contact) => {
                let active = self.selected_contact.as_deref() == Some(contact.username.as_str());
                let username = contact.username.clone();
                let (badge, badge_color) = contact_badge(contact);
                div()
                    .id(ElementId::Name(
                        format!("contact:{}", contact.username).into(),
                    ))
                    .aria_label(contact.display_name.clone())
                    .test_support()
                    .w_full()
                    .h(px(64.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .cursor_pointer()
                    .when(active, |this| this.bg(ui_color(0xdedede)))
                    .hover(|style| style.bg(ui_color(0xeaeaea)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_contact = Some(username.clone());
                        cx.notify();
                    }))
                    .child(self.avatar_chip(&contact.username, &contact.display_name, 40.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_sm()
                                            .text_color(ui_color(0x111827))
                                            .child(contact.display_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .px_1()
                                            .py(px(1.))
                                            .rounded_sm()
                                            .bg(ui_color(badge_color))
                                            .text_xs()
                                            .child(badge),
                                    ),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(ui_color(0x8a8a8a))
                                    .child(contact.username.clone()),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    fn contact_profile(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(username) = self.selected_contact.as_deref() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(ui_color(0x8a8a8a))
                .child("选择联系人查看资料")
                .into_any_element();
        };
        let Some(contact) = self
            .contacts
            .iter()
            .find(|contact| contact.username == username)
        else {
            return div().into_any_element();
        };
        let (badge, _) = contact_badge(contact);
        let send_username = contact.username.clone();
        let can_chat = contact.is_group || !contact.is_official;
        let mut fields: Vec<(&str, String)> = Vec::new();
        if !contact.remark.trim().is_empty() {
            fields.push(("备注", contact.remark.clone()));
        }
        if !contact.alias.trim().is_empty() {
            fields.push(("微信号", contact.alias.clone()));
        }
        if !contact.description.trim().is_empty() {
            fields.push(("描述", contact.description.clone()));
        }
        fields.push(("类型", badge.to_string()));
        if !contact.pinyin_initial.trim().is_empty() {
            fields.push(("拼音", contact.pinyin_initial.clone()));
        }
        let field_rows = fields
            .into_iter()
            .map(|(label, value)| {
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .text_sm()
                            .text_color(ui_color(0x8a8a8a))
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .text_color(ui_color(0x111827))
                            .whitespace_normal()
                            .child(value),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .size_full()
            .flex()
            .justify_center()
            .p_6()
            .child(
                div()
                    .w(px(520.))
                    .rounded_md()
                    .border_1()
                    .border_color(ui_color(0xd4dad5))
                    .bg(ui_color(0xf8faf8))
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_4()
                            .child(self.avatar_chip(&contact.username, &contact.display_name, 64.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_lg()
                                            .text_color(ui_color(0x111827))
                                            .whitespace_normal()
                                            .child(contact.display_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(ui_color(0x6b7280))
                                            .child(format!("微信ID：{}", contact.username)),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(ui_color(0x576b95))
                                            .child(badge.to_string()),
                                    ),
                            ),
                    )
                    .child(div().h(px(1.)).w_full().bg(ui_color(0xe5e7eb)))
                    .children(field_rows)
                    .when(can_chat, |this| {
                        this.child(
                            div()
                                .id("contact-open-chat")
                                .aria_label("发消息")
                                .test_support()
                                .mt_2()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(ui_color(0x07c160))
                                .text_color(ui_color(0xffffff))
                                .cursor_pointer()
                                .hover(|style| style.bg(ui_color(0x06ad56)))
                                .child("发消息")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.view = View::Chat;
                                    this.select_session(send_username.clone());
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_contacts(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let contacts = self.filtered_contacts();
        let mut entries: Vec<ContactEntry> = Vec::new();
        let mut last_initial = String::new();
        for contact in contacts {
            if contact.pinyin_initial != last_initial {
                last_initial = contact.pinyin_initial.clone();
                entries.push(ContactEntry::Header(last_initial.clone()));
            }
            entries.push(ContactEntry::Item(contact));
        }
        let sizes = Rc::new(
            entries
                .iter()
                .map(|entry| match entry {
                    ContactEntry::Header(_) => size(px(320.), px(28.)),
                    ContactEntry::Item(_) => size(px(320.), px(64.)),
                })
                .collect::<Vec<_>>(),
        );
        let scroll = self.contacts_scroll.clone();
        let entries = Rc::new(entries);
        let list = gpui_kit::base::v_virtual_list(
            cx.entity(),
            "contacts",
            sizes,
            move |this, range, _, cx| {
                range
                    .map(|index| this.contact_entry_row(&entries[index], cx))
                    .collect()
            },
        )
        .track_scroll(&scroll)
        .into_any_element();
        let input = self.contact_input.as_ref().map(|state| {
            div()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(ui_color(0xe5e7eb))
                .child(Input::new(state).cleanable(true).aria_label("搜索联系人"))
                .into_any_element()
        });

        div()
            .flex_1()
            .h_full()
            .flex()
            .bg(ui_color(0xededed))
            .text_color(ui_color(0x191919))
            .child(
                div()
                    .w(px(320.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(ui_color(0xf7f7f7))
                    .border_r_1()
                    .border_color(ui_color(0xe5e7eb))
                    .child(
                        div()
                            .h(px(44.))
                            .px_4()
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(ui_color(0xe5e7eb))
                            .text_sm()
                            .child(format!("联系人 · {}", self.filtered_contacts().len())),
                    )
                    .children(input)
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .flex()
                            .when(self.contacts.is_empty(), |this| {
                                this.items_center().justify_center().child(
                                    div()
                                        .text_sm()
                                        .text_color(ui_color(0x8a8a8a))
                                        .child("暂无联系人"),
                                )
                            })
                            .when(!self.contacts.is_empty(), |this| {
                                let viewport = self.smooth_viewport(
                                    "contacts",
                                    SmoothTarget::Handle(scroll.as_ref().clone()),
                                    list,
                                    cx,
                                );
                                this.child(
                                    div().flex_1().h_full().overflow_hidden().child(viewport),
                                )
                                .child(Self::scroll_rail("contacts-rail", &scroll))
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(44.))
                            .px_4()
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(ui_color(0xe5e7eb))
                            .child("通讯录"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(self.contact_profile(cx)),
                    ),
            )
            .into_any_element()
    }

    fn sns_media_cell(
        &self,
        post: &SnsPost,
        media: &wechat_data::SnsMedia,
        size: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("{}:{}", post.tid, media.id);
        let image = self
            .sns_images
            .get(&key)
            .and_then(|image| image.clone())
            .or_else(|| self.remote_images.get(&key).and_then(|image| image.clone()));
        if let Some(image) = image {
            let caption = if post.display_name.trim().is_empty() {
                post.username.clone()
            } else {
                post.display_name.clone()
            };
            let viewer_image = image.clone();
            let stored_md5 = media.md5.clone();
            let caption_for_viewer = caption.clone();
            return div()
                .id(ElementId::Name(
                    format!("open-sns:{}:{}", post.tid, media.id).into(),
                ))
                .w(px(size))
                .h(px(size))
                .rounded_sm()
                .overflow_hidden()
                .bg(ui_color(0xf0f0f0))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    let original = this
                        .store
                        .as_ref()
                        .and_then(Store::account_root)
                        .and_then(|root| find_resource_image(root, &stored_md5))
                        .and_then(|media| decode_image_bytes(&media.data, &media.media_type));
                    this.open_image_viewer(
                        original.unwrap_or_else(|| viewer_image.clone()),
                        caption_for_viewer.clone(),
                        cx,
                    );
                }))
                .child(
                    img(image)
                        .w(px(size))
                        .h(px(size))
                        .object_fit(ObjectFit::Cover),
                )
                .into_any_element();
        }
        div()
            .w(px(size))
            .h(px(size))
            .rounded_sm()
            .bg(ui_color(0xe8eaec))
            .flex()
            .items_center()
            .justify_center()
            .text_xs()
            .text_color(ui_color(0x8a919b))
            .child(if media.kind == "video" {
                "视频"
            } else {
                "图片"
            })
            .into_any_element()
    }

    fn sns_post_card(&self, post: &SnsPost, cx: &mut Context<Self>) -> AnyElement {
        let name = if post.display_name.trim().is_empty() {
            post.username.clone()
        } else {
            post.display_name.clone()
        };
        let mut body = div().flex().flex_col().gap_2().child(
            div()
                .text_sm()
                .text_color(ui_color(0x111827))
                .whitespace_normal()
                .child(post.text.clone()),
        );
        if !post.title.trim().is_empty() || !post.link_url.trim().is_empty() {
            let url = post.link_url.clone();
            let title = if post.title.trim().is_empty() {
                post.link_url.clone()
            } else {
                post.title.clone()
            };
            body = body.child(
                div()
                    .id(ElementId::Name(format!("sns-link:{}", post.tid).into()))
                    .aria_label(title.clone())
                    .test_support()
                    .w(px(420.))
                    .rounded_sm()
                    .bg(ui_color(0xf7f7f7))
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|style| style.bg(ui_color(0xefefef)))
                    .child(
                        div()
                            .text_sm()
                            .text_color(ui_color(0x111827))
                            .line_clamp(2)
                            .whitespace_normal()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui_color(0x8a8a8a))
                            .truncate()
                            .child(post.link_url.clone()),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if !url.is_empty() {
                            cx.open_url(&url);
                        }
                    })),
            );
        }
        if !post.media.is_empty() {
            let single = post.media.len() == 1;
            let cells = post
                .media
                .iter()
                .map(|media| self.sns_media_cell(post, media, if single { 220. } else { 116. }, cx))
                .collect::<Vec<_>>();
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .max_w(px(360.))
                    .children(cells),
            );
        }
        if !post.location.trim().is_empty() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(ui_color(0x576b95))
                    .child(post.location.clone()),
            );
        }
        if !post.likes.is_empty() || !post.comments.is_empty() {
            let mut box_children: Vec<AnyElement> = Vec::new();
            if !post.likes.is_empty() {
                let names = post
                    .likes
                    .iter()
                    .map(|like| {
                        if like.nickname.trim().is_empty() {
                            like.username.clone()
                        } else {
                            like.nickname.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("、");
                box_children.push(
                    div()
                        .text_xs()
                        .text_color(ui_color(0x576b95))
                        .whitespace_normal()
                        .child(format!("赞 {names}"))
                        .into_any_element(),
                );
            }
            for comment in &post.comments {
                let who = if comment.nickname.trim().is_empty() {
                    comment.username.clone()
                } else {
                    comment.nickname.clone()
                };
                let label = if comment.ref_nickname.trim().is_empty() {
                    format!("{who}：")
                } else {
                    format!("{who} 回复 {}：", comment.ref_nickname)
                };
                box_children.push(
                    div()
                        .flex()
                        .gap_1()
                        .text_xs()
                        .whitespace_normal()
                        .child(
                            div()
                                .flex_none()
                                .text_color(ui_color(0x576b95))
                                .child(label),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_color(ui_color(0x333333))
                                .child(comment.content.clone()),
                        )
                        .into_any_element(),
                );
            }
            body = body.child(
                div()
                    .w_full()
                    .rounded_sm()
                    .bg(ui_color(0xf3f3f3))
                    .px_2()
                    .py_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(box_children),
            );
        }

        div()
            .id(ElementId::Name(format!("sns:{}", post.tid).into()))
            .aria_label(name.clone())
            .test_support()
            .w_full()
            .rounded_sm()
            .bg(ui_color(0xffffff))
            .p_4()
            .flex()
            .items_start()
            .gap_3()
            .child(self.avatar_chip(&post.username, &name, 36.))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(div().text_sm().text_color(ui_color(0x576b95)).child(name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x9e9e9e))
                                    .child(relative_time(post.create_time)),
                            ),
                    )
                    .child(body),
            )
            .into_any_element()
    }

    fn render_moments(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let authors = self.sns_authors();
        let selected_author = self.sns_author.clone();
        let author_rows = authors
            .iter()
            .map(|(username, name, count)| {
                let username = username.clone();
                let click_username = username.clone();
                let active = selected_author.as_deref() == Some(username.as_str());
                div()
                    .id(ElementId::Name(format!("sns-author:{username}").into()))
                    .aria_label(name.clone())
                    .test_support()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .when(active, |this| this.bg(ui_color(0xdedede)))
                    .hover(|style| style.bg(ui_color(0xeaeaea)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sns_author = Some(click_username.clone());
                        cx.notify();
                    }))
                    .child(self.avatar_chip(&username, name, 32.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_sm().child(name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x8a8a8a))
                                    .child(format!("{count} 条")),
                            ),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let input = self.sns_input.as_ref().map(|state| {
            div()
                .px_3()
                .py_2()
                .child(Input::new(state).cleanable(true).aria_label("搜索朋友圈"))
                .into_any_element()
        });
        let all_active = self.sns_author.is_none();
        let posts = self.filtered_sns_posts();
        let post_cards = posts
            .into_iter()
            .map(|post| self.sns_post_card(post, cx))
            .collect::<Vec<_>>();
        let can_more = self.sns_before.is_some();
        let no_posts = post_cards.is_empty();

        div()
            .flex_1()
            .h_full()
            .flex()
            .bg(ui_color(0xededed))
            .text_color(ui_color(0x191919))
            .child(
                div()
                    .w(px(280.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(ui_color(0xededed))
                    .border_r_1()
                    .border_color(ui_color(0xe0e0e0))
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_sm().child("朋友圈"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x8a8a8a))
                                    .child(format!("{} 位", authors.len())),
                            ),
                    )
                    .children(input)
                    .child(
                        div()
                            .id("sns-all")
                            .aria_label("全部朋友圈")
                            .test_support()
                            .px_3()
                            .py_2()
                            .text_sm()
                            .cursor_pointer()
                            .when(all_active, |this| this.bg(ui_color(0xdedede)))
                            .hover(|style| style.bg(ui_color(0xeaeaea)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sns_author = None;
                                cx.notify();
                            }))
                            .child("全部"),
                    )
                    .child(
                        div()
                            .id("sns-author-list")
                            .flex_1()
                            .flex()
                            .overflow_hidden()
                            .child({
                                let scroll = self.sns_author_scroll.clone();
                                let viewport = div()
                                    .id("sns-author-viewport")
                                    .size_full()
                                    .overflow_y_scroll()
                                    .track_scroll(&scroll)
                                    .flex()
                                    .flex_col()
                                    .children(author_rows);
                                let viewport = self.smooth_viewport(
                                    "sns-authors",
                                    SmoothTarget::Handle(scroll),
                                    viewport,
                                    cx,
                                );
                                div().flex_1().h_full().overflow_hidden().child(viewport)
                            })
                            .child(Self::scroll_rail(
                                "sns-authors-rail",
                                &self.sns_author_scroll,
                            )),
                    )
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(ui_color(0xe0e0e0))
                            .text_xs()
                            .text_color(ui_color(0x8a8a8a))
                            .truncate()
                            .child(self.status.clone()),
                    ),
            )
            .child({
                let sns_scroll = self.sns_scroll.clone();
                let content = div().w_full().flex().justify_center().child(
                    div()
                        .w(px(672.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .h(px(120.))
                                .w_full()
                                .rounded_sm()
                                .bg(ui_color(0x333333))
                                .flex()
                                .items_end()
                                .justify_end()
                                .p_4()
                                .text_sm()
                                .text_color(ui_color(0xffffff))
                                .child("朋友圈"),
                        )
                        .children(post_cards)
                        .when(no_posts, |this| {
                            this.child(
                                div()
                                    .py_16()
                                    .w_full()
                                    .flex()
                                    .justify_center()
                                    .text_sm()
                                    .text_color(ui_color(0x8a8a8a))
                                    .child("暂无朋友圈数据"),
                            )
                        })
                        .when(can_more, |this| {
                            this.child(
                                div()
                                    .id("sns-load-more")
                                    .aria_label("加载更多朋友圈")
                                    .test_support()
                                    .py_2()
                                    .w_full()
                                    .flex()
                                    .justify_center()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(ui_color(0x576b95))
                                    .child("加载更多")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load_sns_page(false, cx);
                                        cx.notify();
                                    })),
                            )
                        }),
                );
                let viewport = div()
                    .id("sns-viewport")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&sns_scroll)
                    .child(content);
                let viewport =
                    self.smooth_viewport("sns", SmoothTarget::Handle(sns_scroll), viewport, cx);
                div()
                    .id("sns-timeline")
                    .flex_1()
                    .h_full()
                    .flex()
                    .overflow_hidden()
                    .child(div().flex_1().h_full().overflow_hidden().child(viewport))
                    .child(Self::scroll_rail("sns-rail", &self.sns_scroll))
            })
            .into_any_element()
    }

    fn render_finder(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self
            .finder
            .iter()
            .map(|live| {
                let url = if !live.live_url.is_empty() {
                    live.live_url.clone()
                } else {
                    live.profile_url.clone()
                };
                let status = if live.live_status == 1 {
                    ("直播中", 0xf8eded, 0x9a5050)
                } else if live.replay_status == 1 {
                    ("回放", 0xeef1f6, 0x576b95)
                } else {
                    ("已结束", 0xf3f3f3, 0x555b56)
                };
                div()
                    .id(ElementId::Name(format!("finder:{}", live.live_id).into()))
                    .aria_label(live.display_name.clone())
                    .test_support()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .gap_4()
                    .rounded_md()
                    .hover(|style| style.bg(ui_color(0xf5f7f5)))
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(64.))
                            .h(px(64.))
                            .flex_none()
                            .rounded_md()
                            .bg(ui_color(0xe9f8ef))
                            .text_color(ui_color(0x058f48))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("视频号"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(ui_color(0x111827))
                                            .truncate()
                                            .child(live.display_name.clone()),
                                    )
                                    .when(live.charge_flag != 0, |this| {
                                        this.child(
                                            div()
                                                .px_1()
                                                .py(px(1.))
                                                .rounded_sm()
                                                .bg(ui_color(0xfbf1e8))
                                                .text_xs()
                                                .text_color(ui_color(0xa2673e))
                                                .child("付费"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x6b7280))
                                    .line_clamp(2)
                                    .whitespace_normal()
                                    .child(live.description.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x576b95))
                                    .truncate()
                                    .child(format!("# 直播编号 {}", live.live_id)),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px_2()
                            .py_1()
                            .rounded_full()
                            .bg(ui_color(status.1))
                            .text_xs()
                            .text_color(ui_color(status.2))
                            .child(status.0),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if !url.is_empty() {
                            cx.open_url(&url);
                        }
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(ui_color(0xededed))
            .text_color(ui_color(0x191919))
            .child(
                div()
                    .h(px(56.))
                    .px_6()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(ui_color(0xe0e0e0))
                    .child(div().text_lg().child("视频号 / 直播"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui_color(0x8a8a8a))
                            .child(format!("共 {} 条", self.finder.len())),
                    ),
            )
            .child({
                let scroll = self.finder_scroll.clone();
                let viewport = div()
                    .id("finder-viewport")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .p_4()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(980.))
                            .flex()
                            .flex_col()
                            .when(rows.is_empty(), |this| {
                                this.py_16().items_center().child(
                                    div()
                                        .text_sm()
                                        .text_color(ui_color(0x8a8a8a))
                                        .child("暂无直播记录"),
                                )
                            })
                            .children(rows),
                    );
                let viewport =
                    self.smooth_viewport("finder", SmoothTarget::Handle(scroll), viewport, cx);
                div()
                    .id("finder-list")
                    .flex_1()
                    .h_full()
                    .flex()
                    .overflow_hidden()
                    .child(div().flex_1().h_full().overflow_hidden().child(viewport))
                    .child(Self::scroll_rail("finder-rail", &self.finder_scroll))
            })
            .into_any_element()
    }

    fn render_mini_programs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let cards = self
            .mini_programs
            .iter()
            .map(|program| {
                let meta = if program.titles.is_empty() {
                    program.categories.join(" · ")
                } else {
                    program.titles.join(" · ")
                };
                div()
                    .id(ElementId::Name(format!("mini:{}", program.username).into()))
                    .aria_label(program.display_name.clone())
                    .test_support()
                    .w(px(320.))
                    .min_h(px(118.))
                    .rounded_md()
                    .border_1()
                    .border_color(ui_color(0xe5e7eb))
                    .bg(ui_color(0xffffff))
                    .p_3()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .w(px(50.))
                            .h(px(50.))
                            .flex_none()
                            .rounded_md()
                            .bg(ui_color(0xe9f8ef))
                            .text_color(ui_color(0x058f48))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(initial_of(&program.display_name)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .truncate()
                                            .child(program.display_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(ui_color(0x8a8a8a))
                                            .child(program.last_update_text.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x6b7280))
                                    .line_clamp(2)
                                    .whitespace_normal()
                                    .child(if meta.is_empty() {
                                        program.username.clone()
                                    } else {
                                        meta
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x576b95))
                                    .truncate()
                                    .child(if program.app_id.is_empty() {
                                        program.summary.clone()
                                    } else {
                                        format!("AppID {}", program.app_id)
                                    }),
                            ),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(ui_color(0xededed))
            .text_color(ui_color(0x191919))
            .child(
                div()
                    .h(px(56.))
                    .px_6()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(ui_color(0xe0e0e0))
                    .child(div().text_lg().child("小程序"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui_color(0x8a8a8a))
                            .child(format!("共 {} 个", self.mini_programs.len())),
                    ),
            )
            .child({
                let scroll = self.mini_scroll.clone();
                let viewport = div()
                    .id("mini-viewport")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .p_4()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_3()
                            .when(cards.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(ui_color(0x8a8a8a))
                                        .child("暂无小程序数据"),
                                )
                            })
                            .children(cards),
                    );
                let viewport = self.smooth_viewport(
                    "mini-programs",
                    SmoothTarget::Handle(scroll),
                    viewport,
                    cx,
                );
                div()
                    .id("mini-program-list")
                    .flex_1()
                    .h_full()
                    .flex()
                    .overflow_hidden()
                    .child(div().flex_1().h_full().overflow_hidden().child(viewport))
                    .child(Self::scroll_rail("mini-rail", &self.mini_scroll))
            })
            .into_any_element()
    }

    fn render_payments(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self
            .payments
            .iter()
            .map(|payment| {
                let is_red = payment.kind == "red_packet";
                let label = if is_red { "红包" } else { "转账" };
                let color = if is_red { 0xe35b3a } else { 0xf0a638 };
                let session = self.session_display_name(&payment.session_name);
                let session_name = payment.session_name.clone();
                div()
                    .id(ElementId::Name(
                        format!("pay:{}:{}", payment.kind, payment.id).into(),
                    ))
                    .aria_label(format!("{label} {session} {}", payment.status))
                    .test_support()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_md()
                    .hover(|style| style.bg(ui_color(0xf5f7f5)))
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(42.))
                            .h(px(42.))
                            .flex_none()
                            .rounded_md()
                            .bg(ui_color(if is_red { 0xfdf0e9 } else { 0xfbf4e8 }))
                            .text_color(ui_color(color))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .child(format!("{label} · {session}")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x6b7280))
                                    .truncate()
                                    .child(payment.username.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(if is_red { 0xc0764f } else { 0xb07a2c }))
                                    .child(payment.status.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui_color(0x9e9e9e))
                                    .child(payment.create_time_text.clone()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !session_name.is_empty() {
                            this.view = View::Chat;
                            this.select_session(session_name.clone());
                            cx.notify();
                        }
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(ui_color(0xededed))
            .text_color(ui_color(0x191919))
            .child(
                div()
                    .h(px(56.))
                    .px_6()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(ui_color(0xe0e0e0))
                    .child(div().text_lg().child("转账 / 红包"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui_color(0x8a8a8a))
                            .child(format!("共 {} 条", self.payments.len())),
                    ),
            )
            .child({
                let scroll = self.payments_scroll.clone();
                let viewport = div()
                    .id("payments-viewport")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .p_4()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(820.))
                            .flex()
                            .flex_col()
                            .when(rows.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(ui_color(0x8a8a8a))
                                        .child("暂无转账/红包记录"),
                                )
                            })
                            .children(rows),
                    );
                let viewport =
                    self.smooth_viewport("payments", SmoothTarget::Handle(scroll), viewport, cx);
                div()
                    .id("payment-list")
                    .flex_1()
                    .h_full()
                    .flex()
                    .overflow_hidden()
                    .child(div().flex_1().h_full().overflow_hidden().child(viewport))
                    .child(Self::scroll_rail("payments-rail", &self.payments_scroll))
            })
            .into_any_element()
    }

    fn render_search(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let input = self.search_input.as_ref().map(|state| {
            div()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(ui_color(0xe5e7eb))
                .child(Input::new(state).cleanable(true).aria_label("搜索聊天记录"))
                .into_any_element()
        });
        let mut items: Vec<AnyElement> = Vec::new();
        if self.search_note.is_empty() && self.query.is_empty() {
            items.push(
                div()
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(ui_color(0x6b7280))
                    .child("输入关键词开始搜索")
                    .into_any_element(),
            );
        }
        if !self.search_note.is_empty() {
            items.push(
                div()
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(ui_color(0x6b7280))
                    .child(self.search_note.clone())
                    .into_any_element(),
            );
        }
        for hit in &self.hits {
            items.push(self.hit_row(hit, cx));
        }
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(ui_color(0xededed))
            .text_color(ui_color(0x191919))
            .child(
                div()
                    .h(px(44.))
                    .px_4()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(ui_color(0xe5e7eb))
                    .text_sm()
                    .child("内容搜索"),
            )
            .children(input)
            .child({
                let scroll = self.search_scroll.clone();
                let viewport = div()
                    .id("search-viewport")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .child(div().flex().flex_col().children(items));
                let viewport =
                    self.smooth_viewport("search", SmoothTarget::Handle(scroll), viewport, cx);
                div()
                    .id("search-results")
                    .flex_1()
                    .h_full()
                    .flex()
                    .overflow_hidden()
                    .child(div().flex_1().h_full().overflow_hidden().child(viewport))
                    .child(Self::scroll_rail("search-rail", &self.search_scroll))
            })
            .into_any_element()
    }
}

#[derive(Clone)]
enum ContactEntry {
    Header(String),
    Item(ContactItem),
}

fn contact_badge(contact: &ContactItem) -> (&'static str, u32) {
    if contact.is_group {
        ("群聊", 0xdcfce7)
    } else if contact.is_official {
        ("公众号", 0xfef3c7)
    } else if contact.username.contains("@openim") {
        ("企业", 0xffedd5)
    } else if contact.local_type == 0 || contact.local_type == 3 {
        ("陌生人", 0xf3f4f6)
    } else {
        ("朋友", 0xdbeafe)
    }
}

fn session_label(username: &str, display_name: &str, is_group: bool) -> String {
    let display = display_name.trim();
    if is_group
        && (display.is_empty()
            || display.eq_ignore_ascii_case(username)
            || display.ends_with("@chatroom"))
    {
        let id = username.trim_end_matches("@chatroom");
        let short: String = id.chars().take(6).collect();
        format!("群聊 {short}")
    } else if display.is_empty() {
        username.to_string()
    } else {
        display.to_string()
    }
}

fn is_http_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn relative_time(timestamp: i64) -> String {
    if timestamp <= 0 {
        return String::new();
    }
    let now = Local::now().timestamp();
    let diff = (now - timestamp).max(0);
    if diff < 60 {
        "刚刚".to_string()
    } else if diff < 3600 {
        format!("{}分钟前", diff / 60)
    } else if diff < 86_400 {
        format!("{}小时前", diff / 3600)
    } else if diff < 86_400 * 30 {
        format!("{}天前", diff / 86_400)
    } else if diff < 86_400 * 365 {
        format!("{}个月前", diff / (86_400 * 30))
    } else {
        format!("{}年前", diff / (86_400 * 365))
    }
}

impl Render for WeChatApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let desired_theme = if dark_mode() {
            gpui_kit::component::ThemeMode::Dark
        } else {
            gpui_kit::component::ThemeMode::Light
        };
        if gpui_kit::component::Theme::global(cx).mode != desired_theme {
            gpui_kit::component::Theme::change(desired_theme, Some(window), cx);
        }
        if !self.monitor_started {
            self.monitor_started = true;
            self.start_background_monitor(cx);
        }
        if self.store.is_some() {
            self.ensure_search_input(window, cx);
        }
        if self.view == View::Contacts {
            self.ensure_contact_input(window, cx);
        }
        if self.view == View::Moments {
            self.ensure_sns_input(window, cx);
        }
        let content = match self.view {
            View::Chat => self.render_chat(window, cx),
            View::Contacts => self.render_contacts(cx),
            View::Moments => self.render_moments(cx),
            View::Finder => self.render_finder(cx),
            View::MiniPrograms => self.render_mini_programs(cx),
            View::Payments => self.render_payments(cx),
            View::Search => self.render_search(cx),
        };
        let title_bar = gpui_kit::component::TitleBar::new()
            .bg(ui_color(0xf2f2f2))
            .border_color(ui_color(0xe0e0e0))
            .text_color(ui_color(0x4b5563));
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .bg(ui_color(0xededed))
            .text_color(ui_color(0x191919))
            .child(title_bar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.nav_rail(cx))
                    .child(content),
            );
        if self.settings_open {
            root = root.child(self.settings_overlay(cx));
        }
        root
    }
}

fn text_view(text: &str) -> AnyElement {
    div()
        .text_sm()
        .whitespace_normal()
        .child(text.to_string())
        .into_any_element()
}

fn sanitize_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with("<?xml")
        || trimmed.starts_with("<msg")
        || trimmed.starts_with("<appmsg")
        || trimmed.starts_with("<sysmsg")
    {
        "[应用消息]".to_string()
    } else {
        text.to_string()
    }
}

fn selectable_text(text: &str, id: &str, size: f32, color: u32) -> AnyElement {
    gpui_kit::base::SelectableText::new(
        ElementId::Name(format!("select:{id}").into()),
        text.to_string(),
    )
    .text_style(gpui_kit::TextStyleRefinement {
        font_size: Some(px(size).into()),
        line_height: Some(px(size * 1.5).into()),
        color: Some(ui_color(color).into()),
        ..Default::default()
    })
    .into_any_element()
}

type EmojiEntry = (&'static str, &'static str, &'static [u8]);

struct EmojiIndex {
    by_first: HashMap<char, Vec<EmojiEntry>>,
    images: HashMap<&'static str, Arc<Image>>,
    fallback: Arc<Image>,
}

fn emoji_index() -> &'static EmojiIndex {
    static INDEX: OnceLock<EmojiIndex> = OnceLock::new();
    INDEX.get_or_init(|| {
        let files: HashMap<&'static str, &'static [u8]> = EMOJI_FILES.iter().copied().collect();
        let mut by_first: HashMap<char, Vec<EmojiEntry>> = HashMap::new();
        for (token, file) in EMOJI_TABLE {
            let Some(first) = token.chars().next() else {
                continue;
            };
            let Some(data) = files.get(file).copied() else {
                continue;
            };
            by_first.entry(first).or_default().push((token, file, data));
        }
        for list in by_first.values_mut() {
            list.sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
        }
        let images: HashMap<&'static str, Arc<Image>> = files
            .iter()
            .map(|(name, data)| {
                (
                    *name,
                    Arc::new(Image::from_bytes(ImageFormat::Png, data.to_vec())),
                )
            })
            .collect();
        EmojiIndex {
            by_first,
            images,
            fallback: Arc::new(Image::from_bytes(ImageFormat::Png, transparent_png())),
        }
    })
}

fn emoji_match_at(text: &str, offset: usize) -> Option<(&'static str, &'static str)> {
    let rest = text.get(offset..)?;
    let first = rest.chars().next()?;
    let list = emoji_index().by_first.get(&first)?;
    list.iter()
        .find(|(token, _, _)| rest.starts_with(token))
        .map(|(token, file, _)| (*token, *file))
}

fn contains_emoji_token(text: &str) -> bool {
    let mut offset = 0;
    while offset < text.len() {
        if emoji_match_at(text, offset).is_some() {
            return true;
        }
        offset += text[offset..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn push_html_escaped(output: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            '\n' => output.push_str("<br/>"),
            '\r' => {}
            _ => output.push(ch),
        }
    }
}

fn emoji_html(text: &str) -> String {
    emoji_html_sized(text, 20.)
}

fn emoji_html_sized(text: &str, icon: f32) -> String {
    let mut output = String::with_capacity(text.len() + 64);
    let icon = format!("{icon:.0}");
    for line in text.split('\n') {
        output.push_str("<span>");
        if line.is_empty() {
            output.push(' ');
        } else {
            let mut plain_start = 0;
            let mut offset = 0;
            let mut has_text = false;
            while offset < line.len() {
                if let Some((token, file)) = emoji_match_at(line, offset) {
                    push_html_escaped(&mut output, &line[plain_start..offset]);
                    output.push_str("<img src=\"wce-emoji:");
                    output.push_str(file);
                    output.push_str("\" width=\"");
                    output.push_str(&icon);
                    output.push_str("\" height=\"");
                    output.push_str(&icon);
                    output.push_str("\" />");
                    offset += token.len();
                    plain_start = offset;
                } else {
                    has_text = true;
                    offset += line[offset..].chars().next().map_or(1, char::len_utf8);
                }
            }
            push_html_escaped(&mut output, &line[plain_start..]);
            if !has_text {
                output.push_str("&#8203;");
            }
        }
        output.push_str("</span>");
    }
    output
}

fn emoji_image_source(uri: &gpui_kit::SharedUri) -> gpui_kit::ImageSource {
    let value = uri.to_string();
    let name = value.strip_prefix("wce-emoji:").unwrap_or(&value);
    let index = emoji_index();
    match index.images.get(name) {
        Some(image) => gpui_kit::ImageSource::Image(image.clone()),
        None => gpui_kit::ImageSource::Image(index.fallback.clone()),
    }
}

fn transparent_png() -> Vec<u8> {
    const BYTES: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    BYTES.to_vec()
}

fn rich_text(text: &str, id: &str, size: f32, color: u32) -> AnyElement {
    if !contains_emoji_token(text) {
        return selectable_text(text, id, size, color);
    }
    gpui_kit::base::TextView::html(
        ElementId::Name(format!("rich:{id}").into()),
        emoji_html(text),
    )
    .text_size(px(size))
    .line_height(px(size * 1.5))
    .text_color(ui_color(color))
    .selectable(true)
    .style(
        gpui_kit::base::TextViewStyle::default()
            .with_foreground(ui_color(color).into())
            .with_paragraph_gap(gpui_kit::rems(0.)),
    )
    .image_source(emoji_image_source)
    .into_any_element()
}

fn rich_text_preview(text: &str, id: &str, size: f32, color: u32) -> AnyElement {
    if !contains_emoji_token(text) {
        return div().truncate().child(text.to_string()).into_any_element();
    }
    gpui_kit::base::TextView::html(
        ElementId::Name(format!("preview:{id}").into()),
        emoji_html_sized(text, size * 1.2),
    )
    .text_size(px(size))
    .line_height(px(size * 1.5))
    .text_color(ui_color(color))
    .selectable(false)
    .max_lines(1)
    .style(
        gpui_kit::base::TextViewStyle::default()
            .with_foreground(ui_color(color).into())
            .with_paragraph_gap(gpui_kit::rems(0.)),
    )
    .image_source(emoji_image_source)
    .into_any_element()
}

fn message_copy_text(message: &Message) -> String {
    match &message.body {
        Some(MessageBody::Text { content }) | Some(MessageBody::System { content }) => {
            content.clone()
        }
        Some(MessageBody::Link {
            content,
            title,
            url,
            ..
        }) => {
            if url.trim().is_empty() {
                first_non_empty_text([title.as_str(), content.as_str()])
            } else if title.trim().is_empty() {
                url.clone()
            } else {
                format!("{}\n{}", title.trim(), url.trim())
            }
        }
        Some(MessageBody::ChatHistory { content, title, .. }) => {
            format!("{}\n{}", title.trim(), content.trim())
                .trim()
                .to_string()
        }
        Some(MessageBody::File {
            content,
            title,
            size,
            ..
        }) => {
            let name = first_non_empty_text([title.as_str(), content.as_str()]);
            if size.trim().is_empty() {
                name
            } else {
                format!("{name} {size}")
            }
        }
        Some(MessageBody::Quote {
            content,
            quote_title,
            quote_content,
            ..
        }) => format!(
            "{}\n{}",
            content.trim(),
            first_non_empty_text([quote_title.as_str(), quote_content.as_str()])
        )
        .trim()
        .to_string(),
        Some(MessageBody::Transfer {
            title,
            amount,
            sender_title,
            ..
        }) => first_non_empty_text([amount.as_str(), title.as_str(), sender_title.as_str()]),
        Some(MessageBody::RedPacket { title, .. }) => {
            first_non_empty_text([title.as_str(), "红包"])
        }
        Some(MessageBody::Location {
            location_poiname,
            location_label,
            ..
        }) => first_non_empty_text([location_poiname.as_str(), location_label.as_str()]),
        Some(MessageBody::Voip { content, .. }) => content.clone(),
        None => match message.kind {
            MessageKind::Image => "[图片]".to_string(),
            MessageKind::Video => "[视频]".to_string(),
            MessageKind::Voice => "[语音]".to_string(),
            MessageKind::Emoji => "[表情]".to_string(),
            _ => message.content.clone(),
        },
    }
}

fn first_non_empty_text<const N: usize>(values: [&str; N]) -> String {
    values
        .into_iter()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn message_aria(message: &Message) -> String {
    let (text, detail) = message_lines(message);
    match detail {
        Some(detail) if !detail.is_empty() => format!("{text} {detail}"),
        _ => text,
    }
}

fn message_lines(message: &Message) -> (String, Option<String>) {
    match &message.body {
        Some(MessageBody::Text { content }) | Some(MessageBody::System { content }) => {
            (content.clone(), None)
        }
        Some(MessageBody::Link {
            content,
            title,
            url,
            ..
        }) => {
            let detail = if !title.is_empty() { title } else { url };
            (content.clone(), Some(detail.clone()))
        }
        Some(MessageBody::ChatHistory { content, title, .. }) => {
            (content.clone(), Some(title.clone()))
        }
        Some(MessageBody::File { content, .. }) => (content.clone(), None),
        Some(MessageBody::Quote {
            content,
            quote_title,
            quote_content,
            ..
        }) => {
            let detail = if quote_title.is_empty() {
                quote_content.clone()
            } else {
                format!("{quote_title}: {quote_content}")
            };
            (content.clone(), Some(detail))
        }
        Some(MessageBody::Transfer {
            content,
            title,
            amount,
            ..
        }) => (
            content.clone(),
            Some(format!("{title} {amount}").trim().to_string()),
        ),
        Some(MessageBody::RedPacket { content, title, .. }) => {
            (content.clone(), Some(title.clone()))
        }
        Some(MessageBody::Location {
            content,
            location_label,
            ..
        }) => (content.clone(), Some(location_label.clone())),
        Some(MessageBody::Voip { content, .. }) => (content.clone(), None),
        None => match message.kind {
            MessageKind::Image => ("[图片]".to_string(), None),
            MessageKind::Voice => ("[语音]".to_string(), None),
            MessageKind::Video => ("[视频]".to_string(), None),
            MessageKind::Emoji => ("[表情]".to_string(), None),
            MessageKind::Voip => ("[通话]".to_string(), None),
            _ => (message.content.clone(), None),
        },
    }
}

fn hit_text(hit: &SearchHit) -> String {
    const KEYS: [&str; 4] = ["content", "title", "quoteContent", "locationLabel"];
    for key in KEYS {
        if let Some(value) = hit.payload.get(key).and_then(|value| value.as_str()) {
            let text = value.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    hit.render_type.clone()
}

fn format_clock(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn day_key(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| format!("ts:{timestamp}"))
}

fn format_day_label(timestamp: i64) -> String {
    let Some(value) = Local.timestamp_opt(timestamp, 0).single() else {
        return String::new();
    };
    let today = Local::now().date_naive();
    let date = value.date_naive();
    let diff = (today - date).num_days();
    const WEEKDAYS: [&str; 7] = [
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
        "星期日",
    ];
    match diff {
        0 => "今天".to_string(),
        1 => "昨天".to_string(),
        2..=6 => WEEKDAYS[value.weekday().num_days_from_monday() as usize].to_string(),
        _ if value.year() == today.year() => {
            format!("{}月{}日", value.month(), value.day())
        }
        _ => format!("{}年{}月{}日", value.year(), value.month(), value.day()),
    }
}

fn has_database_file(path: &Path) -> bool {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let path = entry.path();
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("db"))
        })
}

fn initial_of(name: &str) -> String {
    name.chars()
        .find(|value| !value.is_whitespace())
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn avatar_color(username: &str) -> u32 {
    const COLORS: [u32; 5] = [0x2c5a7a, 0x2f6f4e, 0x5a4a8a, 0x7a5a2c, 0x6a2f5a];
    let hash = username.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    COLORS[hash as usize % COLORS.len()]
}

fn message_media_key(message: &Message) -> String {
    format!("{}:{}", message.shard, message.local_id)
}

fn image_caption(message: &Message) -> String {
    let sender = if message.sender_display_name.trim().is_empty() {
        if message.is_sent {
            "我"
        } else {
            message.sender_username.as_str()
        }
    } else {
        message.sender_display_name.as_str()
    };
    let time = format_clock(message.create_time);
    if time.is_empty() {
        sender.to_string()
    } else {
        format!("{sender} · {time}")
    }
}

fn xml_attr_value<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    let index = content.find(name)?;
    let rest = &content[index + name.len()..];
    let equals = rest.find('=')?;
    let rest = rest[equals + 1..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    Some(&rest[..end])
}

fn media_dimensions(message: &Message) -> Option<(f32, f32)> {
    for (width_attr, height_attr) in [
        ("cdnthumbwidth", "cdnthumbheight"),
        ("cdnmidwidth", "cdnmidheight"),
        ("cdnhdwidth", "cdnhdheight"),
        ("width", "height"),
    ] {
        let width = xml_attr_value(&message.content, width_attr)
            .and_then(|value| value.parse::<f32>().ok());
        let height = xml_attr_value(&message.content, height_attr)
            .and_then(|value| value.parse::<f32>().ok());
        if let (Some(width), Some(height)) = (width, height)
            && width > 0.
            && height > 0.
        {
            return Some((width, height));
        }
    }
    None
}

fn media_display_size(message: &Message) -> (f32, f32) {
    let (max_w, max_h) = if matches!(message.kind, MessageKind::Video) {
        (240., 180.)
    } else {
        (240., 240.)
    };
    match media_dimensions(message) {
        Some((width, height)) => {
            let scale = (max_w / width).min(max_h / height).min(1.0);
            (
                (width * scale).clamp(40., max_w),
                (height * scale).clamp(40., max_h),
            )
        }
        None => {
            if matches!(message.kind, MessageKind::Video) {
                (220., 160.)
            } else {
                (200., 160.)
            }
        }
    }
}

fn emoji_box_size(reference: Option<&wechat_data::EmojiReference>) -> (f32, f32) {
    let (width, height) = reference
        .map(|reference| (reference.width as f32, reference.height as f32))
        .unwrap_or((0., 0.));
    if width > 0. && height > 0. {
        let scale = (120.0 / width).min(150.0 / height);
        (
            (width * scale).clamp(60., 120.),
            (height * scale).clamp(60., 150.),
        )
    } else {
        (120., 120.)
    }
}

fn decode_image_bytes(data: &[u8], media_type: &str) -> Option<Arc<Image>> {
    let format = match media_type {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        "image/bmp" => ImageFormat::Bmp,
        _ => return None,
    };
    Some(Arc::new(Image::from_bytes(format, data.to_vec())))
}

fn decode_avatar_image(avatar: &AvatarImage) -> Option<Arc<Image>> {
    decode_image_bytes(&avatar.data, &avatar.media_type)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, App, Bounds, ElementId, Point, ScrollDelta, SharedString, TestAppContext,
        Window, WindowBounds, WindowOptions, size,
    };
    use tempfile::TempDir;

    use super::*;

    fn fixture_account() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../wechat-data/tests/fixtures/accounts/wxid_self0001")
    }

    fn copy_dir(source: &Path, destination: &Path) {
        std::fs::create_dir_all(destination).expect("create dir");
        for entry in std::fs::read_dir(source).expect("read dir").flatten() {
            let target = destination.join(entry.file_name());
            if entry.file_type().expect("file type").is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).expect("copy file");
            }
        }
    }

    fn fixture_copy() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let account = temp.path().join("wxid_self0001");
        copy_dir(&fixture_account(), &account);
        (temp, account)
    }

    fn open_app(
        cx: &mut TestAppContext,
        account_path: PathBuf,
    ) -> (AnyWindowHandle, Entity<WeChatApp>) {
        cx.update(gpui_kit::init);
        let view = cx.new(|_| WeChatApp::new(Some(account_path)));
        let handle = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(900.), px(600.)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, _| view.clone(),
            )
            .expect("open window")
            .0
        });
        (handle, view)
    }

    fn session_id(username: &str) -> ElementId {
        ElementId::Name(SharedString::from(format!("session:{username}")))
    }

    fn reveal_message(
        view: &Entity<WeChatApp>,
        window: &mut Window,
        cx: &mut App,
        shard: &str,
        local_id: i64,
    ) -> bool {
        let index = {
            let app = view.read(cx);
            app.message_items.iter().position(|item| match item {
                MessageListItem::Message(index) => {
                    let message = &app.messages[*index];
                    message.shard == shard && message.local_id == local_id
                }
                _ => false,
            })
        };
        let Some(index) = index else {
            return false;
        };
        view.update(cx, |app, _| {
            app.messages_list.scroll_to_reveal_item(index);
        });
        window.render_frame(cx);
        true
    }

    fn nav_id(label: &str) -> ElementId {
        ElementId::Name(SharedString::from(format!("nav:{label}")))
    }

    fn contact_id(username: &str) -> ElementId {
        ElementId::Name(SharedString::from(format!("contact:{username}")))
    }

    fn account_id(account: &str) -> ElementId {
        ElementId::Name(SharedString::from(format!("account:{account}")))
    }

    fn hit_id(username: &str, db_stem: &str, local_id: i64) -> ElementId {
        ElementId::Name(SharedString::from(format!(
            "search:{username}:{db_stem}:{local_id}"
        )))
    }

    fn message_id(shard: &str, local_id: i64) -> ElementId {
        ElementId::Name(SharedString::from(format!("message:{shard}:{local_id}")))
    }

    fn message_list_id() -> ElementId {
        ElementId::Name(SharedString::from("message-list"))
    }

    fn day_id(key: &str) -> ElementId {
        ElementId::Name(SharedString::from(format!("day:{key}")))
    }

    #[gpui_kit::test]
    fn renders_sessions_and_messages_from_fixture(cx: &mut TestAppContext) {
        let (_temp, account) = fixture_copy();
        let (handle, view) = open_app(cx, account);

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let row = window.find(session_id("wxid_friend01"));
            assert_eq!(row.label(), Some("张三"));

            window.click(session_id("wxid_friend01"), cx);
            window.render_frame(cx);
            let (top, count) = {
                let app = view.read(cx);
                (
                    app.messages_list.logical_scroll_top(),
                    app.message_items.len(),
                )
            };
            assert!(count > 0, "fixture message list should have items");
            assert!(
                top.item_ix > 0,
                "selecting a session should land at the newest message (top {top:?})"
            );

            view.update(cx, |app, _| {
                app.messages_list.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: px(0.),
                });
            });
            window.render_frame(cx);
            let first = window.find(message_id("message_0.db", 1));
            assert_eq!(first.label(), Some("第一条消息"));
            window.right_click(message_id("message_0.db", 1), cx);
            let copied = cx.read_from_clipboard().and_then(|item| item.text());
            assert_eq!(copied.as_deref(), Some("第一条消息"));

            let expected_day = format_day_label(1_700_000_100);
            let day = window.find(day_id(&day_key(1_700_000_100)));
            assert_eq!(day.label(), Some(expected_day.as_str()));

            let merged_index = {
                let app = view.read(cx);
                app.message_items.iter().position(|item| match item {
                    MessageListItem::Message(index) => {
                        let message = &app.messages[*index];
                        message.shard == "message_1.db" && message.local_id == 2
                    }
                    _ => false,
                })
            };
            if let Some(index) = merged_index {
                view.update(cx, |app, _| {
                    app.messages_list.scroll_to_reveal_item(index);
                });
                window.render_frame(cx);
                assert!(
                    window.try_find(message_id("message_1.db", 2)).is_some(),
                    "merged shard message should be rendered"
                );
            }
        })
        .expect("update window");
    }

    #[gpui_kit::test]
    fn navigation_switches_between_sections(cx: &mut TestAppContext) {
        let (_temp, account) = fixture_copy();
        let (handle, view) = open_app(cx, account);

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(nav_id("联系人"), cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).view, View::Contacts);
            assert_eq!(
                window.find(contact_id("wxid_friend01")).label(),
                Some("张三")
            );

            for (label, expected) in [
                ("朋友圈", View::Moments),
                ("视频号", View::Finder),
                ("小程序", View::MiniPrograms),
                ("收付款", View::Payments),
                ("搜索", View::Search),
                ("聊天", View::Chat),
            ] {
                window.click(nav_id(label), cx);
                window.render_frame(cx);
                assert_eq!(view.read(cx).view, expected, "nav {label}");
            }
        })
        .expect("update window");
    }

    #[gpui_kit::test]
    fn renders_text_quote_link_media_and_system_rows(cx: &mut TestAppContext) {
        let (_temp, account) = fixture_copy();
        let (handle, view) = open_app(cx, account);

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(session_id("wxid_friend01"), cx);
            window.render_frame(cx);
            for local_id in [1_i64, 5, 9, 10, 12, 13, 15, 16, 17, 19] {
                assert!(
                    reveal_message(&view, window, cx, "message_0.db", local_id),
                    "message {local_id} should exist"
                );
                assert!(
                    window
                        .try_find(message_id("message_0.db", local_id))
                        .is_some(),
                    "message {local_id} should render"
                );
            }
            reveal_message(&view, window, cx, "message_0.db", 12);
            let system = window.find(message_id("message_0.db", 12));
            assert!(
                system.label().is_some_and(|label| !label.is_empty()),
                "system message should keep an accessible label"
            );
        })
        .expect("update window");
    }

    #[gpui_kit::test]
    fn searches_messages_and_opens_conversation(cx: &mut TestAppContext) {
        let (_temp, account) = fixture_copy();
        build_search_index(&account).expect("build search index");
        let (handle, view) = open_app(cx, account);

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |app, cx| app.run_search("第一条", cx));
            window.render_frame(cx);

            let hit = window.find(hit_id("wxid_friend01", "message_0", 1));
            assert_eq!(hit.label(), Some("张三: 第一条消息"));

            window.click(hit_id("wxid_friend01", "message_0", 1), cx);
            window.render_frame(cx);
            reveal_message(&view, window, cx, "message_0.db", 1);
            assert_eq!(
                window.find(message_id("message_0.db", 1)).label(),
                Some("第一条消息")
            );
        })
        .expect("update window");
    }

    #[gpui_kit::test]
    fn builds_search_index_on_demand(cx: &mut TestAppContext) {
        let (_temp, account) = fixture_copy();
        assert!(!chat_search_index_path(&account).exists());
        let (handle, view) = open_app(cx, account);

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |app, cx| app.run_search("第一条", cx));
            window.render_frame(cx);
            assert!(
                window
                    .try_find(hit_id("wxid_friend01", "message_0", 1))
                    .is_none()
            );
        })
        .expect("update window");

        cx.run_until_parked();

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find(hit_id("wxid_friend01", "message_0", 1))
                    .is_some(),
                "pending search should run once the index is built"
            );
        })
        .expect("update window");
    }

    #[gpui_kit::test]
    fn account_picker_lists_and_opens_accounts(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("wechat_files");
        copy_dir(&fixture_account(), &root.join("wxid_first"));
        copy_dir(&fixture_account(), &root.join("wxid_second"));
        let (handle, _view) = open_app(cx, root);

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(account_id("wxid_first")).is_some());
            assert!(window.try_find(account_id("wxid_second")).is_some());

            window.click(account_id("wxid_first"), cx);
            window.render_frame(cx);
            assert!(
                window.try_find(session_id("wxid_friend01")).is_some(),
                "selecting an account should load its sessions"
            );
        })
        .expect("update window");
    }

    #[gpui_kit::test]
    #[ignore]
    fn realtime_mode_renders_from_db_storage(cx: &mut TestAppContext) {
        if std::env::var("WCE_SELF_BROKER").is_err()
            || std::env::var("WECHAT_REALTIME_ROOT").is_err()
        {
            return;
        }
        cx.update(gpui_kit::init);
        let view = cx.new(|_| WeChatApp::new(None));
        let handle = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(900.), px(600.)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, _| view.clone(),
            )
            .expect("open window")
            .0
        });

        let mut search_needle = String::new();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let sessions = view.read(cx).sessions.clone();
            assert!(
                !sessions.is_empty(),
                "realtime sessions should not be empty"
            );
            assert!(
                !view.read(cx).avatars.is_empty(),
                "realtime avatars should load from head_image.db"
            );
            {
                let app = view.read(cx);
                let self_names = app
                    .store
                    .as_ref()
                    .map(Store::self_names)
                    .unwrap_or_default();
                assert!(
                    self_names
                        .iter()
                        .any(|name| app.avatars.contains_key(name)),
                    "current user avatar should be loaded ({self_names:?})"
                );
            }
            let direct = sessions
                .iter()
                .find(|session| !session.username.ends_with("@chatroom"))
                .or_else(|| sessions.first())
                .cloned()
                .expect("at least one session");
            view.update(cx, |app, cx| {
                app.select_session(direct.username.clone());
                cx.notify();
            });
            window.render_frame(cx);
            {
                let app = view.read(cx);
                assert!(
                    !app.messages.is_empty(),
                    "realtime messages should not be empty"
                );
                assert!(
                    !app.pending_scroll_bottom,
                    "initial render should consume the scroll-to-bottom request"
                );
                if let Some(received) = app.messages.iter().find(|message| !message.is_sent) {
                    assert!(
                        app.avatars.contains_key(&received.sender_username),
                        "sender avatar should be loaded for {}",
                        received.sender_username
                    );
                }
                let senders: HashSet<&str> = app
                    .messages
                    .iter()
                    .map(|message| message.sender_username.as_str())
                    .filter(|username| !username.is_empty())
                    .collect();
                eprintln!(
                    "session={} messages={} senders={} sender_avatars={}",
                    direct.username,
                    app.messages.len(),
                    senders.len(),
                    senders
                        .iter()
                        .filter(|username| app.avatars.contains_key(**username))
                        .count()
                );
            }

            let mut paged = None;
            for session in sessions.iter().take(10) {
                if session.username == direct.username {
                    continue;
                }
                view.update(cx, |app, _| app.select_session(session.username.clone()));
                let app = view.read(cx);
                if app.has_more_older {
                    paged = Some((session.username.clone(), app.messages.len()));
                    break;
                }
            }
            if let Some((username, before)) = paged {
                window.render_frame(cx);
                {
                    let app = view.read(cx);
                    let senders: HashSet<&str> = app
                        .messages
                        .iter()
                        .map(|message| message.sender_username.as_str())
                        .filter(|sender| !sender.is_empty())
                        .collect();
                    eprintln!(
                        "group={username} messages={} senders={} sender_avatars={}",
                        app.messages.len(),
                        senders.len(),
                        senders
                            .iter()
                            .filter(|sender| app.avatars.contains_key(**sender))
                            .count()
                    );
                    app.messages_list.scroll_to(ListOffset {
                        item_ix: 0,
                        offset_in_item: px(0.),
                    });
                }
                window.render_frame(cx);
                window.scroll(
                    message_list_id(),
                    ScrollDelta::Lines(Point { x: 0., y: -1. }),
                    cx,
                );
                window.scroll(
                    message_list_id(),
                    ScrollDelta::Lines(Point { x: 0., y: 1. }),
                    cx,
                );
                let after = view.read(cx).messages.len();
                assert!(
                    after > before,
                    "scrolling up should auto-load older messages for {username} ({before} -> {after})"
                );
                eprintln!("wheel-loaded older page for {username}: {before} -> {after}");
            }

            for session in sessions.iter().take(30) {
                view.update(cx, |app, _| {
                    app.select_session(session.username.clone());
                    app.load_message_images();
                });
                let (image_messages, loaded_images) = {
                    let app = view.read(cx);
                    let image_keys: HashSet<String> = app
                        .messages
                        .iter()
                        .filter(|message| matches!(message.kind, MessageKind::Image))
                        .map(message_media_key)
                        .collect();
                    let loaded_images = image_keys
                        .iter()
                        .filter(|key| app.message_images.get(*key).is_some_and(Option::is_some))
                        .count();
                    (image_keys.len(), loaded_images)
                };
                if image_messages > 0 {
                    eprintln!(
                        "images session={} messages={} loaded={}",
                        session.username, image_messages, loaded_images
                    );
                    assert!(
                        loaded_images > 0,
                        "image message thumbnails should decrypt from msg/attach"
                    );
                    break;
                }
            }

            let needle = view
                .read(cx)
                .messages
                .iter()
                .find_map(|message| {
                    let text = message.content.trim();
                    let text = text
                        .split_once('\n')
                        .map(|(_, rest)| rest)
                        .unwrap_or(text);
                    let text = text.trim();
                    (!text.starts_with('<')
                        && !text.starts_with('[')
                        && text.chars().count() >= 2)
                        .then(|| text.chars().take(2).collect::<String>())
                })
                .unwrap_or_else(|| "的".to_string());
            view.update(cx, |app, cx| app.switch_view(View::Contacts, cx));
            window.render_frame(cx);
            let contacts = view.read(cx).contacts.len();
            assert!(contacts > 0, "contacts page should load contact.db");
            eprintln!("contacts={contacts}");

            view.update(cx, |app, cx| app.switch_view(View::Moments, cx));
            window.render_frame(cx);
            let (posts, comments, sns_images) = {
                let app = view.read(cx);
                (
                    app.sns_posts.len(),
                    app.sns_posts.iter().map(|post| post.comments.len()).sum::<usize>(),
                    app.sns_images
                        .values()
                        .filter(|image| image.is_some())
                        .count(),
                )
            };
            assert!(posts > 0, "moments page should load SnsTimeLine");
            eprintln!("sns posts={posts} comments={comments} images={sns_images}");

            view.update(cx, |app, cx| app.switch_view(View::Finder, cx));
            window.render_frame(cx);
            eprintln!("finder={}", view.read(cx).finder.len());

            view.update(cx, |app, cx| app.switch_view(View::MiniPrograms, cx));
            window.render_frame(cx);
            eprintln!("mini_programs={}", view.read(cx).mini_programs.len());

            view.update(cx, |app, cx| app.switch_view(View::Payments, cx));
            window.render_frame(cx);
            eprintln!("payments={}", view.read(cx).payments.len());

            view.update(cx, |app, cx| {
                app.switch_view(View::Search, cx);
                app.run_search(&needle, cx);
            });
            window.render_frame(cx);
            search_needle = needle.clone();
        })
        .expect("update window");

        cx.background_executor.run_until_parked();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let hits = view.read(cx).hits.len();
            assert!(hits > 0, "realtime search should find '{search_needle}'");
            eprintln!("search '{search_needle}' hits={hits}");
            let (emoji_total, emoji_loaded) = {
                let app = view.read(cx);
                let total = app
                    .messages
                    .iter()
                    .filter(|message| matches!(message.kind, MessageKind::Emoji))
                    .count();
                let loaded = app
                    .messages
                    .iter()
                    .filter(|message| matches!(message.kind, MessageKind::Emoji))
                    .filter(|message| {
                        app.message_images
                            .get(&message_media_key(message))
                            .is_some_and(Option::is_some)
                    })
                    .count();
                (total, loaded)
            };
            eprintln!("emoji messages={emoji_total} loaded={emoji_loaded}");
        })
        .expect("update window");
    }

    #[test]
    fn replaces_text_emoji_tokens_with_images() {
        assert!(contains_emoji_token("你好[惊讶]"));
        assert!(contains_emoji_token("你好[偷笑]"));
        assert!(contains_emoji_token("[捂脸][捂脸][捂脸][捂脸][捂脸]"));
        assert_eq!(
            emoji_html("[捂脸][捂脸][捂脸][捂脸][捂脸]")
                .matches("<img")
                .count(),
            5
        );
        assert!(
            emoji_html("[捂脸][捂脸]").contains("&#8203;"),
            "emoji-only lines need a text node for inline layout"
        );
        assert!(!contains_emoji_token("普通文本"));
        let html = emoji_html("你好[偷笑]再见");
        assert!(html.starts_with("<span>你好"));
        assert!(html.contains("<img src=\"wce-emoji:"));
        assert!(html.ends_with("再见</span>"));
        assert!(emoji_html("第一行[偷笑]\n第二行").contains("</span><span>第二行</span>"));
        let file = html
            .split("<img src=\"wce-emoji:")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("emoji file");
        assert!(
            emoji_index().images.contains_key(file),
            "emoji asset should be embedded: {file}"
        );
    }

    struct RichTextHost;

    impl Render for RichTextHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(320.))
                .flex()
                .flex_col()
                .child(rich_text("你好[惊讶]再见", "inline-emoji", 14., 0x1f1f1f))
                .child(rich_text(
                    "[捂脸][捂脸][捂脸][捂脸][捂脸]",
                    "inline-emoji-five",
                    14.,
                    0x1f1f1f,
                ))
        }
    }

    #[gpui_kit::test]
    fn renders_inline_emoji_text(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| RichTextHost)
            })
            .expect("open window")
            .0
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
        })
        .expect("render inline emoji text");
    }

    #[gpui_kit::test]
    fn opens_image_preview_window(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let view = cx.new(|_| WeChatApp::blank());
        let handle = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, _| view.clone())
                .expect("open window")
                .0
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let image = Arc::new(Image::from_bytes(ImageFormat::Png, transparent_png()));
            view.update(cx, |app, cx| {
                app.open_image_viewer(image, "test image".to_string(), cx);
            });
        })
        .expect("open image preview");
        cx.run_until_parked();
    }

    #[test]
    fn original_nav_icons_parse_and_render() {
        let renderer = gpui_kit::SvgRenderer::new(Arc::new(()));
        for (name, data) in [
            ("chat", ICON_CHAT),
            ("moments", ICON_MOMENTS),
            ("contacts", ICON_CONTACTS),
            ("finder", ICON_FINDER),
            ("mini", ICON_MINI),
            ("payments", ICON_PAYMENTS),
            ("search", ICON_SEARCH),
        ] {
            let parsed = renderer.parse_svg(data).expect("nav svg should parse");
            let image = renderer
                .render_parsed(&parsed, 20.0)
                .unwrap_or_else(|error| panic!("{name} svg should render: {error}"));
            assert!(
                image.as_bytes(0).is_some(),
                "{name} svg should produce pixels"
            );
        }
    }

    #[gpui_kit::test]
    fn applies_user_settings_and_opens_export_dir(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let temp = tempfile::tempdir().expect("tempdir");
        let account = temp.path().join("databases").join("wxid_self0001");
        copy_dir(&fixture_account(), &account);
        let view = cx.new(|_| WeChatApp::blank());
        let settings = AppSettings {
            account_path: Some(temp.path().to_path_buf()),
            storage_dir: None,
            media_root: None,
            key_store: None,
            broker: None,
            remote_cdn: false,
            theme: "light".to_string(),
            minimize_to_tray: true,
        };
        cx.update(|cx| view.update(cx, |app, _| app.apply_settings(settings, false)));
        cx.update(|cx| {
            let app = view.read(cx);
            assert!(
                app.store.is_some(),
                "user settings should open the data dir"
            );
            assert!(
                !app.sessions.is_empty(),
                "settings import should list sessions"
            );
            assert!(!app.settings_open, "settings should close after success");
        });
    }

    #[gpui_kit::test]
    fn renders_settings_and_setup_overlay(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let view = cx.new(|_| {
            let mut app = WeChatApp::blank();
            app.settings_open = true;
            app.setup_mode = true;
            app
        });
        let handle = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(900.), px(640.)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, _| view.clone(),
            )
            .expect("open window")
            .0
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("settings-overlay").is_some());
            assert!(window.try_find("settings-save").is_some());
        })
        .expect("render settings overlay");
    }

    #[gpui_kit::test]
    fn opens_original_export_output_root(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("databases").join("wxid_self0001");
        copy_dir(&fixture_account(), &target);
        let view = cx.new(|_| WeChatApp::new(Some(temp.path().to_path_buf())));
        cx.update(|cx| {
            let app = view.read(cx);
            assert!(app.store.is_some(), "export output root should auto-open");
            assert!(
                !app.sessions.is_empty(),
                "exported account should list sessions"
            );
        });
    }

    #[gpui_kit::test]
    #[ignore]
    fn exported_output_loads_resource_media(cx: &mut TestAppContext) {
        let Ok(root) = std::env::var("WCE_EXPORT_ROOT") else {
            return;
        };
        cx.update(gpui_kit::init);
        let view = cx.new(|_| WeChatApp::new(Some(PathBuf::from(root))));
        cx.update(|cx| {
            let usernames: Vec<String> = {
                let app = view.read(cx);
                assert!(app.store.is_some(), "export output root should open");
                app.sessions
                    .iter()
                    .map(|s| s.username.clone())
                    .take(30)
                    .collect()
            };
            let mut picked = None;
            for username in usernames {
                view.update(cx, |app, _| app.select_session(username.clone()));
                let app = view.read(cx);
                if app
                    .messages
                    .iter()
                    .any(|message| matches!(message.kind, MessageKind::Image))
                {
                    picked = Some(username);
                    break;
                }
            }
            let picked = picked.expect("a session with image messages");
            view.update(cx, |app, cx| {
                app.load_message_images();
                app.load_message_emojis(cx);
            });
            let app = view.read(cx);
            let loaded = app
                .messages
                .iter()
                .filter(|message| matches!(message.kind, MessageKind::Image))
                .filter(|message| {
                    app.message_images
                        .get(&message_media_key(message))
                        .is_some_and(Option::is_some)
                })
                .count();
            assert!(
                loaded > 0,
                "exported resource media should load for {picked}"
            );
            let emoji_total = app
                .messages
                .iter()
                .filter(|message| matches!(message.kind, MessageKind::Emoji))
                .count();
            let emoji_loaded = app
                .messages
                .iter()
                .filter(|message| matches!(message.kind, MessageKind::Emoji))
                .filter(|message| {
                    app.message_images
                        .get(&message_media_key(message))
                        .is_some_and(Option::is_some)
                })
                .count();
            if emoji_total > 0 {
                assert!(
                    emoji_loaded > 0,
                    "exported emoji media should load for {picked}"
                );
            }
            eprintln!(
                "exported media session={picked} images={loaded} emoji={emoji_loaded}/{emoji_total}"
            );
        });
    }
}
