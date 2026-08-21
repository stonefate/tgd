mod client;
mod download;
mod links;
mod media_pool;
mod session;
mod store;
mod sync;

pub use client::{
    emit_telegram_status, load_dotenv, AccountInfo, ChatItem, ChatKind, LoginStep, TelegramHandle,
    TelegramStatusChanged,
};
pub use download::{
    file_mtime_unix, infer_from_path, scan_download_dir, MediaIndex, MediaKind, UsageKindBytes,
};
pub use links::{extract_message_links, sanitize_http_url};
pub use session::SessionPaths;
pub use store::{MessageLink, MessageRecord, MessageSearchCursor, MessageStore};
pub use sync::{
    notify_settings_changed, reset_chat_cursor, spawn_download_worker, ActiveDownload,
    ChatIngested, DownloadPhase, DownloadProgress, SyncHandle,
};
