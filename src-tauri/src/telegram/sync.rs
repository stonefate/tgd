use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grammers_client::client::UpdatesConfiguration;
use grammers_client::media::Media;
use grammers_client::message::Message;
use grammers_client::session::types::PeerRef;
use grammers_client::session::updates::UpdatesLike;
use grammers_client::tl::enums::Message as TlMessage;
use grammers_client::update::Update;
use grammers_client::{Client, InvocationError};
use serde::{Deserialize, Serialize};
use specta::Type;
#[cfg(feature = "desktop")]
use tauri_specta::Event;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;

use crate::error::AppError;
use crate::runtime::{AppCtx, EventHub};
use crate::settings::{is_user_peer_id, AppSettings, ChatDownloadTypes};
use crate::telegram::client::{
    channel_comment_count, channel_post_may_have_comments, emit_telegram_status,
    fetch_linked_discussion, fetch_post_comments, resolve_public_chat, CommentMessage, TelegramApi,
};
use crate::telegram::download::{
    aligned_part_len, below_min_media_size, classify_media, clear_part_off, discard_partial,
    is_before_cutoff, media_file_id, media_mime, media_original_name, media_path, part_path,
    part_resume_len, skip_chunks, MediaIndex,
};
use crate::telegram::media_pool::{MediaPool, ParallelError, CHUNK_TIMEOUT, MAX_PARALLEL_LARGE};
use crate::telegram::session::SessionPaths;
use crate::telegram::store::{MessageRecord, MessageStore};
use crate::telegram::MediaKind;

const HISTORY_PAGE: usize = 100;
/// 连续几条帖 getReplies 都 CHANNEL_PRIVATE 才认定整个频道评论不可读。
const COMMENT_PRIVATE_LIMIT: u8 = 3;
/// 连续几次网络停滞（超时 / 断连）后暂不推进回爬游标，短等后重试，不暂停引擎。
const NETWORK_STALL_LIMIT: u8 = 3;
/// 单文件本批网络停滞上限；超过则跳过该文件并清零连续计数，避免死磕。
const NETWORK_FILE_STALL_LIMIT: u8 = 5;
const NETWORK_STALL_WAIT_SECS: u64 = 15;
const NETWORK_STALL_WAIT_DETAIL: &str = "网络中断，等待重试";
const PAGE_DELAY_MS: (u64, u64) = (1500, 2500);
const MEDIA_DELAY_MS: (u64, u64) = (400, 1000);
const CHAT_DELAY_MS: u64 = 5000;
const FLOOD_LONG_SECS: u64 = 300;
const FLOOD_PAUSE_SECS: u64 = 15 * 60;
/// Timeout 重试前短暂等待，不暂停引擎、也不走 FloodWait UI。
const TIMEOUT_RETRY_BACKOFF_SECS: u64 = 5;
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(30);
const QUEUE_PREVIEW_LIMIT: usize = 10;
const GUEST_POLL_INTERVAL: Duration = Duration::from_secs(45);
const GUEST_POLL_PAGE: usize = 30;
const LOADING_PEERS_DETAIL: &str = "正在加载会话";
/// 离线 catch_up 队列上限，避免 getDifference 把回爬饿死。
const UPDATE_PUMP_CAP: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum DownloadPhase {
    Idle,
    Live,
    Backfill,
    Wait,
    FloodWait,
    Reconnect,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ActiveDownload {
    pub file_id: String,
    pub file_name: String,
    pub kind: MediaKind,
    pub bytes: String,
    pub total: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueuedDownload {
    pub file_id: String,
    pub file_name: String,
    pub kind: MediaKind,
    pub chat_id: Option<String>,
    pub message_id: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[cfg_attr(feature = "desktop", derive(Event))]
#[serde(rename_all = "camelCase")]
pub struct ChatIngested {
    pub chat_id: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[cfg_attr(feature = "desktop", derive(Event))]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub phase: DownloadPhase,
    pub chat_id: Option<String>,
    pub chat_title: Option<String>,
    pub processed: u32,
    pub downloaded: u32,
    pub skipped: u32,
    pub flood_wait_secs: Option<u32>,
    pub detail: Option<String>,
    pub current_file: Option<String>,
    pub current_kind: Option<MediaKind>,
    pub active: Vec<ActiveDownload>,
    pub queued: Vec<QueuedDownload>,
    pub paused: bool,
}

impl Default for DownloadProgress {
    fn default() -> Self {
        Self {
            phase: DownloadPhase::Idle,
            chat_id: None,
            chat_title: None,
            processed: 0,
            downloaded: 0,
            skipped: 0,
            flood_wait_secs: None,
            detail: None,
            current_file: None,
            current_kind: None,
            active: Vec::new(),
            queued: Vec::new(),
            paused: false,
        }
    }
}

#[derive(Clone)]
pub struct SyncHandle {
    inner: Arc<SyncInner>,
}

struct SyncInner {
    notify: Notify,
    stopped: Notify,
    started: AtomicBool,
    reload: AtomicBool,
    paused: AtomicBool,
    cancel_all: AtomicBool,
    stop: AtomicBool,
    status: Mutex<DownloadProgress>,
    pending_resets: Mutex<Vec<String>>,
    pending_redownloads: Mutex<VecDeque<RedownloadRequest>>,
    pending_checks: Mutex<VecDeque<String>>,
    inflight_redownloads: Mutex<HashSet<(String, i32)>>,
    inflight_file_ids: Mutex<HashSet<String>>,
    cancel_ids: Mutex<HashSet<String>>,
    cancel_chats: Mutex<HashSet<String>>,
}

#[derive(Clone)]
struct RedownloadRequest {
    chat_id: String,
    message_id: i32,
    file_id: Option<String>,
    file_name: Option<String>,
    kind: Option<MediaKind>,
}

impl SyncHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SyncInner {
                notify: Notify::new(),
                stopped: Notify::new(),
                started: AtomicBool::new(false),
                reload: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                cancel_all: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                status: Mutex::new(DownloadProgress::default()),
                pending_resets: Mutex::new(Vec::new()),
                pending_redownloads: Mutex::new(VecDeque::new()),
                pending_checks: Mutex::new(VecDeque::new()),
                inflight_redownloads: Mutex::new(HashSet::new()),
                inflight_file_ids: Mutex::new(HashSet::new()),
                cancel_ids: Mutex::new(HashSet::new()),
                cancel_chats: Mutex::new(HashSet::new()),
            }),
        }
    }

    pub fn notify(&self) {
        self.inner.notify.notify_one();
    }

    pub fn should_stop(&self) -> bool {
        self.inner.stop.load(Ordering::SeqCst)
    }

    /// 退出登录：立刻停 worker，且不要再重连。
    pub fn request_stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.cancel_all.store(true, Ordering::SeqCst);
        self.inner.paused.store(false, Ordering::SeqCst);
        if let Ok(mut status) = self.inner.status.lock() {
            status.paused = false;
        }
        self.notify();
    }

    fn prepare_for_run(&self) {
        self.inner.stop.store(false, Ordering::SeqCst);
        if !self.is_paused() {
            self.inner.cancel_all.store(false, Ordering::SeqCst);
        }
        if let Ok(mut ids) = self.inner.cancel_ids.lock() {
            ids.clear();
        }
        if let Ok(mut chats) = self.inner.cancel_chats.lock() {
            chats.clear();
        }
        if let Ok(mut set) = self.inner.inflight_redownloads.lock() {
            set.clear();
        }
        if let Ok(mut set) = self.inner.inflight_file_ids.lock() {
            set.clear();
        }
    }

    pub async fn wait_stopped(&self) {
        if !self.inner.started.load(Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(8), self.inner.stopped.notified()).await;
    }

    pub fn reset_progress(&self, events: &EventHub) {
        self.patch_status(events, |status| {
            *status = DownloadProgress::default();
        });
    }

    async fn notified(&self) {
        self.inner.notify.notified().await;
    }

    pub fn mark_reload(&self) {
        self.inner.reload.store(true, Ordering::SeqCst);
        self.notify();
    }

    fn take_reload(&self) -> bool {
        self.inner.reload.swap(false, Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::SeqCst)
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.paused.store(paused, Ordering::SeqCst);
        self.inner.cancel_all.store(paused, Ordering::SeqCst);
        if !paused {
            if let Ok(mut ids) = self.inner.cancel_ids.lock() {
                ids.clear();
            }
        }
        if let Ok(mut status) = self.inner.status.lock() {
            status.paused = paused;
            status.detail = if paused {
                Some("已暂停".into())
            } else {
                None
            };
        }
        self.notify();
    }

    pub fn cancel_file(&self, file_id: String) {
        let file_id = file_id.trim();
        if file_id.is_empty() {
            return;
        }
        if let Ok(mut ids) = self.inner.cancel_ids.lock() {
            ids.insert(file_id.to_string());
        }
        if let Ok(mut status) = self.inner.status.lock() {
            status.active.retain(|item| item.file_id != file_id);
            status.queued.retain(|item| item.file_id != file_id);
            if let Some((name, kind)) = status
                .active
                .first()
                .map(|item| (item.file_name.clone(), item.kind))
            {
                status.current_file = Some(name);
                status.current_kind = Some(kind);
            } else {
                status.current_file = None;
                status.current_kind = None;
            }
        }
        self.notify();
    }

    pub fn cancel_chat(&self, chat_id: String) {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return;
        }
        if let Ok(mut ids) = self.inner.cancel_chats.lock() {
            ids.insert(chat_id.to_string());
        }
        if let Ok(mut list) = self.inner.pending_redownloads.lock() {
            list.retain(|item| item.chat_id != chat_id);
        }
        if let Ok(mut list) = self.inner.pending_checks.lock() {
            list.retain(|id| id != chat_id);
        }
        if let Ok(mut status) = self.inner.status.lock() {
            status
                .queued
                .retain(|item| item.chat_id.as_deref() != Some(chat_id));
        }
        self.notify();
    }

    pub fn allow_chat(&self, chat_id: &str) {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return;
        }
        if let Ok(mut ids) = self.inner.cancel_chats.lock() {
            ids.remove(chat_id);
        }
    }

    fn is_chat_cancelled(&self, chat_id: &str) -> bool {
        self.inner
            .cancel_chats
            .lock()
            .map(|ids| ids.contains(chat_id))
            .unwrap_or(false)
    }

    fn should_cancel(&self, file_id: &str) -> bool {
        if self.inner.cancel_all.load(Ordering::SeqCst) {
            return true;
        }
        self.is_file_cancelled(file_id)
    }

    fn is_file_cancelled(&self, file_id: &str) -> bool {
        self.inner
            .cancel_ids
            .lock()
            .map(|ids| ids.contains(file_id))
            .unwrap_or(false)
    }

    #[cfg(test)]
    fn should_cancel_job(&self, file_id: &str, chat_id: &str) -> bool {
        self.should_cancel(file_id) || self.is_chat_cancelled(chat_id)
    }

    /// 暂停 / 退出保留 `.part`；单文件取消则删掉。关监听不中断进行中的文件。
    fn keep_partial_file(&self) -> bool {
        self.should_stop() || self.inner.cancel_all.load(Ordering::SeqCst)
    }

    pub fn request_reset(&self, chat_id: String) {
        if let Ok(mut list) = self.inner.pending_resets.lock() {
            if !list.iter().any(|id| id == &chat_id) {
                list.push(chat_id);
            }
        }
        self.mark_reload();
    }

    fn take_resets(&self) -> Vec<String> {
        self.inner
            .pending_resets
            .lock()
            .map(|mut list| std::mem::take(&mut *list))
            .unwrap_or_default()
    }

    pub fn request_redownload(
        &self,
        chat_id: String,
        message_id: i32,
        file_id: Option<String>,
        file_name: Option<String>,
        kind: Option<MediaKind>,
    ) -> bool {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() || message_id <= 0 {
            return false;
        }
        let file_id = file_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        if self.is_redownload_busy(chat_id, message_id, file_id.as_deref()) {
            return false;
        }
        if let Ok(mut list) = self.inner.pending_redownloads.lock() {
            list.push_back(RedownloadRequest {
                chat_id: chat_id.to_string(),
                message_id,
                file_id,
                file_name: file_name
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty()),
                kind,
            });
        }
        self.notify();
        true
    }

    fn is_redownload_busy(&self, chat_id: &str, message_id: i32, file_id: Option<&str>) -> bool {
        if self
            .inner
            .inflight_redownloads
            .lock()
            .map(|set| {
                set.iter()
                    .any(|(id, mid)| id == chat_id && *mid == message_id)
            })
            .unwrap_or(false)
        {
            return true;
        }
        if self
            .inner
            .pending_redownloads
            .lock()
            .map(|list| {
                list.iter()
                    .any(|item| item.chat_id == chat_id && item.message_id == message_id)
            })
            .unwrap_or(false)
        {
            return true;
        }
        let Some(file_id) = file_id.filter(|id| !id.is_empty()) else {
            return false;
        };
        if self
            .inner
            .inflight_file_ids
            .lock()
            .map(|set| set.contains(file_id))
            .unwrap_or(false)
        {
            return true;
        }
        if self
            .inner
            .pending_redownloads
            .lock()
            .map(|list| {
                list.iter()
                    .any(|item| item.file_id.as_deref() == Some(file_id))
            })
            .unwrap_or(false)
        {
            return true;
        }
        self.snapshot()
            .active
            .iter()
            .any(|item| item.file_id == file_id)
    }

    fn pop_redownload(&self) -> Option<RedownloadRequest> {
        let req = self
            .inner
            .pending_redownloads
            .lock()
            .ok()
            .and_then(|mut list| list.pop_front())?;
        if let Ok(mut set) = self.inner.inflight_redownloads.lock() {
            set.insert((req.chat_id.clone(), req.message_id));
        }
        if let Some(file_id) = &req.file_id {
            if let Ok(mut set) = self.inner.inflight_file_ids.lock() {
                set.insert(file_id.clone());
            }
        }
        Some(req)
    }

    fn requeue_redownload(&self, req: RedownloadRequest) {
        self.clear_inflight_redownload(&req);
        if let Ok(mut list) = self.inner.pending_redownloads.lock() {
            if !list
                .iter()
                .any(|item| item.chat_id == req.chat_id && item.message_id == req.message_id)
            {
                list.push_front(req);
            }
        }
        self.notify();
    }

    fn finish_redownload(&self, req: &RedownloadRequest) {
        self.clear_inflight_redownload(req);
    }

    fn clear_inflight_redownload(&self, req: &RedownloadRequest) {
        if let Ok(mut set) = self.inner.inflight_redownloads.lock() {
            set.remove(&(req.chat_id.clone(), req.message_id));
        }
        if let Some(file_id) = &req.file_id {
            if let Ok(mut set) = self.inner.inflight_file_ids.lock() {
                set.remove(file_id);
            }
        }
    }

    fn peek_redownloads(&self, limit: usize) -> Vec<RedownloadRequest> {
        self.inner
            .pending_redownloads
            .lock()
            .map(|list| list.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    pub fn request_check(&self, chat_id: String) -> bool {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return false;
        }
        if let Ok(mut list) = self.inner.pending_checks.lock() {
            if list.iter().any(|id| id == chat_id) {
                return false;
            }
            list.push_back(chat_id.to_string());
        }
        self.notify();
        true
    }

    fn has_check(&self) -> bool {
        self.inner
            .pending_checks
            .lock()
            .map(|list| !list.is_empty())
            .unwrap_or(false)
    }

    fn pop_check(&self) -> Option<String> {
        self.inner
            .pending_checks
            .lock()
            .ok()
            .and_then(|mut list| list.pop_front())
    }

    /// 进行中的下载：暂停 / 退出 / 单文件取消会停；关监听不停，让当前文件下完。
    fn should_cancel_media(&self, file_id: &str, _chat_id: &str, force: bool) -> bool {
        if force {
            self.inner.cancel_all.load(Ordering::SeqCst)
        } else {
            self.should_cancel(file_id)
        }
    }

    pub fn snapshot(&self) -> DownloadProgress {
        self.inner
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    fn try_begin(&self) -> bool {
        if self
            .inner
            .started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        self.prepare_for_run();
        true
    }

    fn mark_stopped(&self) {
        self.inner.started.store(false, Ordering::SeqCst);
        self.inner.stopped.notify_one();
    }

    fn patch_status(&self, events: &EventHub, patch: impl FnOnce(&mut DownloadProgress)) {
        let snap = {
            let Ok(mut status) = self.inner.status.lock() else {
                return;
            };
            patch(&mut status);
            status.paused = self.inner.paused.load(Ordering::SeqCst);
            status.clone()
        };
        events.emit_download_progress(snap);
    }

    fn report_file_progress(&self, events: &EventHub, item: ActiveDownload) {
        if self.is_file_cancelled(&item.file_id) {
            return;
        }
        self.patch_status(events, |status| {
            if let Some(slot) = status
                .active
                .iter_mut()
                .find(|slot| slot.file_id == item.file_id)
            {
                *slot = item.clone();
            } else {
                status.active.push(item.clone());
            }
            status.current_file = Some(item.file_name);
            status.current_kind = Some(item.kind);
        });
    }

    fn clear_file_progress(&self, events: &EventHub, file_id: &str) {
        self.patch_status(events, |status| {
            status.active.retain(|item| item.file_id != file_id);
            if let Some(first) = status.active.first() {
                status.current_file = Some(first.file_name.clone());
                status.current_kind = Some(first.kind);
            } else {
                status.current_file = None;
                status.current_kind = None;
            }
        });
    }
}

impl Default for SyncHandle {
    fn default() -> Self {
        Self::new()
    }
}

pub fn notify_settings_changed(sync: &SyncHandle) {
    sync.mark_reload();
}

pub fn spawn_download_worker(ctx: AppCtx) {
    if !ctx.sync.try_begin() {
        return;
    }
    let handle = ctx.sync.clone();
    tokio::spawn(async move {
        run_worker_supervisor(ctx).await;
        handle.mark_stopped();
    });
}

enum SessionAcquire {
    Ready(Client, mpsc::UnboundedReceiver<UpdatesLike>),
    Unauthorized,
    Stopped,
    Failed(AppError),
}

async fn acquire_telegram_session(ctx: &AppCtx, handle: &SyncHandle) -> SessionAcquire {
    if handle.should_stop() {
        return SessionAcquire::Stopped;
    }
    let mut telegram = ctx.telegram.lock().await;
    if handle.should_stop() {
        return SessionAcquire::Stopped;
    }
    if let Some((client, updates)) = telegram.take_worker_session() {
        return SessionAcquire::Ready(client, updates);
    }
    let paths = ctx.paths();
    match telegram.reconnect_transport(&paths).await {
        Ok((client, updates)) if telegram.is_authorized() => {
            emit_telegram_status(&ctx.events, &telegram);
            SessionAcquire::Ready(client, updates)
        }
        Ok(_) => {
            emit_telegram_status(&ctx.events, &telegram);
            SessionAcquire::Unauthorized
        }
        Err(err) => {
            emit_telegram_status(&ctx.events, &telegram);
            SessionAcquire::Failed(err)
        }
    }
}

fn emit_reconnect(events: &EventHub, handle: &SyncHandle, delay: Duration) {
    handle.patch_status(events, |status| {
        status.phase = DownloadPhase::Reconnect;
        status.detail = Some(format!("连接断开，{}s 后重连", delay.as_secs().max(1)));
        status.active.clear();
        status.queued.clear();
        status.current_file = None;
        status.current_kind = None;
    });
}

async fn wait_or_stop(handle: &SyncHandle, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => handle.should_stop(),
        _ = handle.notified() => handle.should_stop(),
    }
}

