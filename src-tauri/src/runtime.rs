use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::telegram::{
    ChatIngested, DownloadProgress, SessionPaths, SyncHandle, TelegramHandle, TelegramStatusChanged,
};

const EVENT_CAP: usize = 64;

/// 桌面 IPC 与 HTTP/SSE 共用的事件总线。
#[derive(Clone)]
pub struct EventHub {
    telegram_status: broadcast::Sender<TelegramStatusChanged>,
    download_progress: broadcast::Sender<DownloadProgress>,
    chat_ingested: broadcast::Sender<ChatIngested>,
}

impl EventHub {
    pub fn new() -> Self {
        Self {
            telegram_status: broadcast::channel(EVENT_CAP).0,
            download_progress: broadcast::channel(EVENT_CAP).0,
            chat_ingested: broadcast::channel(EVENT_CAP).0,
        }
    }

    pub fn emit_telegram_status(&self, payload: TelegramStatusChanged) {
        let _ = self.telegram_status.send(payload);
    }

    pub fn emit_download_progress(&self, payload: DownloadProgress) {
        let _ = self.download_progress.send(payload);
    }

    pub fn emit_chat_ingested(&self, payload: ChatIngested) {
        let _ = self.chat_ingested.send(payload);
    }

    pub fn subscribe_telegram_status(&self) -> broadcast::Receiver<TelegramStatusChanged> {
        self.telegram_status.subscribe()
    }

    pub fn subscribe_download_progress(&self) -> broadcast::Receiver<DownloadProgress> {
        self.download_progress.subscribe()
    }

    pub fn subscribe_chat_ingested(&self) -> broadcast::Receiver<ChatIngested> {
        self.chat_ingested.subscribe()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

/// 桌面与 headless 共用的运行时：数据目录、事件、Telegram 句柄。
#[derive(Clone)]
pub struct AppCtx {
    pub data_root: PathBuf,
    pub download_dir_override: Option<PathBuf>,
    pub events: EventHub,
    pub telegram: Arc<tokio::sync::Mutex<TelegramHandle>>,
    pub sync: SyncHandle,
}

impl AppCtx {
    pub fn new(data_root: PathBuf, download_dir_override: Option<PathBuf>) -> Self {
        Self {
            data_root,
            download_dir_override,
            events: EventHub::new(),
            telegram: Arc::new(tokio::sync::Mutex::new(TelegramHandle::new())),
            sync: SyncHandle::new(),
        }
    }

    pub fn paths(&self) -> SessionPaths {
        SessionPaths::from_root(&self.data_root, self.download_dir_override.as_deref())
    }
}
