#![forbid(unsafe_code)]

pub mod account;
pub mod contact;
pub mod contacts;
pub mod decode;
pub mod error;
pub mod general;
pub mod group;
pub mod key_scan;
pub mod key_store;
pub mod media_index;
pub mod message;
pub mod message_body;
pub mod message_media;
pub mod model;
pub mod realtime;
pub mod realtime_account;
pub mod remote_media;
pub mod search;
pub mod session;
pub mod sns;
pub mod sqlite;
pub mod time;

pub use account::{discover_accounts, AccountStore};
pub use error::{DataError, Result};
pub use group::load_group_nickname_map;
pub use general::{
    parse_finder_lives, parse_mini_programs, parse_payments, query_rows, FinderLive, MiniProgram,
    PaymentItem,
};
pub use key_scan::{
    ImageKeyMatch, RecoveredKey, recover_image_key_for_account, recover_key_for_db_storage,
};
pub use key_store::{
    find_image_keys_for_db_storage, find_key_for_db_storage, find_key_for_db_storage_with_account,
    load_key_for_db_storage, ImageKeys, KeyStore,
};
pub use media_index::{
    extract_md5_from_blob, extract_md5_from_packed_info, extract_voice_transcript_from_packed_info,
    iter_md5_candidates_from_name, MediaIndex,
};
pub use message::MessageQuery;
pub use message_body::{parse_emoji_reference, parse_message_body, EmojiReference, MessageBody};
pub use message_media::{
    cache_media_bytes, extract_image_md5, find_resource_image, load_emoji_image,
    load_message_image, load_message_image_cached, load_message_image_original, load_sns_image,
    set_media_root, MessageImage,
};
pub use model::{
    AccountInfo, AvatarImage, Contact, ContactItem, Message, MessageCursor, MessageKind,
    MessagePage, Session,
};
pub use realtime::{RealtimeDatabase, RealtimeHost, RealtimePage};
pub use realtime_account::RealtimeAccountStore;
pub use remote_media::{
    fetch_emoticon_media, fetch_tencent_media, is_allowed_tencent_host, normalize_tencent_url,
    RemoteMedia,
};
pub use search::{
    build_fts_query, build_search_index, chat_search_index_path, refresh_search_index,
    search_index, SearchBuildStats, SearchHit, SearchPage, SearchQuery, SearchRefreshStats,
};
pub use sns::{decode_sns_content, parse_sns_post, SnsComment, SnsLike, SnsMedia, SnsPost};