async fn run_worker_supervisor(ctx: AppCtx) {
    let handle = ctx.sync.clone();
    let mut backoff = RECONNECT_MIN;
    loop {
        if handle.should_stop() {
            break;
        }
        match acquire_telegram_session(&ctx, &handle).await {
            SessionAcquire::Stopped => break,
            SessionAcquire::Unauthorized => {
                handle.patch_status(&ctx.events, |status| {
                    status.phase = DownloadPhase::Idle;
                    status.detail = Some("登录已失效，请重新登录".into());
                    status.active.clear();
                    status.current_file = None;
                    status.current_kind = None;
                });
                break;
            }
            SessionAcquire::Failed(err) => {
                log::warn!("telegram reconnect failed: {err}");
                emit_reconnect(&ctx.events, &handle, backoff);
                if wait_or_stop(&handle, backoff).await {
                    break;
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
            SessionAcquire::Ready(client, updates) => {
                let started = Instant::now();
                match run_download_worker(ctx.clone(), client, updates, handle.clone()).await {
                    Ok(()) => {
                        if handle.should_stop() {
                            break;
                        }
                        log::warn!("download worker ended");
                    }
                    Err(err) => {
                        log::warn!("download worker exited: {err}");
                    }
                }
                if handle.should_stop() {
                    break;
                }
                if started.elapsed() >= RECONNECT_RESET_AFTER {
                    backoff = RECONNECT_MIN;
                }
                emit_reconnect(&ctx.events, &handle, backoff);
                if wait_or_stop(&handle, backoff).await {
                    break;
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    #[serde(default)]
    chats: HashMap<String, ChatCursor>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ChatCursor {
    #[serde(default)]
    backfill_offset_id: i32,
    #[serde(default)]
    backfill_done: bool,
    #[serde(default)]
    applied_days: i32,
    /// 已经完整扫过当前窗口的类型。缺字段 = 旧数据，按当前勾选 grandfather，不重扫。
    #[serde(default)]
    applied_types: Option<ChatDownloadTypes>,
    /// 类型定点回补已翻到的 message_id（往更旧）。0 = 从头。
    #[serde(default)]
    type_after_id: i32,
    /// 本地没有可用 id，已退回整段 `iter_messages`。
    #[serde(default)]
    type_fallback: bool,
    /// 已按 id 拉完已扫窗口，历史回爬还在继续。
    #[serde(default)]
    type_ids_done: bool,
    /// 已见过的最新 message_id。补洞时拉到这个 id 就停。
    #[serde(default)]
    backfill_head_id: i32,
    /// 正在补离线空洞（从最新拉到 `backfill_head_id`），不是整段回爬。
    #[serde(default)]
    gap_fill: bool,
}

/// 按天数和已扫类型更新游标。返回是否因新勾类型从头回补。
fn reconcile_chat_cursor(cursor: &mut ChatCursor, days: i32, types: ChatDownloadTypes) -> bool {
    let first_seen = cursor.applied_types.is_none();
    if first_seen {
        cursor.applied_types = Some(types);
    }

    if days == 0 {
        cursor.backfill_done = true;
        cursor.applied_days = 0;
        return false;
    }

    if days < 0 {
        if cursor.applied_days >= 0 {
            cursor.backfill_done = false;
        }
        cursor.applied_days = days;
    } else if cursor.applied_days >= 0 && cursor.applied_days < days {
        cursor.backfill_done = false;
        cursor.applied_days = days;
    } else if cursor.applied_days >= 0 {
        cursor.applied_days = days;
    }

    if first_seen {
        return false;
    }
    let applied = cursor.applied_types.unwrap_or_default();
    let newly = types.newly_enabled(applied);
    if newly.text {
        cursor.backfill_offset_id = 0;
        cursor.backfill_done = false;
        cursor.gap_fill = false;
        reset_type_scan(cursor);
        true
    } else if newly.any_media() {
        reset_type_scan(cursor);
        true
    } else {
        false
    }
}

fn reset_type_scan(cursor: &mut ChatCursor) {
    cursor.type_after_id = 0;
    cursor.type_fallback = false;
    cursor.type_ids_done = false;
}

fn mark_cursor_done(cursor: &mut ChatCursor, days: i32, types: ChatDownloadTypes) {
    cursor.backfill_done = true;
    cursor.applied_days = days;
    cursor.applied_types = Some(cursor.applied_types.unwrap_or_default().union(types));
    cursor.gap_fill = false;
    reset_type_scan(cursor);
}

/// 已完成的窗口再启动时，从最新往回补离线空洞。
fn reopen_done_for_gap_fill(cursor: &mut ChatCursor, days: i32, newest_local: Option<i32>) {
    if days == 0 || !cursor.backfill_done {
        return;
    }
    if cursor.backfill_head_id <= 0 {
        if let Some(id) = newest_local {
            cursor.backfill_head_id = id;
        }
    }
    cursor.backfill_done = false;
    cursor.backfill_offset_id = 0;
    cursor.gap_fill = true;
}

fn hits_history_stop(message_id: i32, before_cutoff: bool, head_id: i32, gap_fill: bool) -> bool {
    before_cutoff || (gap_fill && head_id > 0 && message_id <= head_id)
}

/// 普通回爬：索引有过就跳过。检查模式：文件还在才跳过。
fn skip_indexed_media(indexed: bool, file_exists: bool, recover: bool) -> bool {
    indexed && (!recover || file_exists)
}

fn idle_backfill_detail(any_window: bool) -> String {
    if any_window {
        "回爬已完成".into()
    } else {
        "只收新消息".into()
    }
}

/// 回爬需要 PeerRef 的会话：已监听且不是未加入评论组。
fn needed_backfill_peer_ids(settings: &AppSettings) -> Vec<String> {
    let mut ids: Vec<String> = settings
        .watched_chat_ids
        .iter()
        .filter(|id| !settings.is_auto_comment(id) && !is_user_peer_id(id))
        .cloned()
        .collect();
    if let Some(id) = settings.guest_chat_id() {
        if !ids.iter().any(|have| have == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

fn dialog_scan_complete<V>(needed: &[String], peers: &HashMap<String, V>) -> bool {
    !needed.is_empty() && needed.iter().all(|id| peers.contains_key(id))
}

/// 暂停 / 退出 / 关监听时不要提交这批游标。单文件取消或单文件失败会跳过该文件并继续。
fn should_commit_backfill_cursor(leftover: bool, chat_still_syncing: bool) -> bool {
    !leftover && chat_still_syncing
}

fn next_comment_private_streak(streak: u8) -> (u8, bool) {
    let next = streak.saturating_add(1);
    (next, next >= COMMENT_PRIVATE_LIMIT)
}

fn next_network_stall_streak(streak: u8) -> (u8, bool) {
    let next = streak.saturating_add(1);
    (next, next >= NETWORK_STALL_LIMIT)
}

fn next_file_stall_count(count: u8) -> (u8, bool) {
    let next = count.saturating_add(1);
    (next, next >= NETWORK_FILE_STALL_LIMIT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaFailAction {
    Hold,
    Retry,
    Pause,
    Skip { reset_streak: bool },
}

fn media_fail_action(
    err: &DownloadTaskError,
    stall_streak: u8,
    file_over: bool,
    already_retried: bool,
) -> MediaFailAction {
    if is_network_stall(err) && file_over {
        MediaFailAction::Skip { reset_streak: true }
    } else if is_network_stall(err) && stall_streak >= NETWORK_STALL_LIMIT {
        MediaFailAction::Hold
    } else if !already_retried {
        MediaFailAction::Retry
    } else if should_pause_after_retries(err) {
        MediaFailAction::Pause
    } else {
        MediaFailAction::Skip {
            reset_streak: false,
        }
    }
}

fn task_error_blocks_cursor(err: &DownloadTaskError) -> bool {
    matches!(err, DownloadTaskError::Cancelled { keep_part: true })
}

fn type_fetch_pending(cursor: &ChatCursor, days: i32, types: ChatDownloadTypes) -> bool {
    if days == 0 {
        return false;
    }
    let Some(applied) = cursor.applied_types else {
        return false;
    };
    let newly = types.newly_enabled(applied);
    newly.any_media() && !newly.text && !cursor.type_fallback && !cursor.type_ids_done
}

fn local_covers_window(oldest: Option<i64>, days: i32, now: i64) -> bool {
    let Some(oldest) = oldest else {
        return false;
    };
    if days < 0 {
        return true;
    }
    if days == 0 {
        return false;
    }
    let cutoff = now - days as i64 * 86_400;
    oldest <= cutoff + 86_400
}

fn cutoff_unix(days: i32, now: i64) -> Option<i64> {
    if days <= 0 {
        None
    } else {
        Some(now - days as i64 * 86_400)
    }
}

impl SyncState {
    fn path(root: &Path) -> std::path::PathBuf {
        root.join("sync-state.json")
    }

    fn load(root: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(Self::path(root)) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_else(|err| {
            log::warn!("failed to parse sync-state.json: {err}");
            Self::default()
        })
    }

    fn save(&self, root: &Path) -> Result<(), AppError> {
        std::fs::create_dir_all(root).map_err(|err| AppError::Io(err.to_string()))?;
        let raw =
            serde_json::to_string_pretty(self).map_err(|err| AppError::Io(err.to_string()))?;
        std::fs::write(Self::path(root), raw).map_err(|err| AppError::Io(err.to_string()))
    }
}

pub fn reset_chat_cursor(root: &Path, chat_id: &str) -> Result<(), AppError> {
    let mut state = SyncState::load(root);
    state
        .chats
        .insert(chat_id.to_string(), ChatCursor::default());
    state.save(root)
}

#[derive(Clone)]
struct PeerInfo {
    peer: PeerRef,
    title: String,
}

#[derive(Clone, Copy)]
enum BackfillMode {
    History,
    Types { newly: ChatDownloadTypes },
    Check,
}

struct BackfillJob {
    chat_id: String,
    title: String,
    peer: PeerRef,
    pending: VecDeque<Message>,
    finish_when_empty: bool,
    mode: BackfillMode,
    page_end_id: Option<i32>,
    /// 检查模式的内存游标，不写 `sync-state.json`。
    offset_id: i32,
}

struct Worker {
    ctx: AppCtx,
    client: Client,
    handle: SyncHandle,
    settings: AppSettings,
    paths: SessionPaths,
    index: MediaIndex,
    sync: SyncState,
    store: MessageStore,
    peers: HashMap<String, PeerInfo>,
    backfill: Option<BackfillJob>,
    progress: DownloadProgress,
    media_pool: Arc<MediaPool>,
    /// 已拉过的频道帖评论数，避免编辑更新重复请求。
    comment_fetched: HashMap<(String, i32), i32>,
    /// getReplies 对该频道连续 CHANNEL_PRIVATE，不再试。
    comment_blocked: HashSet<String>,
    /// 该频道连续多少条帖评论返回 CHANNEL_PRIVATE。
    comment_private_streak: HashMap<String, u8>,
    /// 刚清除的会话，等当前批次结束后再允许回爬。
    reset_hold: HashSet<String>,
    /// 已失败重试过一次的 file_id（超时 / 过期引用 / 其它失败）。
    /// 跳过该文件或恢复下载时清掉，避免同一 file_id 立刻再次暂停。
    download_retried: HashSet<String>,
    /// media-index 有未落盘的 remember。
    index_dirty: bool,
    /// 已收集、尚未开下的媒体，给下载页队列预览。
    queued_jobs: Vec<QueuedDownload>,
    last_guest_poll: Instant,
}

async fn run_download_worker(
    ctx: AppCtx,
    client: Client,
    updates: mpsc::UnboundedReceiver<UpdatesLike>,
    handle: SyncHandle,
) -> Result<(), AppError> {
    let mut stream = client
        .stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: true,
                ..Default::default()
            },
        )
        .await
        .map_err(|err| AppError::Telegram(err.to_string()))?;

    let paths = ctx.paths();
    paths.ensure_dirs()?;
    let settings = AppSettings::load(&paths.root);
    let store = ctx.message_store().await?;
    let mut worker = Worker {
        ctx,
        client,
        handle,
        settings,
        index: MediaIndex::load(&paths.root),
        sync: SyncState::load(&paths.root),
        store,
        paths,
        peers: HashMap::new(),
        backfill: None,
        progress: DownloadProgress::default(),
        media_pool: MediaPool::new(),
        comment_fetched: HashMap::new(),
        comment_blocked: HashSet::new(),
        comment_private_streak: HashMap::new(),
        reset_hold: HashSet::new(),
        download_retried: HashSet::new(),
        index_dirty: false,
        queued_jobs: Vec::new(),
        last_guest_poll: Instant::now(),
    };
    worker.apply_resets();
    worker.drop_ignored_user_watches();
    worker.reconcile_cursors();
    worker.reopen_gap_fills().await;
    worker.emit(
        DownloadPhase::Idle,
        None,
        None,
        Some(LOADING_PEERS_DETAIL.into()),
    );

    if let Err(err) = worker.refresh_peers().await {
        log::warn!("initial dialog refresh failed: {err}");
    }
    worker.handle.set_paused(worker.settings.download_paused);
    if worker.handle.is_paused() {
        worker.emit(DownloadPhase::Idle, None, None, Some("已暂停".into()));
    } else {
        worker.emit(DownloadPhase::Idle, None, None, None);
    }

    let (update_tx, mut update_rx) = mpsc::channel(UPDATE_PUMP_CAP);
    tokio::spawn(async move {
        loop {
            let item = stream.next().await;
            if update_tx.send(item).await.is_err() {
                break;
            }
        }
    });

    let mut delay = Duration::from_millis(200);
    loop {
        if worker.handle.should_stop() {
            worker.flush_index();
            return Ok(());
        }
        tokio::select! {
            update = update_rx.recv() => {
                if worker.handle.should_stop() {
                    worker.flush_index();
                    return Ok(());
                }
                match update {
                    Some(Ok(update)) => {
                        if let Err(err) = worker.handle_update(update).await {
                            if let Some(secs) = flood_wait_secs(&err) {
                                worker.sleep_flood(secs).await;
                            } else {
                                log::warn!("live update failed: {err}");
                            }
                        }
                    }
                    Some(Err(err)) => {
                        if let Some(secs) = flood_wait_secs(&err) {
                            worker.sleep_flood(secs).await;
                        } else {
                            return Err(err.into());
                        }
                    }
                    None => {
                        return Err(AppError::Telegram("更新流已断开".into()));
                    }
                }
            }
            _ = worker.handle.notified() => {
                if worker.handle.should_stop() {
                    worker.flush_index();
                    return Ok(());
                }
                worker.apply_pending_settings().await;
                worker.clear_retries_on_resume();
                if worker.progress.paused {
                    worker.emit(
                        worker.progress.phase,
                        None,
                        None,
                        Some("已暂停".into()),
                    );
                } else {
                    worker.emit(worker.progress.phase, None, None, None);
                }
            }
            _ = tokio::time::sleep(delay) => {
                match worker.step_backfill().await {
                    Ok(next) => delay = next,
                    Err(err) => {
                        if let Some(secs) = flood_wait_secs_app(&err) {
                            worker.sleep_flood(secs).await;
                            delay = Duration::from_millis(500);
                        } else {
                            log::warn!("backfill step failed: {err}");
                            worker.backfill = None;
                            delay = Duration::from_secs(5);
                        }
                    }
                }
                if let Err(err) = worker.maybe_guest_poll().await {
                    log::warn!("guest poll: {err}");
                }
            }
        }
    }
}

impl Worker {
    async fn apply_pending_settings(&mut self) {
        if self.handle.take_reload() {
            self.reload_settings();
            self.reopen_gap_fills().await;
        } else {
            for id in std::mem::take(&mut self.reset_hold) {
                if self.settings.should_sync_chat(&id) {
                    self.handle.allow_chat(&id);
                }
            }
        }
        self.drop_unwatched_backfill();
        self.drop_unwatched_queue();
    }

    fn drop_unwatched_queue(&mut self) {
        let jobs = std::mem::take(&mut self.queued_jobs);
        self.queued_jobs = jobs
            .into_iter()
            .filter(|item| self.queued_chat_wanted(item.chat_id.as_deref()))
            .collect();
    }

    fn queued_chat_wanted(&self, chat_id: Option<&str>) -> bool {
        chat_id.is_none_or(|id| self.chat_still_syncing(id))
    }

    fn drop_ignored_user_watches(&mut self) {
        let dropped = self.settings.drop_user_watches();
        if dropped.is_empty() {
            return;
        }
        for id in dropped {
            self.handle.cancel_chat(id);
        }
        let _ = self.settings.save(&self.paths.root);
    }

    fn drop_unwatched_backfill(&mut self) {
        let drop = self.backfill.as_ref().is_some_and(|job| {
            !self.settings.should_sync_chat(&job.chat_id)
                || self.handle.is_chat_cancelled(&job.chat_id)
        });
        if drop {
            if let Some(job) = self.backfill.take() {
                self.handle.cancel_chat(job.chat_id);
            }
        }
    }

    fn chat_still_syncing(&self, chat_id: &str) -> bool {
        self.settings.should_sync_chat(chat_id) && !self.handle.is_chat_cancelled(chat_id)
    }

    fn reload_settings(&mut self) {
        let previous = self.settings.clone();
        self.paths = self.ctx.paths();
        let _ = self.paths.ensure_dirs();
        self.settings = AppSettings::load(&self.paths.root);
        self.apply_resets();
        self.drop_ignored_user_watches();
        self.reconcile_cursors();

        if let Some(job) = &self.backfill {
            if !self.settings.should_sync_chat(&job.chat_id) {
                self.handle.cancel_chat(job.chat_id.clone());
            }
        }
        for id in previous.watched_chat_ids {
            if self.settings.should_sync_chat(&id) {
                self.handle.allow_chat(&id);
            } else {
                self.handle.cancel_chat(id);
            }
        }
        for id in &self.settings.watched_chat_ids {
            if self.reset_hold.contains(id) {
                continue;
            }
            if self.settings.should_sync_chat(id) {
                self.handle.allow_chat(id);
            }
        }

        self.backfill = None;
        let detail = if self.any_type_backfill() {
            Some("回补新类型".into())
        } else {
            Some("设置已更新".into())
        };
        self.emit(DownloadPhase::Idle, None, None, detail);
        self.last_guest_poll = Instant::now()
            .checked_sub(GUEST_POLL_INTERVAL)
            .unwrap_or_else(Instant::now);
    }

    fn apply_resets(&mut self) {
        let resets = self.handle.take_resets();
        if resets.is_empty() {
            return;
        }
        self.index = MediaIndex::load(&self.paths.root);
        self.index_dirty = false;
        for chat_id in &resets {
            self.sync
                .chats
                .insert(chat_id.clone(), ChatCursor::default());
            self.handle.cancel_chat(chat_id.clone());
            self.reset_hold.insert(chat_id.clone());
            self.comment_blocked.remove(chat_id);
            self.comment_private_streak.remove(chat_id);
            self.comment_fetched
                .retain(|(channel, _), _| channel != chat_id);
            if let Some(channel) = self.settings.comment_channel_of(chat_id) {
                self.comment_fetched.retain(|(id, _), _| id != &channel);
                self.comment_blocked.remove(&channel);
                self.comment_private_streak.remove(&channel);
            }
            if self
                .backfill
                .as_ref()
                .is_some_and(|job| job.chat_id == *chat_id)
            {
                self.backfill = None;
            }
        }
        let _ = self.sync.save(&self.paths.root);
    }

    fn reconcile_cursors(&mut self) {
        let watched = self.settings.watched_chat_ids.clone();
        for chat_id in watched {
            let days = self.settings.effective_backfill_days(&chat_id);
            let types = self.settings.chat_types(&chat_id);
            let cursor = self.sync.chats.entry(chat_id).or_default();
            reconcile_chat_cursor(cursor, days, types);
        }
        let _ = self.sync.save(&self.paths.root);
    }

    async fn reopen_gap_fills(&mut self) {
        let watched = self.settings.watched_chat_ids.clone();
        for chat_id in watched {
            let days = self.settings.effective_backfill_days(&chat_id);
            let done = self
                .sync
                .chats
                .get(&chat_id)
                .is_some_and(|cursor| cursor.backfill_done);
            if days == 0 || !done {
                continue;
            }
            let newest = self.store.newest_message_id(&chat_id).await.ok().flatten();
            if let Some(cursor) = self.sync.chats.get_mut(&chat_id) {
                reopen_done_for_gap_fill(cursor, days, newest);
            }
        }
        let _ = self.sync.save(&self.paths.root);
    }

    fn idle_detail(&self) -> String {
        let any_window = self
            .settings
            .watched_chat_ids
            .iter()
            .any(|id| self.settings.effective_backfill_days(id) != 0);
        idle_backfill_detail(any_window)
    }

    fn is_type_backfill(&self, chat_id: &str) -> bool {
        if self.settings.effective_backfill_days(chat_id) == 0 {
            return false;
        }
        let types = self.settings.chat_types(chat_id);
        let applied = self
            .sync
            .chats
            .get(chat_id)
            .and_then(|cursor| cursor.applied_types)
            .unwrap_or_default();
        types.newly_enabled(applied).any()
    }

    fn any_type_backfill(&self) -> bool {
        self.settings
            .watched_chat_ids
            .iter()
            .any(|id| self.is_type_backfill(id))
    }

    fn type_backfill_detail(&self, chat_id: &str) -> Option<String> {
        if self.is_type_backfill(chat_id) {
            Some("回补新类型".into())
        } else {
            None
        }
    }

    fn job_detail(&self, chat_id: &str) -> Option<String> {
        if matches!(
            self.backfill.as_ref().map(|job| job.mode),
            Some(BackfillMode::Check)
        ) {
            Some("检查文件".into())
        } else {
            self.type_backfill_detail(chat_id)
        }
    }

    async fn refresh_peers(&mut self) -> Result<(), AppError> {
        let needed = needed_backfill_peer_ids(&self.settings);
        self.refresh_peers_for(&needed).await
    }

    async fn refresh_peers_for(&mut self, needed: &[String]) -> Result<(), AppError> {
        let missing: Vec<String> = needed
            .iter()
            .filter(|id| !self.peers.contains_key(*id))
            .cloned()
            .collect();
        if missing.is_empty() {
            self.fill_missing_discussion_peers().await;
            self.ensure_guest_peer().await;
            return Ok(());
        }

        let mut dialogs = self.client.iter_dialogs();
        while let Some(dialog) = dialogs.next().await? {
            let peer = dialog.peer();
            if matches!(peer, grammers_client::peer::Peer::User(_)) {
                continue;
            }
            let id = peer.id().to_string();
            let title = peer
                .name()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("#{id}"));
            if let Ok(Some(peer_ref)) = peer.to_ref().await {
                self.peers.insert(
                    id,
                    PeerInfo {
                        peer: peer_ref,
                        title,
                    },
                );
            }
            if dialog_scan_complete(&missing, &self.peers) {
                break;
            }
        }
        self.fill_missing_discussion_peers().await;
        self.ensure_guest_peer().await;
        Ok(())
    }

    async fn ensure_guest_peer(&mut self) {
        let Some(chat_id) = self.settings.guest_chat_id().map(str::to_string) else {
            return;
        };
        if self.peers.contains_key(&chat_id) {
            return;
        }
        let query = if !self.settings.guest_watch_query.trim().is_empty() {
            self.settings.guest_watch_query.clone()
        } else if !self.settings.guest_watch_username.trim().is_empty() {
            self.settings.guest_watch_username.clone()
        } else {
            return;
        };
        match resolve_public_chat(&self.client, &query).await {
            Ok(resolved) => {
                self.peers.insert(
                    resolved.chat_id,
                    PeerInfo {
                        peer: resolved.peer,
                        title: resolved.title,
                    },
                );
            }
            Err(err) => log::warn!("guest peer {chat_id}: {err}"),
        }
    }

    async fn maybe_guest_poll(&mut self) -> Result<(), AppError> {
        if self.last_guest_poll.elapsed() < GUEST_POLL_INTERVAL {
            return Ok(());
        }
        let Some(chat_id) = self.settings.active_guest_chat_id().map(str::to_string) else {
            return Ok(());
        };
        if self.handle.is_paused() || !self.chat_still_syncing(&chat_id) {
            self.last_guest_poll = Instant::now();
            return Ok(());
        }
        self.last_guest_poll = Instant::now();
        self.ensure_guest_peer().await;
        let Some(info) = self.peers.get(&chat_id).cloned() else {
            return Ok(());
        };
        let newest = self
            .store
            .newest_message_id(&chat_id)
            .await
            .ok()
            .flatten()
            .unwrap_or(0);

        let mut iter = self.client.iter_messages(info.peer).limit(GUEST_POLL_PAGE);
        let mut page = Vec::new();
        loop {
            match iter.next().await {
                Ok(Some(message)) => {
                    if message.id() <= newest {
                        break;
                    }
                    page.push(message);
                }
                Ok(None) => break,
                Err(err) => {
                    if let Some(secs) = flood_wait_secs(&err) {
                        self.sleep_flood(secs).await;
                    } else {
                        log::warn!("guest poll {chat_id}: {err}");
                        self.emit(
                            DownloadPhase::Live,
                            Some(chat_id),
                            None,
                            Some(guest_poll_detail(&err)),
                        );
                    }
                    return Ok(());
                }
            }
        }

        if page.is_empty() {
            return Ok(());
        }
        self.emit(
            DownloadPhase::Live,
            Some(chat_id.clone()),
            Some(self.display_chat_title(&chat_id, &info.title)),
            Some("未加入轮询".into()),
        );
        for message in page.into_iter().rev() {
            if !self.chat_still_syncing(&chat_id) {
                break;
            }
            match self.process_message(&message, false).await {
                Ok(_) => {}
                Err(err) => {
                    if let Some(secs) = flood_wait_secs(&err) {
                        self.sleep_flood(secs).await;
                        break;
                    }
                    log::warn!("guest poll ingest {chat_id}: {err}");
                }
            }
        }
        self.emit(DownloadPhase::Live, Some(chat_id), None, None);
        Ok(())
    }

    async fn fill_missing_discussion_peers(&mut self) {
        let missing: Vec<(String, String)> = self
            .settings
            .watched_chat_ids
            .iter()
            .filter_map(|id| {
                if self.peers.contains_key(id) {
                    return None;
                }
                self.settings
                    .comment_channel_of(id)
                    .map(|channel_id| (id.clone(), channel_id))
            })
            .collect();
        for (disc_id, channel_id) in missing {
            let Some(channel) = self.peers.get(&channel_id).cloned() else {
                continue;
            };
            match fetch_linked_discussion(&self.client, channel.peer).await {
                Ok(Some(linked)) if linked.group_id == disc_id && linked.joined => {
                    if let Some(peer) = linked.peer {
                        self.peers.insert(
                            disc_id,
                            PeerInfo {
                                peer,
                                title: linked.title,
                            },
                        );
                    }
                }
                Ok(_) => {}
                Err(err) => log::warn!("linked discussion {channel_id}: {err}"),
            }
        }
    }

    async fn handle_update(&mut self, update: Update) -> Result<(), InvocationError> {
        let message = match update {
            Update::NewMessage(message) | Update::MessageEdited(message) => message,
            _ => return Ok(()),
        };
        self.apply_pending_settings().await;
        let chat_id = message.peer_id().to_string();
        if !self.chat_still_syncing(&chat_id) {
            return Ok(());
        }
        let title = self.chat_title(&chat_id, &message);
        self.emit(
            DownloadPhase::Live,
            Some(chat_id.clone()),
            Some(self.display_chat_title(&chat_id, &title)),
            None,
        );
        let downloaded = self.process_message(&message, false).await?;
        if downloaded {
            tokio::time::sleep(Duration::from_millis(jitter_ms(MEDIA_DELAY_MS))).await;
        }
        self.emit(DownloadPhase::Live, Some(chat_id), None, None);
        Ok(())
    }

    async fn step_backfill(&mut self) -> Result<Duration, AppError> {
        self.apply_pending_settings().await;
        if self.handle.is_paused() {
            let phase = if self.backfill.is_some() {
                DownloadPhase::Backfill
            } else {
                self.progress.phase
            };
            self.emit(phase, None, None, Some("已暂停".into()));
            return Ok(Duration::from_millis(800));
        }

        if let Some(req) = self.handle.pop_redownload() {
            if !self.chat_still_syncing(&req.chat_id) {
                self.handle.finish_redownload(&req);
                return Ok(Duration::from_millis(30));
            }
            let result = self
                .force_download_message(&req.chat_id, req.message_id, &req)
                .await;
            self.handle.finish_redownload(&req);
            if let Err(err) = result {
                log::warn!("redownload {}#{}: {err}", req.chat_id, req.message_id);
                self.emit(
                    DownloadPhase::Idle,
                    Some(req.chat_id),
                    None,
                    Some(format!("重新下载失败：{err}")),
                );
            }
            return Ok(Duration::from_millis(jitter_ms(MEDIA_DELAY_MS)));
        }

        if !matches!(
            self.backfill.as_ref().map(|job| job.mode),
            Some(BackfillMode::Check)
        ) {
            if self.handle.has_check() {
                self.backfill = None;
            }
            while let Some(chat_id) = self.handle.pop_check() {
                if let Some(job) = self.start_check_job(&chat_id).await? {
                    self.backfill = Some(job);
                    break;
                }
            }
        }

        if self.backfill.is_none() {
            if let Some(job) = self.next_backfill_job().await? {
                self.backfill = Some(job);
            } else {
                self.emit(DownloadPhase::Idle, None, None, Some(self.idle_detail()));
                return Ok(Duration::from_secs(2));
            }
        }

        let Some(job) = self.backfill.as_mut() else {
            return Ok(Duration::from_secs(2));
        };

        if job.pending.is_empty() {
            if matches!(job.mode, BackfillMode::Types { .. }) {
                return self.fill_type_page().await;
            }
            return self.fill_history_page().await;
        }

        let mut media_jobs = Vec::new();
        let mut ingested = Vec::new();
        let mut claimed = HashSet::new();
        let mut finish_job = false;
        let mut leftover = false;
        let mut hold_cursor = false;
        let mut chat_id = String::new();
        let mut title = String::new();

        loop {
            self.apply_pending_settings().await;
            let (pending_empty, finish_when_empty) = {
                let Some(job) = self.backfill.as_ref() else {
                    break;
                };
                chat_id = job.chat_id.clone();
                title = job.title.clone();
                (job.pending.is_empty(), job.finish_when_empty)
            };
            if pending_empty {
                finish_job = finish_when_empty;
                break;
            }
            if self.handle.is_paused() || !self.chat_still_syncing(&chat_id) {
                leftover = true;
                break;
            }
            let (message, recover) = {
                let Some(job) = self.backfill.as_mut() else {
                    break;
                };
                let Some(message) = job.pending.pop_front() else {
                    finish_job = job.finish_when_empty;
                    break;
                };
                finish_job = job.pending.is_empty() && job.finish_when_empty;
                let recover = matches!(job.mode, BackfillMode::Check);
                (message, recover)
            };
            ingested.push((message.peer_id().to_string(), message.id()));
            match self.ingest_message(&message, false, recover).await {
                Ok(Some(media_job)) => {
                    if claimed.insert(media_job.file_id.clone()) {
                        media_jobs.push(media_job);
                    } else {
                        self.progress.skipped += 1;
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    if let Some(secs) = flood_wait_secs(&err) {
                        self.sleep_flood(secs).await;
                        if let Some(job) = self.backfill.as_mut() {
                            job.pending.push_front(message);
                        }
                        self.flush_index();
                        return Ok(Duration::from_millis(200));
                    }
                    return Err(AppError::Telegram(err.to_string()));
                }
            }
            match self.ingest_post_comments(&message, recover).await {
                Ok(comments) => {
                    for media_job in comments {
                        if claimed.insert(media_job.file_id.clone()) {
                            media_jobs.push(media_job);
                        } else {
                            self.progress.skipped += 1;
                        }
                    }
                }
                Err(err) => {
                    if let Some(secs) = flood_wait_secs(&err) {
                        self.sleep_flood(secs).await;
                        if let Some(job) = self.backfill.as_mut() {
                            job.pending.push_front(message);
                        }
                        hold_cursor = true;
                        finish_job = false;
                        break;
                    }
                    log::warn!("post comments: {err}");
                }
            }
            self.set_queued_jobs(&media_jobs);
            self.sync_progress();
        }

        self.apply_pending_settings().await;
        media_jobs.retain(|job| self.chat_still_syncing(&job.chat_id));
        self.set_queued_jobs(&media_jobs);

        self.emit(
            DownloadPhase::Backfill,
            Some(chat_id.clone()),
            Some(self.display_chat_title(&chat_id, &title)),
            self.job_detail(&chat_id),
        );

        let (downloaded, download_leftover) = if leftover || media_jobs.is_empty() {
            (false, leftover)
        } else {
            self.run_media_jobs(media_jobs).await
        };
        leftover = leftover || download_leftover;
        self.flush_index();
        let leftover = !should_commit_backfill_cursor(leftover, self.chat_still_syncing(&chat_id));

        let mode = self.backfill.as_ref().map(|job| job.mode);
        let page_end_id = self.backfill.as_ref().and_then(|job| job.page_end_id);

        if leftover {
            if let Some(job) = self.backfill.as_mut() {
                job.pending.clear();
                job.finish_when_empty = false;
            }
        } else if matches!(mode, Some(BackfillMode::Types { .. })) {
            if let Some(end) = page_end_id {
                self.advance_type_cursor(&chat_id, end);
            }
        } else if matches!(mode, Some(BackfillMode::Check)) {
            if let Some(job) = self.backfill.as_mut() {
                for (id, message_id) in &ingested {
                    if id == &job.chat_id {
                        job.offset_id = *message_id;
                    }
                }
            }
        } else if !hold_cursor {
            for (id, message_id) in ingested {
                self.advance_backfill_cursor(&id, message_id);
            }
        }
        let _ = self.sync.save(&self.paths.root);

        if leftover {
            return Ok(Duration::from_millis(if self.handle.is_paused() {
                800
            } else {
                30
            }));
        }

        if finish_job && self.chat_still_syncing(&chat_id) {
            self.finish_backfill_job(&chat_id, mode);
            self.backfill = None;
            return Ok(Duration::from_millis(CHAT_DELAY_MS));
        }
        if !self.chat_still_syncing(&chat_id) {
            self.backfill = None;
            return Ok(Duration::from_millis(30));
        }
        if downloaded {
            return Ok(Duration::from_millis(jitter_ms(MEDIA_DELAY_MS)));
        }
        Ok(Duration::from_millis(30))
    }

    async fn next_backfill_job(&mut self) -> Result<Option<BackfillJob>, AppError> {
        let watched: Vec<String> = self.settings.watched_chat_ids.clone();
        if let Some(job) = self.build_backfill_job(&watched, true).await? {
            return Ok(Some(job));
        }
        self.build_backfill_job(&watched, false).await
    }

    async fn build_backfill_job(
        &mut self,
        watched: &[String],
        types_only: bool,
    ) -> Result<Option<BackfillJob>, AppError> {
        for chat_id in watched {
            if !self.chat_still_syncing(chat_id) {
                continue;
            }
            let days = self.settings.effective_backfill_days(chat_id);
            if days == 0 {
                continue;
            }
            let types = self.settings.chat_types(chat_id);
            let (want, applied) = {
                let cursor = self.sync.chats.get(chat_id);
                let applied = cursor
                    .and_then(|cursor| cursor.applied_types)
                    .unwrap_or_default();
                let want = if types_only {
                    cursor
                        .map(|cursor| type_fetch_pending(cursor, days, types))
                        .unwrap_or(false)
                } else {
                    !cursor.map(|cursor| cursor.backfill_done).unwrap_or(false)
                };
                (want, applied)
            };
            if !want {
                continue;
            }
            if !self.peers.contains_key(chat_id) {
                if let Err(err) = self.refresh_peers_for(std::slice::from_ref(chat_id)).await {
                    log::warn!("refresh peers for backfill: {err}");
                    continue;
                }
            }
            let Some(info) = self.peers.get(chat_id) else {
                if self.settings.is_auto_comment(chat_id) {
                    // 未加入时评论走频道帖 getReplies，不要把评论组提前标完成。
                    if let Some(channel_id) = self.settings.comment_channel_of(chat_id) {
                        let channel_done = self
                            .sync
                            .chats
                            .get(&channel_id)
                            .is_some_and(|cursor| cursor.backfill_done);
                        if channel_done {
                            self.mark_backfill_done(chat_id);
                        }
                    }
                } else {
                    log::warn!("cannot resolve peer {chat_id}, skip backfill");
                }
                continue;
            };
            let mode = if types_only {
                BackfillMode::Types {
                    newly: types.newly_enabled(applied),
                }
            } else {
                BackfillMode::History
            };
            return Ok(Some(BackfillJob {
                chat_id: chat_id.clone(),
                title: info.title.clone(),
                peer: info.peer,
                pending: VecDeque::new(),
                finish_when_empty: false,
                mode,
                page_end_id: None,
                offset_id: 0,
            }));
        }
        Ok(None)
    }

    async fn fill_history_page(&mut self) -> Result<Duration, AppError> {
        let Some(job) = self.backfill.as_ref() else {
            return Ok(Duration::from_secs(2));
        };
        let chat_id = job.chat_id.clone();
        let title = job.title.clone();
        let peer = job.peer;
        let check_mode = matches!(job.mode, BackfillMode::Check);
        let (offset, head_id, gap_fill) = if check_mode {
            (job.offset_id, 0, false)
        } else {
            self.sync
                .chats
                .get(&chat_id)
                .map(|cursor| {
                    (
                        cursor.backfill_offset_id,
                        cursor.backfill_head_id,
                        cursor.gap_fill,
                    )
                })
                .unwrap_or((0, 0, false))
        };
        self.emit(
            DownloadPhase::Backfill,
            Some(chat_id.clone()),
            Some(self.display_chat_title(&chat_id, &title)),
            self.job_detail(&chat_id),
        );

        let mut iter = self.client.iter_messages(peer).limit(HISTORY_PAGE);
        if offset > 0 {
            iter = iter.offset_id(offset);
        }

        let mut page = Vec::new();
        while let Some(message) = iter.next().await? {
            page.push(message);
        }

        if page.is_empty() {
            if !check_mode {
                self.mark_backfill_done(&chat_id);
            }
            self.backfill = None;
            return Ok(Duration::from_millis(CHAT_DELAY_MS));
        }

        let days = self.settings.effective_backfill_days(&chat_id);
        let mut pending = VecDeque::new();
        let mut hit_stop = false;
        for message in page {
            if hits_history_stop(
                message.id(),
                is_before_cutoff(message.date(), days),
                head_id,
                gap_fill,
            ) {
                hit_stop = true;
                break;
            }
            pending.push_back(message);
        }
        if pending.is_empty() {
            if !check_mode {
                self.mark_backfill_done(&chat_id);
            }
            self.backfill = None;
            return Ok(Duration::from_millis(CHAT_DELAY_MS));
        }
        if let Some(job) = self.backfill.as_mut() {
            job.pending = pending;
            job.finish_when_empty = hit_stop;
        }
        self.sync_progress();
        Ok(Duration::from_millis(jitter_ms(PAGE_DELAY_MS)))
    }

    async fn fill_type_page(&mut self) -> Result<Duration, AppError> {
        let Some(job) = self.backfill.as_ref() else {
            return Ok(Duration::from_secs(2));
        };
        let BackfillMode::Types { newly } = job.mode else {
            return Ok(Duration::from_millis(30));
        };
        let chat_id = job.chat_id.clone();
        let title = job.title.clone();
        let peer = job.peer;
        let kinds = newly.media_kind_names();
        let days = self.settings.effective_backfill_days(&chat_id);
        let (before_id, min_id) = {
            let cursor = self.sync.chats.get(&chat_id);
            let after = cursor.map(|cursor| cursor.type_after_id).unwrap_or(0);
            let done = cursor.map(|cursor| cursor.backfill_done).unwrap_or(false);
            let offset = cursor.map(|cursor| cursor.backfill_offset_id).unwrap_or(0);
            (
                if after > 0 { Some(after) } else { None },
                if !done && offset > 0 {
                    Some(offset)
                } else {
                    None
                },
            )
        };
        let now = chrono::Utc::now().timestamp();
        let min_date = cutoff_unix(days, now);

        self.emit(
            DownloadPhase::Backfill,
            Some(chat_id.clone()),
            Some(self.display_chat_title(&chat_id, &title)),
            self.job_detail(&chat_id),
        );

        let ids = self
            .store
            .list_media_ids(
                &chat_id,
                &kinds,
                min_date,
                min_id,
                before_id,
                HISTORY_PAGE as i32,
            )
            .await?;

        if ids.is_empty() {
            let oldest = self.store.oldest_date_unix(&chat_id).await.ok().flatten();
            if local_covers_window(oldest, days, now) {
                self.finish_backfill_job(&chat_id, Some(BackfillMode::Types { newly }));
                self.backfill = None;
                return Ok(Duration::from_millis(CHAT_DELAY_MS));
            }
            self.fallback_type_to_history(&chat_id);
            return Ok(Duration::from_millis(30));
        }

        let fetched = match self.client.get_messages_by_id(peer, &ids).await {
            Ok(list) => list,
            Err(err) => {
                if let Some(secs) = flood_wait_secs(&err) {
                    self.sleep_flood(secs).await;
                    return Ok(Duration::from_millis(200));
                }
                return Err(AppError::Telegram(err.to_string()));
            }
        };

        let pending: VecDeque<Message> = fetched.into_iter().flatten().collect();
        let last_id = ids.last().copied();
        let last_page = ids.len() < HISTORY_PAGE;
        if let Some(job) = self.backfill.as_mut() {
            job.pending = pending;
            job.finish_when_empty = last_page;
            job.page_end_id = last_id;
        }
        self.sync_progress();
        if self
            .backfill
            .as_ref()
            .is_some_and(|job| job.pending.is_empty())
        {
            if let Some(end) = last_id {
                self.advance_type_cursor(&chat_id, end);
            }
            if last_page {
                self.finish_backfill_job(&chat_id, Some(BackfillMode::Types { newly }));
                self.backfill = None;
                return Ok(Duration::from_millis(CHAT_DELAY_MS));
            }
            return Ok(Duration::from_millis(30));
        }
        Ok(Duration::from_millis(jitter_ms(PAGE_DELAY_MS)))
    }

    fn finish_backfill_job(&mut self, chat_id: &str, mode: Option<BackfillMode>) {
        if matches!(mode, Some(BackfillMode::Check)) {
            return;
        }
        let history_done = self
            .sync
            .chats
            .get(chat_id)
            .map(|cursor| cursor.backfill_done)
            .unwrap_or(false);
        if matches!(mode, Some(BackfillMode::Types { .. })) && !history_done {
            self.mark_type_ids_done(chat_id);
        } else {
            self.mark_backfill_done(chat_id);
        }
    }

    async fn start_check_job(&mut self, chat_id: &str) -> Result<Option<BackfillJob>, AppError> {
        if !self.chat_still_syncing(chat_id) {
            return Ok(None);
        }
        let days = self.settings.effective_backfill_days(chat_id);
        if days == 0 || !self.settings.chat_types(chat_id).any_media() {
            return Ok(None);
        }
        if !self.peers.contains_key(chat_id) {
            if let Err(err) = self.refresh_peers_for(&[chat_id.to_string()]).await {
                log::warn!("refresh peers for check: {err}");
            }
        }
        let Some(info) = self.peers.get(chat_id) else {
            if !self.settings.is_auto_comment(chat_id) {
                log::warn!("cannot resolve peer {chat_id}, skip check");
            }
            return Ok(None);
        };
        Ok(Some(BackfillJob {
            chat_id: chat_id.to_string(),
            title: info.title.clone(),
            peer: info.peer,
            pending: VecDeque::new(),
            finish_when_empty: false,
            mode: BackfillMode::Check,
            page_end_id: None,
            offset_id: 0,
        }))
    }

    fn mark_backfill_done(&mut self, chat_id: &str) {
        let days = self.settings.effective_backfill_days(chat_id);
        let types = self.settings.chat_types(chat_id);
        let cursor = self.sync.chats.entry(chat_id.to_string()).or_default();
        mark_cursor_done(cursor, days, types);
        let _ = self.sync.save(&self.paths.root);
    }

    fn mark_type_ids_done(&mut self, chat_id: &str) {
        let cursor = self.sync.chats.entry(chat_id.to_string()).or_default();
        cursor.type_ids_done = true;
        cursor.type_after_id = 0;
        let _ = self.sync.save(&self.paths.root);
    }

    fn fallback_type_to_history(&mut self, chat_id: &str) {
        let cursor = self.sync.chats.entry(chat_id.to_string()).or_default();
        cursor.backfill_offset_id = 0;
        cursor.backfill_done = false;
        cursor.type_fallback = true;
        cursor.type_after_id = 0;
        cursor.type_ids_done = false;
        let _ = self.sync.save(&self.paths.root);
        if let Some(job) = self.backfill.as_mut() {
            job.mode = BackfillMode::History;
            job.pending.clear();
            job.finish_when_empty = false;
            job.page_end_id = None;
        }
    }

    fn advance_backfill_cursor(&mut self, chat_id: &str, message_id: i32) {
        self.sync
            .chats
            .entry(chat_id.to_string())
            .or_default()
            .backfill_offset_id = message_id;
    }

    fn note_backfill_head(&mut self, chat_id: &str, message_id: i32) {
        let cursor = self.sync.chats.entry(chat_id.to_string()).or_default();
        if message_id > cursor.backfill_head_id {
            cursor.backfill_head_id = message_id;
        }
    }

    fn advance_type_cursor(&mut self, chat_id: &str, message_id: i32) {
        self.sync
            .chats
            .entry(chat_id.to_string())
            .or_default()
            .type_after_id = message_id;
    }

    async fn process_message(
        &mut self,
        message: &Message,
        from_backfill: bool,
    ) -> Result<bool, InvocationError> {
        let mut jobs = Vec::new();
        if let Some(job) = self.ingest_message(message, from_backfill, false).await? {
            jobs.push(job);
        }
        match self.ingest_post_comments(message, false).await {
            Ok(comments) => jobs.extend(comments),
            Err(err) => {
                if flood_wait_secs(&err).is_some() {
                    return Err(err);
                }
                log::warn!("post comments: {err}");
            }
        }
        let downloaded = if jobs.is_empty() {
            false
        } else if self.handle.is_paused() {
            self.progress.skipped += jobs.len() as u32;
            false
        } else {
            self.run_media_jobs(jobs).await.0
        };
        self.flush_index();
        let _ = self.sync.save(&self.paths.root);
        Ok(downloaded)
    }

    async fn ingest_message(
        &mut self,
        message: &Message,
        commit_cursor: bool,
        recover: bool,
    ) -> Result<Option<MediaJob>, InvocationError> {
        let chat_id = message.peer_id().to_string();
        if !self.chat_still_syncing(&chat_id) {
            return Ok(None);
        }
        if !matches!(message.raw, TlMessage::Message(_)) {
            return Ok(None);
        }

        let id = message.id();
        self.note_backfill_head(&chat_id, id);
        let types = self.settings.chat_types(&chat_id);
        let title = self.chat_title(&chat_id, message);
        let media = message.media();
        let media_kind = media.as_ref().and_then(classify_media);
        let media_file_id = media.as_ref().and_then(media_file_id);

        if types.text && !recover {
            match self
                .store
                .upsert(&MessageRecord {
                    chat_id: chat_id.clone(),
                    message_id: id,
                    date_unix: message.date().timestamp(),
                    sender: sender_label(message),
                    text: message.text().to_string(),
                    media_kind: media_kind.map(|kind| kind.as_str().to_string()),
                    media_file_id: media_file_id.clone(),
                    links: crate::telegram::extract_message_links(message),
                })
                .await
            {
                Ok(true) => self.notify_chat_ingested(&chat_id),
                Ok(false) => self.progress.skipped += 1,
                Err(err) => log::warn!("insert message: {err}"),
            }
        }

        let job = if let (Some(media), Some(kind)) = (media, media_kind) {
            if types.allows_media(kind) {
                match self.take_media_job(&chat_id, &title, kind, &media, id, false, recover) {
                    Ok(Some(job)) => Some(job),
                    Ok(None) => {
                        self.progress.skipped += 1;
                        None
                    }
                    Err(err) => return Err(err),
                }
            } else {
                None
            }
        } else {
            None
        };

        if commit_cursor {
            self.advance_backfill_cursor(&chat_id, id);
        }
        self.progress.processed += 1;
        Ok(job)
    }

    /// 未加入评论组时，按频道帖 getReplies 拉评论。已加入则走群回爬，这里跳过。
    async fn ingest_post_comments(
        &mut self,
        message: &Message,
        recover: bool,
    ) -> Result<Vec<MediaJob>, InvocationError> {
        let channel_id = message.peer_id().to_string();
        let Some(disc_id) = self.settings.discussion_id(&channel_id) else {
            return Ok(Vec::new());
        };
        if self.comment_blocked.contains(&channel_id) {
            return Ok(Vec::new());
        }
        if self.peers.contains_key(&disc_id) {
            return Ok(Vec::new());
        }
        if !self.settings.should_sync_chat(&disc_id) {
            return Ok(Vec::new());
        }
        if !channel_post_may_have_comments(message) {
            return Ok(Vec::new());
        }
        let count = channel_comment_count(message).unwrap_or(0);
        let key = (channel_id.clone(), message.id());
        if let Some(&seen) = self.comment_fetched.get(&key) {
            if count == 0 || seen >= count {
                return Ok(Vec::new());
            }
        }
        let Some(peer) = self.channel_peer(&channel_id, message).await else {
            return Ok(Vec::new());
        };
        let comments = match fetch_post_comments(&self.client, peer, message.id()).await {
            Ok(list) => {
                self.comment_private_streak.remove(&channel_id);
                list
            }
            Err(err) if err.is("MSG_ID_INVALID") || err.is("TOPIC_ID_INVALID") => {
                log::debug!("comments of {channel_id}#{}: {err}", message.id());
                self.comment_fetched.insert(key, count.max(1));
                return Ok(Vec::new());
            }
            Err(err) if err.is("CHANNEL_PRIVATE") || err.is("CHANNEL_INVALID") => {
                let streak = self
                    .comment_private_streak
                    .entry(channel_id.clone())
                    .or_insert(0);
                let (next, blocked) = next_comment_private_streak(*streak);
                *streak = next;
                if blocked {
                    log::warn!("comments of {channel_id} not readable without joining");
                    self.comment_blocked.insert(channel_id);
                } else {
                    log::warn!(
                        "comments of {channel_id}#{}: {err} ({next}/{COMMENT_PRIVATE_LIMIT})",
                        message.id()
                    );
                }
                return Ok(Vec::new());
            }
            Err(err) => return Err(err),
        };
        self.comment_fetched
            .insert(key, if count > 0 { count } else { 1 });
        let days = self.settings.effective_backfill_days(&channel_id);
        let mut jobs = Vec::new();
        for comment in comments {
            if days > 0 {
                if let Some(date) = chrono::DateTime::from_timestamp(comment.date_unix, 0) {
                    if is_before_cutoff(date, days) {
                        continue;
                    }
                }
            }
            if let Some(job) = self.ingest_comment(comment, recover).await? {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    async fn channel_peer(&self, chat_id: &str, message: &Message) -> Option<PeerRef> {
        if let Some(info) = self.peers.get(chat_id) {
            return Some(info.peer);
        }
        message.peer_ref().await.ok().flatten()
    }

    async fn ingest_comment(
        &mut self,
        comment: CommentMessage,
        recover: bool,
    ) -> Result<Option<MediaJob>, InvocationError> {
        let chat_id = comment.chat_id;
        let from_channel = self.settings.comment_channel_of(&chat_id).is_some();
        if !from_channel && !self.chat_still_syncing(&chat_id) {
            return Ok(None);
        }
        let types = if self.settings.chat_types(&chat_id).any() {
            self.settings.chat_types(&chat_id)
        } else if let Some(channel_id) = self.settings.comment_channel_of(&chat_id) {
            self.settings.chat_types(&channel_id)
        } else {
            return Ok(None);
        };
        if !types.any() {
            return Ok(None);
        }
        let title = self.comment_chat_title(&chat_id);
        let media_kind = comment.media.as_ref().and_then(classify_media);
        let media_file_id = comment.media.as_ref().and_then(media_file_id);

        if types.text && !recover {
            match self
                .store
                .upsert(&MessageRecord {
                    chat_id: chat_id.clone(),
                    message_id: comment.message_id,
                    date_unix: comment.date_unix,
                    sender: comment.sender,
                    text: comment.text,
                    media_kind: media_kind.map(|kind| kind.as_str().to_string()),
                    media_file_id: media_file_id.clone(),
                    links: comment.links,
                })
                .await
            {
                Ok(true) => self.notify_chat_ingested(&chat_id),
                Ok(false) => self.progress.skipped += 1,
                Err(err) => log::warn!("insert comment: {err}"),
            }
        }

        let job = if let (Some(media), Some(kind)) = (comment.media, media_kind) {
            if types.allows_media(kind) {
                match self.take_media_job(
                    &chat_id,
                    &title,
                    kind,
                    &media,
                    comment.message_id,
                    false,
                    recover,
                ) {
                    Ok(Some(job)) => Some(job),
                    Ok(None) => {
                        self.progress.skipped += 1;
                        None
                    }
                    Err(err) => return Err(err),
                }
            } else {
                None
            }
        } else {
            None
        };
        self.progress.processed += 1;
        Ok(job)
    }

    fn comment_chat_title(&self, chat_id: &str) -> String {
        if let Some(info) = self.peers.get(chat_id) {
            return self.display_chat_title(chat_id, &info.title);
        }
        if let Some(channel_id) = self.settings.comment_channel_of(chat_id) {
            if let Some(info) = self.peers.get(&channel_id) {
                return format!(
                    "{} 的评论",
                    self.display_chat_title(&channel_id, &info.title)
                );
            }
        }
        self.display_chat_title(chat_id, &format!("#{chat_id}"))
    }

    fn take_media_job(
        &mut self,
        chat_id: &str,
        title: &str,
        kind: MediaKind,
        media: &Media,
        message_id: i32,
        force: bool,
        recover: bool,
    ) -> Result<Option<MediaJob>, InvocationError> {
        let Some(file_id) = media_file_id(media) else {
            return Ok(None);
        };
        if !force && self.handle.is_file_cancelled(&file_id) {
            return Ok(None);
        }
        let indexed = self.index.contains(&file_id);
        let file_exists = self.index.existing_path(&file_id).is_some();
        if !force && skip_indexed_media(indexed, file_exists, recover) {
            return Ok(None);
        }
        if !force && recover && indexed && !file_exists {
            self.index.forget(&file_id);
            self.index_dirty = true;
        }

        let dest = media_path(
            &self.paths.download_dir,
            title,
            chat_id,
            kind,
            &file_id,
            media_original_name(media).as_deref(),
            media_mime(media),
        );
        if force {
            if let Some(old) = self.index.forget(&file_id) {
                let _ = std::fs::remove_file(&old.path);
                let old_part = part_path(&old.path);
                let _ = std::fs::remove_file(&old_part);
                clear_part_off(&old_part);
            }
            let _ = std::fs::remove_file(&dest);
            let dest_part = part_path(&dest);
            let _ = std::fs::remove_file(&dest_part);
            clear_part_off(&dest_part);
            self.index_dirty = true;
            self.flush_index();
        } else if dest.exists() {
            let size = std::fs::metadata(&dest).ok().map(|meta| meta.len());
            self.remember_media(
                file_id,
                dest,
                kind,
                size,
                Some(chat_id.to_string()),
                Some(title.to_string()),
            );
            return Ok(None);
        } else if below_min_media_size(
            media.size().map(|size| size as u64),
            self.settings.effective_min_media_mb(),
        ) {
            return Ok(None);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(InvocationError::from)?;
        }
        Ok(Some(MediaJob {
            file_id,
            kind,
            media: media.clone(),
            dest,
            chat_id: chat_id.to_string(),
            title: title.to_string(),
            message_id,
            total: media.size().map(|size| size as u64),
            force,
            skip_extra: false,
        }))
    }

    async fn force_download_message(
        &mut self,
        chat_id: &str,
        message_id: i32,
        req: &RedownloadRequest,
    ) -> Result<(), AppError> {
        if !self.peers.contains_key(chat_id) {
            self.refresh_peers_for(&[chat_id.to_string()]).await?;
        }
        let Some(info) = self.peers.get(chat_id) else {
            return Err(AppError::Telegram(format!("找不到会话 {chat_id}")));
        };
        let peer = info.peer;
        let title = info.title.clone();
        self.emit(
            DownloadPhase::Live,
            Some(chat_id.to_string()),
            Some(self.display_chat_title(chat_id, &title)),
            Some("重新下载媒体".into()),
        );
        let fetched = match self.client.get_messages_by_id(peer, &[message_id]).await {
            Ok(list) => list,
            Err(err) => {
                if let Some(secs) = flood_wait_secs(&err) {
                    self.sleep_flood(secs).await;
                    self.handle.requeue_redownload(req.clone());
                    return Ok(());
                }
                return Err(AppError::Telegram(err.to_string()));
            }
        };
        let Some(Some(message)) = fetched.into_iter().next() else {
            return Err(AppError::Telegram("Telegram 上找不到这条消息".into()));
        };
        let Some(media) = message.media() else {
            return Err(AppError::Io("该消息没有媒体".into()));
        };
        let Some(kind) = classify_media(&media) else {
            return Err(AppError::Io("不支持的媒体类型".into()));
        };
        let Some(job) =
            self.take_media_job(chat_id, &title, kind, &media, message_id, true, false)?
        else {
            self.notify_chat_ingested(chat_id);
            return Ok(());
        };
        let _ = self.run_media_jobs(vec![job]).await;
        Ok(())
    }

    async fn run_media_jobs(&mut self, jobs: Vec<MediaJob>) -> (bool, bool) {
        self.apply_pending_settings().await;
        let mut pending: Vec<MediaJob> = jobs
            .into_iter()
            .filter(|job| self.chat_still_syncing(&job.chat_id))
            .collect();
        let mut any = false;
        let mut leftover = false;
        let mut network_stall_streak = 0u8;
        let mut file_stall_counts: HashMap<String, u8> = HashMap::new();
        let mut set: JoinSet<MediaJobOutcome> = JoinSet::new();
        let mut in_flight_large = 0usize;
        let mut wait_until: Option<Instant> = None;
        let mut wait_is_flood = false;
        let mut wait_detail: Option<&str> = None;
        loop {
            self.apply_pending_settings().await;
            self.clear_retries_on_resume();
            let mut skipped = Vec::new();
            pending.retain(|job| {
                if !self.chat_still_syncing(&job.chat_id) {
                    skipped.push(job.file_id.clone());
                    false
                } else if !job.force && self.handle.is_file_cancelled(&job.file_id) {
                    skipped.push(job.file_id.clone());
                    false
                } else {
                    true
                }
            });
            for file_id in skipped {
                self.handle.clear_file_progress(&self.ctx.events, &file_id);
                self.progress.skipped += 1;
                self.download_retried.remove(&file_id);
            }

            if self.handle.is_paused() {
                leftover = true;
                wait_until = None;
                wait_is_flood = false;
                wait_detail = None;
                self.skip_pending_jobs(&mut pending);
                if set.is_empty() {
                    break;
                }
            } else {
                if wait_until.is_some_and(|until| Instant::now() >= until) {
                    wait_until = None;
                    wait_is_flood = false;
                    wait_detail = None;
                }
                if wait_until.is_none() {
                    let concurrency =
                        self.settings.effective_download_concurrency().max(1) as usize;
                    while let Some(job) = take_next_ready_job(
                        &mut pending,
                        set.len(),
                        in_flight_large,
                        concurrency,
                        MAX_PARALLEL_LARGE,
                        |job| is_large_media(job.total),
                    ) {
                        let large = is_large_media(job.total);
                        let lane_budget = download_lane_budget(
                            job.skip_extra,
                            large,
                            in_flight_large,
                            pending
                                .iter()
                                .filter(|job| is_large_media(job.total))
                                .count(),
                        );
                        self.spawn_download_job(&mut set, job, lane_budget);
                        if large {
                            in_flight_large += 1;
                        }
                    }
                }
            }
            self.set_queued_jobs(&pending);
            self.sync_progress();

            if set.is_empty() {
                if pending.is_empty() {
                    break;
                }
                if self.handle.is_paused() {
                    break;
                }
                if let Some(until) = wait_until.take() {
                    let secs = until.saturating_duration_since(Instant::now()).as_secs();
                    let is_flood = wait_is_flood;
                    let detail = wait_detail.take();
                    wait_is_flood = false;
                    if secs > 0 {
                        if is_flood {
                            self.sleep_flood(secs).await;
                        } else {
                            self.sleep_timeout_backoff(secs, detail).await;
                        }
                    }
                    continue;
                }
                break;
            }

            let joined = if let Some(until) = wait_until.filter(|until| Instant::now() < *until) {
                let left = until.saturating_duration_since(Instant::now());
                tokio::select! {
                    joined = set.join_next() => joined,
                    _ = tokio::time::sleep(left) => {
                        wait_until = None;
                        wait_is_flood = false;
                        wait_detail = None;
                        continue;
                    }
                    _ = self.handle.notified() => continue,
                }
            } else {
                tokio::select! {
                    joined = set.join_next() => joined,
                    _ = self.handle.notified() => continue,
                }
            };
            let Some(joined) = joined else {
                continue;
            };
            match joined {
                Ok((large, Ok(done))) => {
                    if large {
                        in_flight_large = in_flight_large.saturating_sub(1);
                    }
                    self.handle
                        .clear_file_progress(&self.ctx.events, &done.job.file_id);
                    let size = done.size;
                    self.remember_media(
                        done.job.file_id,
                        done.job.dest,
                        done.job.kind,
                        size,
                        Some(done.job.chat_id),
                        Some(done.job.title),
                    );
                    self.progress.downloaded += 1;
                    any = true;
                    network_stall_streak = 0;
                }
                Ok((large, Err((mut job, err)))) => {
                    if large {
                        in_flight_large = in_flight_large.saturating_sub(1);
                    }
                    if task_error_blocks_cursor(&err) {
                        leftover = true;
                    }
                    match err {
                        DownloadTaskError::Flood(secs) => {
                            job.skip_extra = true;
                            note_flood_until(&mut wait_until, secs);
                            wait_is_flood = true;
                            wait_detail = None;
                            pending.insert(0, job);
                        }
                        DownloadTaskError::Cancelled { .. } => {
                            self.handle
                                .clear_file_progress(&self.ctx.events, &job.file_id);
                            self.progress.skipped += 1;
                        }
                        DownloadTaskError::Timeout
                        | DownloadTaskError::ExpiredRef
                        | DownloadTaskError::Other(_) => {
                            self.handle
                                .clear_file_progress(&self.ctx.events, &job.file_id);
                            if let DownloadTaskError::Other(text) = &err {
                                log::warn!("download media failed: {text}");
                            }
                            let file_over = if is_network_stall(&err) {
                                let count =
                                    file_stall_counts.entry(job.file_id.clone()).or_insert(0);
                                let (next, over) = next_file_stall_count(*count);
                                *count = next;
                                network_stall_streak =
                                    next_network_stall_streak(network_stall_streak).0;
                                over
                            } else {
                                false
                            };
                            let already_retried = self.download_retried.contains(&job.file_id);
                            match media_fail_action(
                                &err,
                                network_stall_streak,
                                file_over,
                                already_retried,
                            ) {
                                MediaFailAction::Hold => {
                                    leftover = true;
                                    self.download_retried.remove(&job.file_id);
                                    note_flood_until(&mut wait_until, NETWORK_STALL_WAIT_SECS);
                                    wait_is_flood = false;
                                    wait_detail = Some(NETWORK_STALL_WAIT_DETAIL);
                                    self.emit(
                                        self.progress.phase,
                                        None,
                                        None,
                                        Some(NETWORK_STALL_WAIT_DETAIL.into()),
                                    );
                                    log::warn!(
                                        "network stall on {} ({}/{NETWORK_STALL_LIMIT}), holding backfill cursor",
                                        job.file_id,
                                        network_stall_streak
                                    );
                                    pending.insert(0, job);
                                }
                                MediaFailAction::Retry => {
                                    note_download_retry(&mut self.download_retried, &job.file_id);
                                    let mut job = job;
                                    if should_refresh_media(&err) {
                                        if let Err(refresh_err) =
                                            self.refresh_job_media(&mut job).await
                                        {
                                            if let Some(secs) = flood_wait_secs(&refresh_err) {
                                                job.skip_extra = true;
                                                note_flood_until(&mut wait_until, secs);
                                                wait_is_flood = true;
                                                wait_detail = None;
                                            } else {
                                                log::warn!(
                                                    "refresh media {}: {refresh_err}",
                                                    job.file_id
                                                );
                                            }
                                        }
                                    }
                                    if let Some(secs) = timeout_retry_backoff_secs(&err) {
                                        let had_wait = wait_until.is_some();
                                        note_flood_until(&mut wait_until, secs);
                                        if !had_wait {
                                            wait_is_flood = false;
                                            wait_detail = None;
                                        }
                                    }
                                    pending.insert(0, job);
                                }
                                MediaFailAction::Pause => {
                                    leftover = true;
                                    self.progress.skipped += 1;
                                    self.pause_after_stall();
                                }
                                MediaFailAction::Skip { reset_streak } => {
                                    if reset_streak {
                                        network_stall_streak = 0;
                                    }
                                    self.progress.skipped += 1;
                                    self.download_retried.remove(&job.file_id);
                                    discard_partial(&job.dest);
                                    log::warn!(
                                        "skip media {} after retries ({})",
                                        job.file_id,
                                        download_error_label(&err)
                                    );
                                }
                            }
                        }
                    }
                }
                Err(err) => log::warn!("download task: {err}"),
            }
            in_flight_large = in_flight_large.min(set.len());
            self.flush_index();
        }
        self.queued_jobs.clear();
        self.flush_index();
        self.sync_progress();
        (any, leftover)
    }

    fn skip_pending_jobs(&mut self, pending: &mut Vec<MediaJob>) {
        for job in pending.drain(..) {
            self.handle
                .clear_file_progress(&self.ctx.events, &job.file_id);
            self.progress.skipped += 1;
            self.download_retried.remove(&job.file_id);
        }
    }

    fn spawn_download_job(
        &self,
        set: &mut JoinSet<MediaJobOutcome>,
        job: MediaJob,
        lane_budget: usize,
    ) {
        let large = is_large_media(job.total);
        let start = part_resume_len(&part_path(&job.dest));
        self.handle
            .report_file_progress(&self.ctx.events, job.active_item(start));
        let client = self.client.clone();
        let handle = self.handle.clone();
        let events = self.ctx.events.clone();
        let pool = self.media_pool.clone();
        let proxy_url = self.settings.effective_proxy_url();
        set.spawn(async move {
            (
                large,
                execute_media_job(client, pool, proxy_url, job, handle, events, lane_budget).await,
            )
        });
    }

    fn set_queued_jobs(&mut self, jobs: &[MediaJob]) {
        self.queued_jobs = jobs
            .iter()
            .filter(|job| self.chat_still_syncing(&job.chat_id))
            .take(QUEUE_PREVIEW_LIMIT)
            .map(MediaJob::queued_item)
            .collect();
    }

    fn remember_media(
        &mut self,
        file_id: String,
        dest: PathBuf,
        kind: MediaKind,
        size: Option<u64>,
        chat_id: Option<String>,
        chat_title: Option<String>,
    ) {
        self.download_retried.remove(&file_id);
        self.index
            .remember(file_id, dest, kind, size, chat_id.clone(), chat_title);
        self.index_dirty = true;
        if let Some(chat_id) = chat_id {
            self.notify_chat_ingested(&chat_id);
        }
    }

    fn flush_index(&mut self) {
        if !self.index_dirty {
            return;
        }
        match self.index.save(&self.paths.root) {
            Ok(()) => self.index_dirty = false,
            Err(err) => log::warn!("save media-index: {err}"),
        }
    }

    fn notify_chat_ingested(&self, chat_id: &str) {
        if chat_id.is_empty() {
            return;
        }
        self.ctx.events.emit_chat_ingested(ChatIngested {
            chat_id: chat_id.to_string(),
        });
    }

    fn chat_title(&self, chat_id: &str, message: &Message) -> String {
        self.peers
            .get(chat_id)
            .map(|info| info.title.clone())
            .or_else(|| {
                message
                    .peer()
                    .and_then(|peer| peer.name().map(str::to_string))
            })
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| format!("#{chat_id}"))
    }

    fn display_chat_title(&self, chat_id: &str, title: &str) -> String {
        self.settings.display_title(chat_id, title)
    }

    fn queued_preview(&self) -> Vec<QueuedDownload> {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = self
            .handle
            .snapshot()
            .active
            .iter()
            .map(|item| item.file_id.clone())
            .collect();
        for item in &self.queued_jobs {
            if out.len() >= QUEUE_PREVIEW_LIMIT {
                break;
            }
            if !self.queued_chat_wanted(item.chat_id.as_deref()) {
                continue;
            }
            if !seen.insert(item.file_id.clone()) {
                continue;
            }
            out.push(item.clone());
        }
        for req in self.handle.peek_redownloads(QUEUE_PREVIEW_LIMIT) {
            if out.len() >= QUEUE_PREVIEW_LIMIT {
                break;
            }
            if !self.chat_still_syncing(&req.chat_id) {
                continue;
            }
            let file_id = req
                .file_id
                .clone()
                .unwrap_or_else(|| format!("redownload:{}:{}", req.chat_id, req.message_id));
            if !seen.insert(file_id.clone()) {
                continue;
            }
            out.push(QueuedDownload {
                file_id,
                file_name: req
                    .file_name
                    .unwrap_or_else(|| format!("消息 {}", req.message_id)),
                kind: req.kind.unwrap_or(MediaKind::Document),
                chat_id: Some(req.chat_id),
                message_id: Some(req.message_id),
            });
        }
        if let Some(job) = &self.backfill {
            if self.chat_still_syncing(&job.chat_id) {
                for message in &job.pending {
                    if out.len() >= QUEUE_PREVIEW_LIMIT {
                        break;
                    }
                    if let Some(item) = self.preview_queued_message(message, &mut seen) {
                        out.push(item);
                    }
                }
            }
        }
        out
    }

    fn preview_queued_message(
        &self,
        message: &Message,
        seen: &mut HashSet<String>,
    ) -> Option<QueuedDownload> {
        if !matches!(message.raw, TlMessage::Message(_)) {
            return None;
        }
        let media = message.media()?;
        let kind = classify_media(&media)?;
        let chat_id = message.peer_id().to_string();
        if !self.chat_still_syncing(&chat_id) {
            return None;
        }
        if !self.settings.chat_types(&chat_id).allows_media(kind) {
            return None;
        }
        let file_id = media_file_id(&media)?;
        if !seen.insert(file_id.clone()) {
            return None;
        }
        if self.index.contains(&file_id) || self.handle.is_file_cancelled(&file_id) {
            return None;
        }
        if below_min_media_size(
            media.size().map(|size| size as u64),
            self.settings.effective_min_media_mb(),
        ) {
            return None;
        }
        Some(QueuedDownload {
            file_id: file_id.clone(),
            file_name: media_original_name(&media).unwrap_or(file_id),
            kind,
            chat_id: Some(chat_id),
            message_id: Some(message.id()),
        })
    }

    fn sync_progress(&mut self) {
        self.progress.paused = self.handle.is_paused();
        self.progress.queued = self.queued_preview();
        let counters = (
            self.progress.phase,
            self.progress.chat_id.clone(),
            self.progress.chat_title.clone(),
            self.progress.processed,
            self.progress.downloaded,
            self.progress.skipped,
            self.progress.flood_wait_secs,
            self.progress.detail.clone(),
            self.progress.paused,
            self.progress.queued.clone(),
        );
        self.handle.patch_status(&self.ctx.events, |status| {
            status.phase = counters.0;
            status.chat_id = counters.1.clone();
            status.chat_title = counters.2.clone();
            status.processed = counters.3;
            status.downloaded = counters.4;
            status.skipped = counters.5;
            status.flood_wait_secs = counters.6;
            status.detail = counters.7.clone();
            status.paused = counters.8;
            status.queued = counters.9.clone();
            if let Some(first) = status.active.first() {
                status.current_file = Some(first.file_name.clone());
                status.current_kind = Some(first.kind);
            } else {
                status.current_file = None;
                status.current_kind = None;
            }
        });
    }

    fn emit(
        &mut self,
        phase: DownloadPhase,
        chat_id: Option<String>,
        chat_title: Option<String>,
        detail: Option<String>,
    ) {
        if let Some(chat_id) = chat_id {
            self.progress.chat_id = Some(chat_id);
        }
        if let Some(chat_title) = chat_title {
            self.progress.chat_title = Some(chat_title);
        }
        self.progress.phase = phase;
        self.progress.detail = detail;
        if phase != DownloadPhase::FloodWait {
            self.progress.flood_wait_secs = None;
        }
        self.sync_progress();
    }

    fn clear_retries_on_resume(&mut self) {
        let paused = self.handle.is_paused();
        if self.progress.paused && !paused {
            self.download_retried.clear();
        }
        self.progress.paused = paused;
    }

    fn pause_after_stall(&mut self) {
        self.settings.download_paused = true;
        let _ = self.settings.save(&self.paths.root);
        self.handle.set_paused(true);
        self.progress.paused = true;
        self.emit(
            self.progress.phase,
            None,
            None,
            Some("下载失败，已暂停".into()),
        );
    }

    async fn refresh_job_media(&mut self, job: &mut MediaJob) -> Result<(), InvocationError> {
        if job.message_id <= 0 {
            return Ok(());
        }
        let Some(peer) = self.peers.get(&job.chat_id).map(|info| info.peer) else {
            return Ok(());
        };
        let fetched = self
            .client
            .get_messages_by_id(peer, &[job.message_id])
            .await?;
        let Some(Some(message)) = fetched.into_iter().next() else {
            return Ok(());
        };
        let Some(media) = message.media() else {
            return Ok(());
        };
        if media_file_id(&media).as_deref() != Some(job.file_id.as_str()) {
            return Ok(());
        }
        job.total = media.size().map(|size| size as u64);
        job.media = media;
        Ok(())
    }

    async fn sleep_flood(&mut self, secs: u64) {
        let pause = flood_pause_secs(secs);
        self.progress.flood_wait_secs = Some(pause as u32);
        self.emit(
            DownloadPhase::FloodWait,
            None,
            None,
            Some(format!("Telegram 限流，等待 {pause} 秒")),
        );
        self.sleep_until_or_stop(Instant::now() + Duration::from_secs(pause))
            .await;
    }

    async fn sleep_timeout_backoff(&mut self, secs: u64, detail: Option<&str>) {
        self.emit(
            self.progress.phase,
            None,
            None,
            Some(
                detail
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("下载超时，{secs} 秒后重试")),
            ),
        );
        self.sleep_until_or_stop(Instant::now() + Duration::from_secs(secs))
            .await;
    }

    async fn sleep_until_or_stop(&mut self, until: Instant) {
        loop {
            if self.handle.should_stop() {
                return;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            if self.progress.phase == DownloadPhase::FloodWait {
                let secs = remaining_wait_secs(left);
                if self.progress.flood_wait_secs != Some(secs) {
                    self.progress.flood_wait_secs = Some(secs);
                    self.progress.detail = Some(format!("Telegram 限流，等待 {secs} 秒"));
                    self.sync_progress();
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(left.min(Duration::from_secs(1))) => {}
                _ = self.handle.notified() => {
                    if self.handle.should_stop() {
                        return;
                    }
                }
            }
        }
    }
}

type MediaJobOutcome = (bool, Result<FinishedMedia, (MediaJob, DownloadTaskError)>);

struct MediaJob {
    file_id: String,
    kind: MediaKind,
    media: Media,
    dest: PathBuf,
    chat_id: String,
    title: String,
    message_id: i32,
    total: Option<u64>,
    force: bool,
    skip_extra: bool,
}

impl MediaJob {
    fn file_name(&self) -> String {
        self.dest
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| self.file_id.clone())
    }

    fn active_item(&self, bytes: u64) -> ActiveDownload {
        ActiveDownload {
            file_id: self.file_id.clone(),
            file_name: self.file_name(),
            kind: self.kind,
            bytes: bytes.to_string(),
            total: self.total.map(|size| size.to_string()),
        }
    }

    fn queued_item(&self) -> QueuedDownload {
        QueuedDownload {
            file_id: self.file_id.clone(),
            file_name: self.file_name(),
            kind: self.kind,
            chat_id: Some(self.chat_id.clone()),
            message_id: Some(self.message_id),
        }
    }
}

struct FinishedMedia {
    job: MediaJob,
    size: Option<u64>,
}

enum DownloadTaskError {
    Flood(u64),
    Timeout,
    ExpiredRef,
    Cancelled { keep_part: bool },
    Other(String),
}

fn note_download_retry(retried: &mut HashSet<String>, file_id: &str) -> bool {
    retried.insert(file_id.to_string())
}

fn should_refresh_media(err: &DownloadTaskError) -> bool {
    matches!(
        err,
        DownloadTaskError::ExpiredRef | DownloadTaskError::Other(_)
    )
}

fn timeout_retry_backoff_secs(err: &DownloadTaskError) -> Option<u64> {
    matches!(err, DownloadTaskError::Timeout).then_some(TIMEOUT_RETRY_BACKOFF_SECS)
}

fn should_pause_after_retries(err: &DownloadTaskError) -> bool {
    match err {
        DownloadTaskError::Other(text) => is_local_fatal_download(text),
        DownloadTaskError::Timeout
        | DownloadTaskError::ExpiredRef
        | DownloadTaskError::Flood(_)
        | DownloadTaskError::Cancelled { .. } => false,
    }
}

fn is_local_fatal_download(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("enospc")
        || lower.contains("no space")
        || lower.contains("not enough space")
        || lower.contains("os error 28")
        || text.contains("空间不足")
        || text.contains("磁盘已满")
        || text.contains("没有空间")
}

fn download_error_label(err: &DownloadTaskError) -> String {
    match err {
        DownloadTaskError::Timeout => "timeout".into(),
        DownloadTaskError::ExpiredRef => "失效文件/FILE_REFERENCE".into(),
        DownloadTaskError::Other(text) => text.clone(),
        DownloadTaskError::Flood(secs) => format!("flood {secs}s"),
        DownloadTaskError::Cancelled { .. } => "cancelled".into(),
    }
}

fn is_expired_ref(err: &InvocationError) -> bool {
    err.is("FILE_REFERENCE*")
}

fn is_expired_ref_text(text: &str) -> bool {
    text.to_ascii_uppercase().contains("FILE_REFERENCE")
}

fn is_stale_conn_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("超时")
        || text.contains("连接断开")
        || text.contains("read 0 bytes")
        || text.contains("connection reset")
        || text.contains("broken pipe")
}

fn is_network_stall(err: &DownloadTaskError) -> bool {
    match err {
        DownloadTaskError::Timeout => true,
        DownloadTaskError::Other(text) => is_stale_conn_text(text),
        DownloadTaskError::ExpiredRef
        | DownloadTaskError::Flood(_)
        | DownloadTaskError::Cancelled { .. } => false,
    }
}

fn cancel_error(handle: &SyncHandle) -> DownloadTaskError {
    DownloadTaskError::Cancelled {
        keep_part: handle.keep_partial_file(),
    }
}

fn finish_part(tmp: &Path, dest: &Path) -> Result<(), String> {
    if let Err(err) = std::fs::rename(tmp, dest) {
        let _ = std::fs::copy(tmp, dest);
        let _ = std::fs::remove_file(tmp);
        if !dest.exists() {
            return Err(err.to_string());
        }
    }
    Ok(())
}

async fn execute_media_job(
    client: Client,
    pool: Arc<MediaPool>,
    proxy_url: Option<String>,
    mut job: MediaJob,
    handle: SyncHandle,
    events: EventHub,
    lane_budget: usize,
) -> Result<FinishedMedia, (MediaJob, DownloadTaskError)> {
    if handle.should_cancel_media(&job.file_id, &job.chat_id, job.force) {
        return Err((job, cancel_error(&handle)));
    }

    let tmp = part_path(&job.dest);
    let aligned = part_resume_len(&tmp);
    if job.total.is_some_and(|total| total > 0 && aligned >= total) {
        clear_part_off(&tmp);
        if let Err(err) = finish_part(&tmp, &job.dest) {
            return Err((job, DownloadTaskError::Other(err)));
        }
        let size = std::fs::metadata(&job.dest)
            .ok()
            .map(|meta| meta.len())
            .or(job.total);
        return Ok(FinishedMedia { job, size });
    }

    let existing = std::fs::metadata(&tmp)
        .ok()
        .map(|meta| meta.len())
        .unwrap_or(0);
    if existing != aligned {
        if let Err(err) = std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
            .and_then(|file| file.set_len(aligned))
        {
            return Err((job, DownloadTaskError::Other(err.to_string())));
        }
    }
    clear_part_off(&tmp);
    handle.report_file_progress(&events, job.active_item(aligned));

    if let Some(total) = job.total.filter(|size| MediaPool::should_parallel(*size)) {
        match TelegramApi::from_env() {
            Ok(api) => {
                let file_id = job.file_id.clone();
                let chat_id = job.chat_id.clone();
                let force = job.force;
                let handle_ref = handle.clone();
                let events_ref = events.clone();
                match pool
                    .download(
                        &client,
                        api.api_id,
                        proxy_url.clone(),
                        &job.media,
                        &tmp,
                        aligned,
                        total,
                        lane_budget,
                        || handle_ref.should_cancel_media(&file_id, &chat_id, force),
                        || handle_ref.keep_partial_file(),
                        |bytes| {
                            handle_ref.report_file_progress(&events_ref, job.active_item(bytes));
                        },
                    )
                    .await
                {
                    Ok(downloaded) => {
                        handle.report_file_progress(&events, job.active_item(downloaded));
                        if let Err(err) = finish_part(&tmp, &job.dest) {
                            return Err((job, DownloadTaskError::Other(err)));
                        }
                        let size = std::fs::metadata(&job.dest)
                            .ok()
                            .map(|meta| meta.len())
                            .or(Some(downloaded));
                        return Ok(FinishedMedia { job, size });
                    }
                    Err(ParallelError::Flood(secs)) => {
                        job.skip_extra = true;
                        return Err((job, DownloadTaskError::Flood(secs)));
                    }
                    Err(ParallelError::HomeDc) => {
                        log::info!("media extra is current DC, fallback sequential");
                    }
                    Err(ParallelError::Cancelled { keep_part }) => {
                        if !keep_part {
                            let _ = std::fs::remove_file(&tmp);
                            clear_part_off(&tmp);
                        }
                        return Err((job, DownloadTaskError::Cancelled { keep_part }));
                    }
                    Err(ParallelError::ExpiredRef) => {
                        pool.invalidate().await;
                        return Err((job, DownloadTaskError::ExpiredRef));
                    }
                    Err(ParallelError::Other(err)) => {
                        if is_stale_conn_text(&err) {
                            pool.invalidate().await;
                            return Err((job, DownloadTaskError::Timeout));
                        }
                        if is_expired_ref_text(&err) {
                            pool.invalidate().await;
                            return Err((job, DownloadTaskError::ExpiredRef));
                        }
                        log::warn!("parallel download failed, fallback sequential: {err}");
                        pool.invalidate().await;
                    }
                }
            }
            Err(err) => log::warn!("parallel download skipped: {err}"),
        }
    }

    let existing = std::fs::metadata(&tmp)
        .ok()
        .map(|meta| meta.len())
        .unwrap_or(aligned);
    let aligned = aligned_part_len(existing);
    if existing != aligned {
        if let Err(err) = std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
            .and_then(|file| file.set_len(aligned))
        {
            return Err((job, DownloadTaskError::Other(err.to_string())));
        }
    }

    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&tmp)
    {
        Ok(mut file) => {
            if let Err(err) = file.seek(SeekFrom::Start(aligned)) {
                return Err((job, DownloadTaskError::Other(err.to_string())));
            }
            file
        }
        Err(err) => return Err((job, DownloadTaskError::Other(err.to_string()))),
    };

    let mut downloaded = aligned;
    let mut last_emit = Instant::now() - PROGRESS_EVERY;
    handle.report_file_progress(&events, job.active_item(downloaded));

    let mut iter = client.iter_download(&job.media);
    let skip = skip_chunks(aligned);
    if skip > 0 {
        iter = iter.skip_chunks(skip);
    }
    let result = loop {
        if handle.should_cancel_media(&job.file_id, &job.chat_id, job.force) {
            break Err(cancel_error(&handle));
        }
        match tokio::time::timeout(CHUNK_TIMEOUT, iter.next()).await {
            Err(_) => break Err(DownloadTaskError::Timeout),
            Ok(Ok(Some(chunk))) => {
                if let Err(err) = file.write_all(&chunk) {
                    break Err(DownloadTaskError::Other(err.to_string()));
                }
                downloaded += chunk.len() as u64;
                if last_emit.elapsed() >= PROGRESS_EVERY {
                    handle.report_file_progress(&events, job.active_item(downloaded));
                    last_emit = Instant::now();
                }
            }
            Ok(Ok(None)) => break Ok(()),
            Ok(Err(err)) => {
                break Err(if let Some(secs) = flood_wait_secs(&err) {
                    DownloadTaskError::Flood(secs)
                } else if is_expired_ref(&err) {
                    DownloadTaskError::ExpiredRef
                } else if is_stale_conn_text(&err.to_string()) {
                    DownloadTaskError::Timeout
                } else {
                    DownloadTaskError::Other(err.to_string())
                });
            }
        }
    };

    drop(file);
    match result {
        Ok(()) => {
            handle.report_file_progress(&events, job.active_item(downloaded));
            if let Err(err) = finish_part(&tmp, &job.dest) {
                return Err((job, DownloadTaskError::Other(err)));
            }
            let size = std::fs::metadata(&job.dest)
                .ok()
                .map(|meta| meta.len())
                .or(Some(downloaded));
            Ok(FinishedMedia { job, size })
        }
        Err(err) => {
            let keep = matches!(
                err,
                DownloadTaskError::Flood(_)
                    | DownloadTaskError::Timeout
                    | DownloadTaskError::ExpiredRef
                    | DownloadTaskError::Other(_)
                    | DownloadTaskError::Cancelled { keep_part: true }
            );
            if !keep {
                let _ = std::fs::remove_file(&tmp);
                clear_part_off(&tmp);
            }
            Err((job, err))
        }
    }
}

fn sender_label(message: &Message) -> String {
    message
        .sender()
        .and_then(|peer| {
            peer.name()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .or_else(|| Some(peer.id().to_string()))
        })
        .unwrap_or_else(|| "unknown".into())
}

fn is_large_media(total: Option<u64>) -> bool {
    total.is_some_and(MediaPool::should_parallel)
}

/// 槽位空出时立刻补下一个。大文件已满则跳过它们，先开小文件把槽填上。
fn take_next_ready_job<T>(
    pending: &mut Vec<T>,
    in_flight: usize,
    in_flight_large: usize,
    concurrency: usize,
    max_large: usize,
    is_large: impl Fn(&T) -> bool,
) -> Option<T> {
    if in_flight >= concurrency.max(1) || pending.is_empty() {
        return None;
    }
    let idx = if in_flight_large >= max_large {
        pending.iter().position(|job| !is_large(job))?
    } else {
        0
    };
    Some(pending.remove(idx))
}

fn large_lane_budget(in_flight_large: usize, pending_large: usize) -> usize {
    MediaPool::lanes_per_file(in_flight_large + 1 + pending_large)
}

fn download_lane_budget(
    skip_extra: bool,
    large: bool,
    in_flight_large: usize,
    pending_large: usize,
) -> usize {
    if skip_extra {
        0
    } else if large {
        large_lane_budget(in_flight_large, pending_large)
    } else {
        MediaPool::lanes_per_file(1)
    }
}

fn flood_pause_secs(secs: u64) -> u64 {
    if secs > FLOOD_LONG_SECS {
        FLOOD_PAUSE_SECS.max(secs + 5)
    } else {
        secs + 5
    }
}

fn remaining_wait_secs(left: Duration) -> u32 {
    if left.is_zero() {
        0
    } else {
        u32::try_from(left.as_secs().max(1)).unwrap_or(u32::MAX)
    }
}

fn note_flood_until(flood_until: &mut Option<Instant>, secs: u64) {
    let until = Instant::now() + Duration::from_secs(secs.max(1));
    *flood_until = Some(flood_until.map_or(until, |prev| prev.max(until)));
}

fn jitter_ms(range: (u64, u64)) -> u64 {
    let (min, max) = range;
    if max <= min {
        return min;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    min + now % (max - min + 1)
}

fn guest_poll_detail(err: &InvocationError) -> String {
    if err.is("CHANNEL_PRIVATE") || err.is("CHAT_PRIVATE") || err.is("CHANNEL_INVALID") {
        "该公开群需要加入才能读".into()
    } else {
        format!("未加入轮询失败：{err}")
    }
}

fn flood_wait_secs(err: &InvocationError) -> Option<u64> {
    match err {
        InvocationError::Rpc(rpc) if rpc.code == 420 || rpc.name == "FLOOD_WAIT" => {
            rpc.value.map(|value| value as u64)
        }
        _ => None,
    }
}

fn flood_wait_secs_app(err: &AppError) -> Option<u64> {
    match err {
        AppError::Telegram(message) => {
            let message = message.to_ascii_uppercase();
            if let Some(rest) = message.split("FLOOD_WAIT").nth(1) {
                let digits: String = rest
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .take(6)
                    .collect();
                if let Ok(secs) = digits.parse::<u64>() {
                    return Some(secs);
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_chat_stops_job_until_allowed() {
        let handle = SyncHandle::new();
        assert!(!handle.should_cancel_job("f1", "c1"));
        handle.cancel_chat("c1".into());
        assert!(handle.should_cancel_job("f1", "c1"));
        assert!(!handle.should_cancel_job("f1", "c2"));
        assert!(!handle.should_cancel_media("f1", "c1", false));
        handle.allow_chat("c1");
        assert!(!handle.should_cancel_job("f1", "c1"));
    }

    #[test]
    fn cancel_file_still_works() {
        let handle = SyncHandle::new();
        handle.cancel_file("f1".into());
        assert!(handle.should_cancel_job("f1", "c1"));
        assert!(!handle.should_cancel_job("f2", "c1"));
    }

    #[test]
    fn take_next_ready_job_refills_slot() {
        let mut pending = vec![1, 2, 3, 4, 5];
        let is_large = |_: &i32| false;
        let mut got = Vec::new();
        let mut in_flight = 0;
        while let Some(job) = take_next_ready_job(&mut pending, in_flight, 0, 4, 4, is_large) {
            got.push(job);
            in_flight += 1;
        }
        assert_eq!(got, vec![1, 2, 3, 4]);
        assert_eq!(pending, vec![5]);
        in_flight -= 1;
        assert_eq!(
            take_next_ready_job(&mut pending, in_flight, 0, 4, 4, is_large),
            Some(5)
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn take_next_ready_job_skips_large_when_cap_full() {
        let mut pending = vec![(1, true), (2, false), (3, true)];
        let job = take_next_ready_job(&mut pending, 4, 4, 8, 4, |job| job.1);
        assert_eq!(job, Some((2, false)));
        assert_eq!(pending, vec![(1, true), (3, true)]);
    }

    #[test]
    fn take_next_ready_job_waits_when_only_large_and_cap_full() {
        let mut pending = vec![(1, true), (2, true)];
        let job = take_next_ready_job(&mut pending, 4, 4, 8, 4, |job| job.1);
        assert!(job.is_none());
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn large_lane_budget_looks_ahead() {
        assert_eq!(large_lane_budget(0, 3), 2);
        assert_eq!(large_lane_budget(0, 0), 8);
        assert_eq!(large_lane_budget(3, 5), 2);
        assert_eq!(large_lane_budget(1, 0), 4);
    }

    #[test]
    fn cancel_file_drops_active_and_queued() {
        let handle = SyncHandle::new();
        let events = EventHub::new();
        handle.report_file_progress(
            &events,
            ActiveDownload {
                file_id: "f1".into(),
                file_name: "a.mp4".into(),
                kind: MediaKind::Video,
                bytes: "1".into(),
                total: Some("2".into()),
            },
        );
        handle.patch_status(&events, |status| {
            status.queued.push(QueuedDownload {
                file_id: "f1".into(),
                file_name: "a.mp4".into(),
                kind: MediaKind::Video,
                chat_id: Some("c1".into()),
                message_id: Some(1),
            });
        });
        handle.cancel_file("f1".into());
        let snap = handle.snapshot();
        assert!(snap.active.is_empty());
        assert!(snap.queued.is_empty());
        assert!(handle.should_cancel_job("f1", "c1"));
    }

    #[test]
    fn cancel_chat_drops_queued_keeps_active() {
        let handle = SyncHandle::new();
        let events = EventHub::new();
        handle.report_file_progress(
            &events,
            ActiveDownload {
                file_id: "f1".into(),
                file_name: "a.mp4".into(),
                kind: MediaKind::Video,
                bytes: "1".into(),
                total: Some("2".into()),
            },
        );
        handle.patch_status(&events, |status| {
            status.queued.push(QueuedDownload {
                file_id: "f2".into(),
                file_name: "b.mp4".into(),
                kind: MediaKind::Video,
                chat_id: Some("c1".into()),
                message_id: Some(2),
            });
            status.queued.push(QueuedDownload {
                file_id: "f3".into(),
                file_name: "c.mp4".into(),
                kind: MediaKind::Video,
                chat_id: Some("c2".into()),
                message_id: Some(3),
            });
        });
        assert!(handle.request_redownload(
            "c1".into(),
            4,
            Some("f4".into()),
            Some("d.mp4".into()),
            Some(MediaKind::Video)
        ));
        handle.cancel_chat("c1".into());
        let snap = handle.snapshot();
        assert_eq!(snap.active.len(), 1);
        assert_eq!(snap.active[0].file_id, "f1");
        assert_eq!(snap.queued.len(), 1);
        assert_eq!(snap.queued[0].file_id, "f3");
        assert!(handle.peek_redownloads(8).is_empty());
        assert!(!handle.should_cancel_media("f1", "c1", false));
        assert!(handle.should_cancel_job("f2", "c1"));
        assert!(!handle.should_cancel_job("f3", "c2"));
    }

    #[test]
    fn single_file_cancel_does_not_block_cursor() {
        assert!(!task_error_blocks_cursor(&DownloadTaskError::Cancelled {
            keep_part: false
        }));
        assert!(task_error_blocks_cursor(&DownloadTaskError::Cancelled {
            keep_part: true
        }));
        assert!(!task_error_blocks_cursor(&DownloadTaskError::Other(
            "下载超时".into()
        )));
        assert!(!task_error_blocks_cursor(&DownloadTaskError::Flood(15)));
        assert!(!task_error_blocks_cursor(&DownloadTaskError::Timeout));
        assert!(!task_error_blocks_cursor(&DownloadTaskError::ExpiredRef));
    }

    #[test]
    fn stall_retries_once() {
        let mut retried = HashSet::new();
        assert!(note_download_retry(&mut retried, "f1"));
        assert!(!note_download_retry(&mut retried, "f1"));
        assert!(note_download_retry(&mut retried, "f2"));
    }

    #[test]
    fn stall_timeout_and_expired_skip_not_pause() {
        assert!(!should_pause_after_retries(&DownloadTaskError::Timeout));
        assert!(!should_pause_after_retries(&DownloadTaskError::ExpiredRef));
        assert!(!should_pause_after_retries(&DownloadTaskError::Other(
            "rpc 400 FILE_REFERENCE_EXPIRED".into()
        )));
        assert!(!should_pause_after_retries(&DownloadTaskError::Other(
            "download failed".into()
        )));
        assert!(!should_pause_after_retries(&DownloadTaskError::Flood(15)));
        assert!(!should_pause_after_retries(&DownloadTaskError::Cancelled {
            keep_part: true
        }));
        assert!(should_pause_after_retries(&DownloadTaskError::Other(
            "No space left on device (os error 28)".into()
        )));
        assert!(should_pause_after_retries(&DownloadTaskError::Other(
            "ENOSPC".into()
        )));
        assert!(is_local_fatal_download(
            "There is not enough space on the disk"
        ));
        assert!(is_local_fatal_download("磁盘已满"));
        assert!(!is_local_fatal_download("下载超时"));
    }

    #[test]
    fn skipped_file_clears_retry_so_it_can_retry_later() {
        let mut retried = HashSet::new();
        assert!(note_download_retry(&mut retried, "f1"));
        assert!(!note_download_retry(&mut retried, "f1"));
        retried.remove("f1");
        assert!(note_download_retry(&mut retried, "f1"));
    }

    #[test]
    fn skip_timeout_does_not_block_backfill_cursor() {
        assert!(!task_error_blocks_cursor(&DownloadTaskError::Timeout));
        assert!(!task_error_blocks_cursor(&DownloadTaskError::ExpiredRef));
        assert!(!should_pause_after_retries(&DownloadTaskError::Timeout));
        assert!(!should_pause_after_retries(&DownloadTaskError::ExpiredRef));
        assert!(should_commit_backfill_cursor(false, true));
    }

    fn fail_action_label(action: MediaFailAction) -> &'static str {
        match action {
            MediaFailAction::Hold => "hold",
            MediaFailAction::Retry => "retry",
            MediaFailAction::Pause => "pause",
            MediaFailAction::Skip { .. } => "skip",
        }
    }

    /// 与 run_media_jobs 同一套判定：本批连续停滞、单文件上限。
    fn stall_decision(
        streak: u8,
        file_stalls: u8,
        already_retried: bool,
        err: &DownloadTaskError,
    ) -> (u8, u8, &'static str) {
        let (file_stalls, file_over) = if is_network_stall(err) {
            next_file_stall_count(file_stalls)
        } else {
            (file_stalls, false)
        };
        let streak = if is_network_stall(err) {
            next_network_stall_streak(streak).0
        } else {
            streak
        };
        let action = media_fail_action(err, streak, file_over, already_retried);
        let streak = match action {
            MediaFailAction::Skip { reset_streak: true } => 0,
            _ => streak,
        };
        (streak, file_stalls, fail_action_label(action))
    }

    #[test]
    fn isolated_timeout_still_skips_not_pause() {
        let (streak, file_stalls, action) =
            stall_decision(0, 0, false, &DownloadTaskError::Timeout);
        assert_eq!((streak, file_stalls, action), (1, 1, "retry"));
        let (streak, file_stalls, action) =
            stall_decision(streak, file_stalls, true, &DownloadTaskError::Timeout);
        assert_eq!((streak, file_stalls, action), (2, 2, "skip"));
        assert!(!should_pause_after_retries(&DownloadTaskError::Timeout));
        assert!(!task_error_blocks_cursor(&DownloadTaskError::Timeout));
        assert!(should_commit_backfill_cursor(false, true));
    }

    #[test]
    fn consecutive_network_stalls_hold_cursor_not_skip_or_pause() {
        let (s1, f1, a1) = stall_decision(0, 0, false, &DownloadTaskError::Timeout);
        assert_eq!((s1, f1, a1), (1, 1, "retry"));
        let (s2, f2, a2) = stall_decision(s1, f1, true, &DownloadTaskError::Timeout);
        assert_eq!((s2, f2, a2), (2, 2, "skip"));
        let (s3, f3, a3) = stall_decision(
            s2,
            0,
            false,
            &DownloadTaskError::Other("read 0 bytes".into()),
        );
        assert_eq!(s3, 3);
        assert_eq!(f3, 1);
        assert_eq!(a3, "hold");
        assert!(!should_pause_after_retries(&DownloadTaskError::Timeout));
        assert!(!should_commit_backfill_cursor(true, true));
        assert_eq!(NETWORK_STALL_WAIT_SECS, 15);
        assert!(NETWORK_STALL_WAIT_SECS < FLOOD_PAUSE_SECS);
        assert_eq!(NETWORK_STALL_WAIT_DETAIL, "网络中断，等待重试");
    }

    #[test]
    fn file_stall_cap_skips_instead_of_holding_forever() {
        let mut streak = 2;
        let mut file_stalls = 0;
        let mut actions = Vec::new();
        for i in 0..NETWORK_FILE_STALL_LIMIT {
            let (next_streak, next_file, action) =
                stall_decision(streak, file_stalls, i > 0, &DownloadTaskError::Timeout);
            streak = next_streak;
            file_stalls = next_file;
            actions.push(action);
        }
        assert_eq!(actions, ["hold", "hold", "hold", "hold", "skip"]);
        assert_eq!(file_stalls, NETWORK_FILE_STALL_LIMIT);
        assert_eq!(streak, 0);
        assert!(NETWORK_FILE_STALL_LIMIT > NETWORK_STALL_LIMIT);
    }

    #[test]
    fn expired_ref_does_not_count_as_network_stall() {
        assert!(!is_network_stall(&DownloadTaskError::ExpiredRef));
        let (streak, file_stalls, action) =
            stall_decision(2, 0, false, &DownloadTaskError::ExpiredRef);
        assert_eq!((streak, file_stalls, action), (2, 0, "retry"));
        let (streak, file_stalls, action) =
            stall_decision(2, 0, true, &DownloadTaskError::ExpiredRef);
        assert_eq!((streak, file_stalls, action), (2, 0, "skip"));
        assert!(!should_pause_after_retries(&DownloadTaskError::ExpiredRef));
    }

    #[test]
    fn successful_download_resets_network_stall_streak() {
        let mut streak = stall_decision(0, 0, false, &DownloadTaskError::Timeout).0;
        streak = stall_decision(streak, 1, true, &DownloadTaskError::Timeout).0;
        assert_eq!(streak, 2);
        streak = 0;
        let (streak, _, action) = stall_decision(streak, 0, false, &DownloadTaskError::Timeout);
        assert_eq!((streak, action), (1, "retry"));
    }

    #[test]
    fn network_stall_includes_timeout_and_stale_conn_not_disk_full() {
        assert!(is_network_stall(&DownloadTaskError::Timeout));
        assert!(is_network_stall(&DownloadTaskError::Other(
            "连接断开".into()
        )));
        assert!(is_network_stall(&DownloadTaskError::Other(
            "connection reset".into()
        )));
        assert!(is_network_stall(&DownloadTaskError::Other(
            "broken pipe".into()
        )));
        assert!(!is_network_stall(&DownloadTaskError::ExpiredRef));
        assert!(!is_network_stall(&DownloadTaskError::Other(
            "ENOSPC".into()
        )));
        assert!(!is_network_stall(&DownloadTaskError::Flood(15)));
        let (_, _, action) = stall_decision(2, 0, true, &DownloadTaskError::Other("ENOSPC".into()));
        assert_eq!(action, "pause");
    }

    #[test]
    fn flood_pause_adds_five_and_long_wait_floors_to_fifteen_min() {
        assert_eq!(flood_pause_secs(12), 17);
        assert_eq!(flood_pause_secs(1), 6);
        assert_eq!(flood_pause_secs(FLOOD_LONG_SECS), FLOOD_LONG_SECS + 5);
        assert_eq!(flood_pause_secs(FLOOD_LONG_SECS + 1), FLOOD_PAUSE_SECS);
        assert_eq!(flood_pause_secs(FLOOD_PAUSE_SECS), FLOOD_PAUSE_SECS + 5);
    }

    #[test]
    fn remaining_wait_secs_counts_down() {
        assert_eq!(remaining_wait_secs(Duration::from_secs(17)), 17);
        assert_eq!(remaining_wait_secs(Duration::from_millis(1500)), 1);
        assert_eq!(remaining_wait_secs(Duration::from_millis(400)), 1);
        assert_eq!(remaining_wait_secs(Duration::ZERO), 0);
    }

    #[test]
    fn flood_retry_skips_extra_lanes() {
        assert_eq!(download_lane_budget(true, true, 0, 0), 0);
        assert_eq!(download_lane_budget(true, false, 0, 0), 0);
        assert!(download_lane_budget(false, true, 0, 0) >= 2);
    }

    #[test]
    fn timeout_retry_uses_short_backoff_not_flood_pause() {
        assert_eq!(
            timeout_retry_backoff_secs(&DownloadTaskError::Timeout),
            Some(TIMEOUT_RETRY_BACKOFF_SECS)
        );
        assert!(TIMEOUT_RETRY_BACKOFF_SECS < FLOOD_PAUSE_SECS);
        assert!(timeout_retry_backoff_secs(&DownloadTaskError::ExpiredRef).is_none());
        assert!(timeout_retry_backoff_secs(&DownloadTaskError::Other("ENOSPC".into())).is_none());
    }

    #[test]
    fn expired_ref_text_matches() {
        assert!(is_expired_ref_text("FILE_REFERENCE_EXPIRED"));
        assert!(is_expired_ref_text("rpc 400 FILE_REFERENCE_INVALID"));
        assert!(!is_expired_ref_text("FLOOD_WAIT"));
    }

    #[test]
    fn stale_conn_text_matches() {
        assert!(is_stale_conn_text("request error: read 0 bytes"));
        assert!(is_stale_conn_text("连接断开"));
        assert!(is_stale_conn_text("下载超时"));
        assert!(!is_stale_conn_text("FILE_REFERENCE_EXPIRED"));
    }

    #[test]
    fn force_redownload_ignores_file_cancel() {
        let handle = SyncHandle::new();
        handle.cancel_file("f1".into());
        assert!(handle.should_cancel_media("f1", "c1", false));
        assert!(!handle.should_cancel_media("f1", "c1", true));
        handle.set_paused(true);
        assert!(handle.should_cancel_media("f1", "c1", true));
    }

    #[test]
    fn pause_does_not_clear_cancelled_chats() {
        let handle = SyncHandle::new();
        handle.cancel_chat("c1".into());
        handle.set_paused(true);
        handle.set_paused(false);
        assert!(handle.should_cancel_job("f1", "c1"));
    }

    #[test]
    fn take_next_ready_job_respects_concurrency() {
        let mut pending = vec![1, 2, 3, 4, 5];
        let is_large = |_: &i32| false;
        assert_eq!(
            take_next_ready_job(&mut pending, 0, 0, 1, 4, is_large),
            Some(1)
        );
        assert_eq!(pending, vec![2, 3, 4, 5]);
        assert_eq!(
            take_next_ready_job(&mut pending, 1, 0, 1, 4, is_large),
            None
        );
        assert_eq!(
            take_next_ready_job(&mut pending, 0, 0, 8, 4, is_large),
            Some(2)
        );
        assert!(take_next_ready_job(&mut Vec::<i32>::new(), 0, 0, 1, 4, is_large).is_none());
    }

    #[test]
    fn take_reload_is_one_shot() {
        let handle = SyncHandle::new();
        handle.mark_reload();
        assert!(handle.take_reload());
        assert!(!handle.take_reload());
    }

    #[test]
    fn request_stop_cancels_jobs() {
        let handle = SyncHandle::new();
        handle.request_stop();
        assert!(handle.should_stop());
        assert!(handle.should_cancel_job("f1", "c1"));
    }

    #[test]
    fn prepare_for_run_clears_stop() {
        let handle = SyncHandle::new();
        handle.request_stop();
        handle.prepare_for_run();
        assert!(!handle.should_stop());
        assert!(!handle.should_cancel_job("f1", "c1"));
    }

    #[test]
    fn prepare_for_run_keeps_pause_cancel_all() {
        let handle = SyncHandle::new();
        handle.set_paused(true);
        handle.prepare_for_run();
        assert!(!handle.should_stop());
        assert!(handle.is_paused());
        assert!(handle.should_cancel_job("f1", "c1"));
    }

    #[test]
    fn redownload_queue_dedupes_and_pops() {
        let handle = SyncHandle::new();
        assert!(handle.request_redownload(" c1 ".into(), 12, None, None, None));
        assert!(!handle.request_redownload("c1".into(), 12, None, None, None));
        assert!(handle.request_redownload("c1".into(), 13, None, None, None));
        assert!(!handle.request_redownload("".into(), 1, None, None, None));
        assert!(!handle.request_redownload("c1".into(), 0, None, None, None));
        let first = handle.pop_redownload().expect("queued");
        assert_eq!(first.chat_id, "c1");
        assert_eq!(first.message_id, 12);
        assert!(!handle.request_redownload("c1".into(), 12, None, None, None));
        handle.finish_redownload(&first);
        assert!(handle.request_redownload("c1".into(), 12, None, None, None));
        let second = handle.pop_redownload().expect("second");
        assert_eq!(second.message_id, 13);
        handle.finish_redownload(&second);
        assert!(handle.pop_redownload().is_some());
        assert!(handle.pop_redownload().is_none());
    }

    #[test]
    fn redownload_dedupes_same_file_id() {
        let handle = SyncHandle::new();
        assert!(handle.request_redownload(
            "c1".into(),
            1,
            Some("f1".into()),
            Some("a.mp4".into()),
            Some(MediaKind::Video)
        ));
        assert!(!handle.request_redownload("c2".into(), 2, Some("f1".into()), None, None));
        let peeked = handle.peek_redownloads(10);
        assert_eq!(peeked.len(), 1);
        assert_eq!(peeked[0].file_name.as_deref(), Some("a.mp4"));
    }

    #[test]
    fn pause_keeps_part_file_cancel_does_not() {
        let handle = SyncHandle::new();
        assert!(!handle.keep_partial_file());
        handle.set_paused(true);
        assert!(handle.keep_partial_file());
        handle.set_paused(false);
        handle.cancel_file("f1".into());
        assert!(!handle.keep_partial_file());
        assert!(handle.should_cancel_job("f1", "c1"));
        handle.cancel_chat("c2".into());
        assert!(!handle.keep_partial_file());
        handle.request_stop();
        assert!(handle.keep_partial_file());
    }

    fn types(text: bool, video: bool) -> ChatDownloadTypes {
        ChatDownloadTypes {
            text,
            video,
            ..ChatDownloadTypes::default()
        }
    }

    #[test]
    fn old_cursor_grandfathers_current_types() {
        let raw =
            r#"{"chats":{"1":{"backfill_offset_id":99,"backfill_done":true,"applied_days":7}}}"#;
        let state: SyncState = serde_json::from_str(raw).unwrap();
        let mut cursor = state.chats.get("1").cloned().unwrap();
        assert!(cursor.applied_types.is_none());

        let reset = reconcile_chat_cursor(&mut cursor, 7, types(true, true));
        assert!(!reset);
        assert_eq!(cursor.backfill_offset_id, 99);
        assert!(cursor.backfill_done);
        assert_eq!(cursor.applied_types, Some(types(true, true)));
    }

    #[test]
    fn new_media_type_keeps_offset_for_id_fetch() {
        let mut cursor = ChatCursor {
            backfill_offset_id: 80,
            backfill_done: true,
            applied_days: 7,
            applied_types: Some(types(true, false)),
            ..ChatCursor::default()
        };
        assert!(reconcile_chat_cursor(&mut cursor, 7, types(true, true)));
        assert_eq!(cursor.backfill_offset_id, 80);
        assert!(cursor.backfill_done);
        assert_eq!(cursor.applied_types, Some(types(true, false)));
        assert_eq!(cursor.type_after_id, 0);
        assert!(!cursor.type_fallback);
        assert!(!cursor.type_ids_done);
        assert!(type_fetch_pending(&cursor, 7, types(true, true)));
    }

    #[test]
    fn new_text_type_resets_offset() {
        let mut cursor = ChatCursor {
            backfill_offset_id: 80,
            backfill_done: true,
            applied_days: 7,
            applied_types: Some(types(false, true)),
            ..ChatCursor::default()
        };
        assert!(reconcile_chat_cursor(&mut cursor, 7, types(true, true)));
        assert_eq!(cursor.backfill_offset_id, 0);
        assert!(!cursor.backfill_done);
        assert!(!type_fetch_pending(&cursor, 7, types(true, true)));
    }

    #[test]
    fn new_type_does_not_backfill_when_days_zero() {
        let mut cursor = ChatCursor {
            backfill_offset_id: 80,
            backfill_done: true,
            applied_days: 0,
            applied_types: Some(types(true, false)),
            ..ChatCursor::default()
        };
        assert!(!reconcile_chat_cursor(&mut cursor, 0, types(true, true)));
        assert_eq!(cursor.backfill_offset_id, 80);
        assert!(cursor.backfill_done);
        assert_eq!(cursor.applied_types, Some(types(true, false)));
        assert!(!type_fetch_pending(&cursor, 0, types(true, true)));
    }

    #[test]
    fn reenable_applied_type_does_not_rescan() {
        let mut cursor = ChatCursor {
            backfill_offset_id: 40,
            backfill_done: true,
            applied_days: 7,
            applied_types: Some(types(true, true)),
            ..ChatCursor::default()
        };
        assert!(!reconcile_chat_cursor(&mut cursor, 7, types(true, true)));
        assert_eq!(cursor.backfill_offset_id, 40);
        assert!(cursor.backfill_done);
    }

    #[test]
    fn expand_days_keeps_offset() {
        let mut cursor = ChatCursor {
            backfill_offset_id: 40,
            backfill_done: true,
            applied_days: 7,
            applied_types: Some(types(true, true)),
            ..ChatCursor::default()
        };
        assert!(!reconcile_chat_cursor(&mut cursor, 30, types(true, true)));
        assert_eq!(cursor.backfill_offset_id, 40);
        assert!(!cursor.backfill_done);
        assert_eq!(cursor.applied_days, 30);
    }

    #[test]
    fn days_zero_then_positive_with_new_type_resets() {
        let mut cursor = ChatCursor {
            backfill_offset_id: 0,
            backfill_done: true,
            applied_days: 0,
            applied_types: Some(types(true, false)),
            ..ChatCursor::default()
        };
        assert!(reconcile_chat_cursor(&mut cursor, 7, types(true, true)));
        assert_eq!(cursor.backfill_offset_id, 0);
        assert!(!cursor.backfill_done);
        assert_eq!(cursor.applied_days, 7);
        assert_eq!(cursor.applied_types, Some(types(true, false)));
        assert!(type_fetch_pending(&cursor, 7, types(true, true)));
    }

    #[test]
    fn mark_done_unions_types_and_clear_then_days_reopens() {
        let mut cursor = ChatCursor {
            applied_types: Some(types(true, false)),
            ..ChatCursor::default()
        };
        mark_cursor_done(&mut cursor, 7, types(true, true));
        assert!(cursor.backfill_done);
        assert_eq!(cursor.applied_days, 7);
        assert_eq!(cursor.applied_types, Some(types(true, true)));
        assert!(!cursor.type_fallback);
        assert!(!cursor.type_ids_done);
        assert!(!cursor.gap_fill);
        assert_eq!(cursor.type_after_id, 0);

        let mut reset = ChatCursor::default();
        assert!(!reconcile_chat_cursor(&mut reset, 7, types(true, true)));
        assert!(!reset.backfill_done);
        assert_eq!(reset.applied_days, 7);
        assert_eq!(reset.applied_types, Some(types(true, true)));
        assert_eq!(reset.backfill_offset_id, 0);
    }

    #[test]
    fn type_fetch_skips_fallback_and_ids_done() {
        let mut cursor = ChatCursor {
            backfill_done: true,
            applied_days: 7,
            applied_types: Some(types(true, false)),
            ..ChatCursor::default()
        };
        assert!(type_fetch_pending(&cursor, 7, types(true, true)));
        cursor.type_fallback = true;
        assert!(!type_fetch_pending(&cursor, 7, types(true, true)));
        cursor.type_fallback = false;
        cursor.type_ids_done = true;
        assert!(!type_fetch_pending(&cursor, 7, types(true, true)));
    }

    #[test]
    fn local_history_covers_window() {
        let now = 1_700_000_000;
        assert!(!local_covers_window(None, 7, now));
        assert!(!local_covers_window(Some(now - 2 * 86_400), 7, now));
        assert!(local_covers_window(Some(now - 7 * 86_400), 7, now));
        assert!(local_covers_window(Some(now - 6 * 86_400), 7, now));
        assert!(local_covers_window(Some(now), -1, now));
        assert!(!local_covers_window(Some(now), 0, now));
        assert_eq!(cutoff_unix(7, now), Some(now - 7 * 86_400));
        assert_eq!(cutoff_unix(-1, now), None);
    }

    #[test]
    fn reopen_done_gap_fill_resets_offset_keeps_types() {
        let mut cursor = ChatCursor {
            backfill_offset_id: 65254,
            backfill_done: true,
            applied_days: 1,
            applied_types: Some(types(true, true)),
            ..ChatCursor::default()
        };
        reopen_done_for_gap_fill(&mut cursor, 1, Some(65316));
        assert!(!cursor.backfill_done);
        assert_eq!(cursor.backfill_offset_id, 0);
        assert_eq!(cursor.backfill_head_id, 65316);
        assert!(cursor.gap_fill);
        assert_eq!(cursor.applied_days, 1);
        assert_eq!(cursor.applied_types, Some(types(true, true)));
    }

    #[test]
    fn reopen_skips_days_zero_and_unfinished() {
        let mut done_zero = ChatCursor {
            backfill_done: true,
            applied_days: 0,
            backfill_offset_id: 10,
            ..ChatCursor::default()
        };
        reopen_done_for_gap_fill(&mut done_zero, 0, Some(99));
        assert!(done_zero.backfill_done);
        assert_eq!(done_zero.backfill_offset_id, 10);
        assert!(!done_zero.gap_fill);

        let mut running = ChatCursor {
            backfill_done: false,
            applied_days: 1,
            backfill_offset_id: 80,
            ..ChatCursor::default()
        };
        reopen_done_for_gap_fill(&mut running, 1, Some(99));
        assert!(!running.backfill_done);
        assert_eq!(running.backfill_offset_id, 80);
        assert!(!running.gap_fill);
    }

    #[test]
    fn history_stop_only_uses_head_when_gap_fill() {
        assert!(hits_history_stop(10, true, 0, false));
        assert!(!hits_history_stop(10, false, 20, false));
        assert!(hits_history_stop(10, false, 20, true));
        assert!(hits_history_stop(20, false, 20, true));
        assert!(!hits_history_stop(21, false, 20, true));
        assert!(!hits_history_stop(21, false, 0, true));
    }

    #[test]
    fn idle_detail_distinguishes_done_window() {
        assert_eq!(idle_backfill_detail(true), "回爬已完成");
        assert_eq!(idle_backfill_detail(false), "只收新消息");
    }

    #[test]
    fn new_text_clears_gap_fill() {
        let mut cursor = ChatCursor {
            backfill_offset_id: 80,
            backfill_done: true,
            applied_days: 7,
            applied_types: Some(types(false, true)),
            gap_fill: true,
            backfill_head_id: 90,
            ..ChatCursor::default()
        };
        assert!(reconcile_chat_cursor(&mut cursor, 7, types(true, true)));
        assert!(!cursor.gap_fill);
        assert_eq!(cursor.backfill_offset_id, 0);
        assert!(!cursor.backfill_done);
    }

    #[test]
    fn commit_cursor_skips_cancel_and_unwatch() {
        assert!(should_commit_backfill_cursor(false, true));
        assert!(!should_commit_backfill_cursor(true, true));
        assert!(!should_commit_backfill_cursor(false, false));
        assert!(!should_commit_backfill_cursor(true, false));
    }

    #[test]
    fn comment_private_blocks_after_limit() {
        assert_eq!(next_comment_private_streak(0), (1, false));
        assert_eq!(next_comment_private_streak(1), (2, false));
        assert_eq!(next_comment_private_streak(2), (3, true));
        assert_eq!(next_comment_private_streak(u8::MAX), (u8::MAX, true));
    }

    #[test]
    fn network_stall_blocks_after_limit() {
        assert_eq!(next_network_stall_streak(0), (1, false));
        assert_eq!(next_network_stall_streak(1), (2, false));
        assert_eq!(next_network_stall_streak(2), (3, true));
        assert_eq!(next_network_stall_streak(u8::MAX), (u8::MAX, true));
        assert_eq!(NETWORK_STALL_LIMIT, COMMENT_PRIVATE_LIMIT);
    }

    #[test]
    fn file_stall_blocks_after_limit() {
        assert_eq!(next_file_stall_count(0), (1, false));
        assert_eq!(next_file_stall_count(3), (4, false));
        assert_eq!(next_file_stall_count(4), (5, true));
        assert_eq!(next_file_stall_count(u8::MAX), (u8::MAX, true));
        assert!(NETWORK_FILE_STALL_LIMIT > NETWORK_STALL_LIMIT);
    }

    #[test]
    fn dialog_scan_stops_when_watched_peers_found() {
        let mut peers = HashMap::new();
        peers.insert("a".into(), ());
        assert!(!dialog_scan_complete(&["a".into(), "b".into()], &peers));
        peers.insert("b".into(), ());
        assert!(dialog_scan_complete(&["a".into(), "b".into()], &peers));
        assert!(!dialog_scan_complete(&[], &peers));
    }

    #[test]
    fn needed_peers_skip_auto_comment_groups() {
        let mut settings = AppSettings::default();
        settings.watched_chat_ids = vec!["ch".into(), "disc".into(), "42".into()];
        settings.auto_comment_chats = vec!["disc".into()];
        settings.guest_watch_chat_id = "guest".into();
        assert_eq!(
            needed_backfill_peer_ids(&settings),
            vec!["ch".to_string(), "guest".to_string()]
        );
    }

    #[test]
    fn skip_indexed_media_recover_only_when_missing() {
        assert!(!skip_indexed_media(false, false, false));
        assert!(skip_indexed_media(true, false, false));
        assert!(skip_indexed_media(true, true, false));
        assert!(skip_indexed_media(true, true, true));
        assert!(!skip_indexed_media(true, false, true));
        assert!(!skip_indexed_media(false, false, true));
    }

    #[test]
    fn request_check_dedupes_and_cancel_drops() {
        let handle = SyncHandle::new();
        assert!(handle.request_check(" c1 ".into()));
        assert!(!handle.request_check("c1".into()));
        assert!(handle.request_check("c2".into()));
        assert!(!handle.request_check("".into()));
        assert!(handle.has_check());
        handle.cancel_chat("c1".into());
        assert_eq!(handle.pop_check().as_deref(), Some("c2"));
        assert!(handle.pop_check().is_none());
        assert!(!handle.has_check());
    }

    #[test]
    fn check_history_stop_ignores_head() {
        assert!(!hits_history_stop(10, false, 20, false));
        assert!(hits_history_stop(10, true, 20, false));
    }
}
