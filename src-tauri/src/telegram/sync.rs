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
use tauri::{AppHandle, Manager};
use tauri_specta::Event;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;

use crate::error::AppError;
use crate::settings::{AppSettings, ChatDownloadTypes};
use crate::telegram::client::emit_telegram_status;
use crate::telegram::download::{
    aligned_part_len, classify_media, is_before_cutoff, media_file_id, media_mime,
    media_original_name, media_path, part_path, skip_chunks, MediaIndex,
};
use crate::telegram::session::SessionPaths;
use crate::telegram::store::{MessageRecord, MessageStore};
use crate::telegram::MediaKind;

const HISTORY_PAGE: usize = 100;
const PAGE_DELAY_MS: (u64, u64) = (1500, 2500);
const MEDIA_DELAY_MS: (u64, u64) = (400, 1000);
const CHAT_DELAY_MS: u64 = 5000;
const FLOOD_LONG_SECS: u64 = 300;
const FLOOD_PAUSE_SECS: u64 = 15 * 60;
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(30);

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

#[derive(Debug, Clone, Serialize, Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct ChatIngested {
    pub chat_id: String,
}

#[derive(Debug, Clone, Serialize, Type, Event)]
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
    cancel_ids: Mutex<HashSet<String>>,
    cancel_chats: Mutex<HashSet<String>>,
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
    }

    pub async fn wait_stopped(&self) {
        if !self.inner.started.load(Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(8), self.inner.stopped.notified()).await;
    }

    pub fn reset_progress(&self, app: &AppHandle) {
        self.patch_status(app, |status| {
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
    }

    pub fn cancel_chat(&self, chat_id: String) {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return;
        }
        if let Ok(mut ids) = self.inner.cancel_chats.lock() {
            ids.insert(chat_id.to_string());
        }
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
        self.inner
            .cancel_ids
            .lock()
            .map(|ids| ids.contains(file_id))
            .unwrap_or(false)
    }

    fn should_cancel_job(&self, file_id: &str, chat_id: &str) -> bool {
        self.should_cancel(file_id) || self.is_chat_cancelled(chat_id)
    }

    /// 暂停 / 退出保留 `.part`；单文件取消或关掉会话监听则删掉。
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

    fn patch_status(&self, app: &AppHandle, patch: impl FnOnce(&mut DownloadProgress)) {
        let snap = {
            let Ok(mut status) = self.inner.status.lock() else {
                return;
            };
            patch(&mut status);
            status.paused = self.inner.paused.load(Ordering::SeqCst);
            status.clone()
        };
        let _ = snap.emit(app);
    }

    fn report_file_progress(&self, app: &AppHandle, item: ActiveDownload) {
        self.patch_status(app, |status| {
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

    fn clear_file_progress(&self, app: &AppHandle, file_id: &str) {
        self.patch_status(app, |status| {
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

pub fn notify_settings_changed(app: &AppHandle) {
    if let Some(state) = app.try_state::<crate::AppState>() {
        state.sync.mark_reload();
    }
}

pub fn spawn_download_worker(app: &AppHandle) {
    let Some(state) = app.try_state::<crate::AppState>() else {
        return;
    };
    if !state.sync.try_begin() {
        return;
    }
    if let Ok(paths) = SessionPaths::resolve(app) {
        let settings = AppSettings::load(&paths.root);
        state.sync.set_paused(settings.download_paused);
    }

    let app = app.clone();
    let handle = state.sync.clone();
    tauri::async_runtime::spawn(async move {
        run_worker_supervisor(app, handle.clone()).await;
        handle.mark_stopped();
    });
}

enum SessionAcquire {
    Ready(Client, mpsc::UnboundedReceiver<UpdatesLike>),
    Unauthorized,
    Stopped,
    Failed(AppError),
}

async fn acquire_telegram_session(app: &AppHandle, handle: &SyncHandle) -> SessionAcquire {
    if handle.should_stop() {
        return SessionAcquire::Stopped;
    }
    let Some(state) = app.try_state::<crate::AppState>() else {
        return SessionAcquire::Failed(AppError::Internal("状态未初始化".into()));
    };
    let mut telegram = state.telegram.lock().await;
    if handle.should_stop() {
        return SessionAcquire::Stopped;
    }
    if let Some((client, updates)) = telegram.take_worker_session() {
        return SessionAcquire::Ready(client, updates);
    }
    match telegram.reconnect_transport(app).await {
        Ok((client, updates)) if telegram.is_authorized() => {
            emit_telegram_status(app, &telegram);
            SessionAcquire::Ready(client, updates)
        }
        Ok(_) => {
            emit_telegram_status(app, &telegram);
            SessionAcquire::Unauthorized
        }
        Err(err) => {
            emit_telegram_status(app, &telegram);
            SessionAcquire::Failed(err)
        }
    }
}

fn emit_reconnect(app: &AppHandle, handle: &SyncHandle, delay: Duration) {
    handle.patch_status(app, |status| {
        status.phase = DownloadPhase::Reconnect;
        status.detail = Some(format!("连接断开，{}s 后重连", delay.as_secs().max(1)));
        status.active.clear();
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

async fn run_worker_supervisor(app: AppHandle, handle: SyncHandle) {
    let mut backoff = RECONNECT_MIN;
    loop {
        if handle.should_stop() {
            break;
        }
        match acquire_telegram_session(&app, &handle).await {
            SessionAcquire::Stopped => break,
            SessionAcquire::Unauthorized => {
                handle.patch_status(&app, |status| {
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
                emit_reconnect(&app, &handle, backoff);
                if wait_or_stop(&handle, backoff).await {
                    break;
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
            SessionAcquire::Ready(client, updates) => {
                let started = Instant::now();
                match run_download_worker(app.clone(), client, updates, handle.clone()).await {
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
                emit_reconnect(&app, &handle, backoff);
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

fn idle_backfill_detail(any_window: bool) -> String {
    if any_window {
        "回爬已完成".into()
    } else {
        "只收新消息".into()
    }
}

/// 取消 / 关监听 / 失败时不要提交这批游标，否则重开会跳过当前文件。
fn should_commit_backfill_cursor(leftover: bool, chat_still_syncing: bool) -> bool {
    !leftover && chat_still_syncing
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

struct PeerInfo {
    peer: PeerRef,
    title: String,
}

#[derive(Clone, Copy)]
enum BackfillMode {
    History,
    Types { newly: ChatDownloadTypes },
}

struct BackfillJob {
    chat_id: String,
    title: String,
    peer: PeerRef,
    pending: VecDeque<Message>,
    finish_when_empty: bool,
    mode: BackfillMode,
    page_end_id: Option<i32>,
}

struct Worker {
    app: AppHandle,
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
}

async fn run_download_worker(
    app: AppHandle,
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

    let paths = SessionPaths::resolve(&app)?;
    paths.ensure_dirs()?;
    let settings = AppSettings::load(&paths.root);
    let store = MessageStore::open(&paths.root).await?;
    let mut worker = Worker {
        app,
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
    };
    worker.apply_resets();
    worker.reconcile_cursors();
    worker.reopen_gap_fills().await;
    worker.emit(DownloadPhase::Idle, None, None, None);

    if let Err(err) = worker.refresh_peers().await {
        log::warn!("initial dialog refresh failed: {err}");
    }

    let mut delay = Duration::from_millis(200);
    loop {
        if worker.handle.should_stop() {
            return Ok(());
        }
        tokio::select! {
            update = stream.next() => {
                if worker.handle.should_stop() {
                    return Ok(());
                }
                match update {
                    Ok(update) => {
                        if let Err(err) = worker.handle_update(update).await {
                            if let Some(secs) = flood_wait_secs(&err) {
                                worker.sleep_flood(secs).await;
                            } else {
                                log::warn!("live update failed: {err}");
                            }
                        }
                    }
                    Err(err) => {
                        if worker.handle.should_stop() {
                            return Ok(());
                        }
                        if let Some(secs) = flood_wait_secs(&err) {
                            worker.sleep_flood(secs).await;
                        } else {
                            return Err(err.into());
                        }
                    }
                }
            }
            _ = worker.handle.notified() => {
                if worker.handle.should_stop() {
                    return Ok(());
                }
                worker.apply_pending_settings().await;
                worker.progress.paused = worker.handle.is_paused();
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
            }
        }
    }
}

impl Worker {
    async fn apply_pending_settings(&mut self) {
        if self.handle.take_reload() {
            self.reload_settings();
            self.reopen_gap_fills().await;
        }
        self.drop_unwatched_backfill();
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
        if let Ok(paths) = SessionPaths::resolve(&self.app) {
            self.paths = paths;
            let _ = self.paths.ensure_dirs();
        }
        self.settings = AppSettings::load(&self.paths.root);
        self.apply_resets();
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
    }

    fn apply_resets(&mut self) {
        for chat_id in self.handle.take_resets() {
            self.sync.chats.insert(chat_id, ChatCursor::default());
        }
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

    async fn refresh_peers(&mut self) -> Result<(), AppError> {
        let mut dialogs = self.client.iter_dialogs();
        let mut peers = HashMap::new();
        while let Some(dialog) = dialogs.next().await? {
            let peer = dialog.peer();
            let id = peer.id().to_string();
            let title = peer
                .name()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("#{id}"));
            if let Ok(Some(peer_ref)) = peer.to_ref().await {
                peers.insert(
                    id,
                    PeerInfo {
                        peer: peer_ref,
                        title,
                    },
                );
            }
        }
        self.peers = peers;
        Ok(())
    }

    async fn handle_update(&mut self, update: Update) -> Result<(), InvocationError> {
        let Update::NewMessage(message) = update else {
            return Ok(());
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

        let concurrency = self.settings.effective_download_concurrency().max(1) as usize;
        let mut media_jobs = Vec::new();
        let mut ingested = Vec::new();
        let mut claimed = HashSet::new();
        let mut finish_job = false;
        let mut chat_id = String::new();
        let mut title = String::new();

        while media_jobs.len() < concurrency {
            let Some(job) = self.backfill.as_mut() else {
                break;
            };
            chat_id = job.chat_id.clone();
            title = job.title.clone();
            let Some(message) = job.pending.pop_front() else {
                finish_job = job.finish_when_empty;
                break;
            };
            finish_job = job.pending.is_empty() && job.finish_when_empty;
            ingested.push((message.peer_id().to_string(), message.id()));
            match self.ingest_message(&message, false).await {
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
                        return Ok(Duration::from_millis(200));
                    }
                    return Err(AppError::Telegram(err.to_string()));
                }
            }
        }

        self.apply_pending_settings().await;
        media_jobs.retain(|job| self.chat_still_syncing(&job.chat_id));

        self.emit(
            DownloadPhase::Backfill,
            Some(chat_id.clone()),
            Some(self.display_chat_title(&chat_id, &title)),
            self.type_backfill_detail(&chat_id),
        );

        let (downloaded, leftover) = if media_jobs.is_empty() {
            (false, false)
        } else {
            self.run_media_jobs(media_jobs).await
        };
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
        } else {
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
                if let Err(err) = self.refresh_peers().await {
                    log::warn!("refresh peers for backfill: {err}");
                    continue;
                }
            }
            let Some(info) = self.peers.get(chat_id) else {
                log::warn!("cannot resolve peer {chat_id}, skip backfill");
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
        let (offset, head_id, gap_fill) = self
            .sync
            .chats
            .get(&chat_id)
            .map(|cursor| {
                (
                    cursor.backfill_offset_id,
                    cursor.backfill_head_id,
                    cursor.gap_fill,
                )
            })
            .unwrap_or((0, 0, false));
        self.emit(
            DownloadPhase::Backfill,
            Some(chat_id.clone()),
            Some(self.display_chat_title(&chat_id, &title)),
            self.type_backfill_detail(&chat_id),
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
            self.mark_backfill_done(&chat_id);
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
            self.mark_backfill_done(&chat_id);
            self.backfill = None;
            return Ok(Duration::from_millis(CHAT_DELAY_MS));
        }
        if let Some(job) = self.backfill.as_mut() {
            job.pending = pending;
            job.finish_when_empty = hit_stop;
        }
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
            self.type_backfill_detail(&chat_id),
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
        let job = self.ingest_message(message, from_backfill).await?;
        let downloaded = if let Some(job) = job {
            if self.handle.is_paused() {
                self.progress.skipped += 1;
                false
            } else {
                self.run_media_jobs(vec![job]).await.0
            }
        } else {
            false
        };
        let _ = self.sync.save(&self.paths.root);
        Ok(downloaded)
    }

    async fn ingest_message(
        &mut self,
        message: &Message,
        commit_cursor: bool,
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

        if types.text {
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
                match self.take_media_job(&chat_id, &title, kind, &media) {
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

    fn take_media_job(
        &mut self,
        chat_id: &str,
        title: &str,
        kind: MediaKind,
        media: &Media,
    ) -> Result<Option<MediaJob>, InvocationError> {
        let Some(file_id) = media_file_id(media) else {
            return Ok(None);
        };
        if self.index.contains(&file_id) {
            return Ok(None);
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
        if dest.exists() {
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
            total: media.size().map(|size| size as u64),
        }))
    }

    async fn run_media_jobs(&mut self, jobs: Vec<MediaJob>) -> (bool, bool) {
        self.apply_pending_settings().await;
        let mut pending: Vec<MediaJob> = jobs
            .into_iter()
            .filter(|job| self.chat_still_syncing(&job.chat_id))
            .collect();
        let mut any = false;
        let mut leftover = false;
        while !pending.is_empty() {
            if self.handle.is_paused() {
                leftover = true;
                for job in pending.drain(..) {
                    self.handle.clear_file_progress(&self.app, &job.file_id);
                    self.progress.skipped += 1;
                }
                break;
            }
            let mut skipped = Vec::new();
            pending.retain(|job| {
                if self.chat_still_syncing(&job.chat_id) {
                    true
                } else {
                    skipped.push(job.file_id.clone());
                    false
                }
            });
            for file_id in skipped {
                self.handle.clear_file_progress(&self.app, &file_id);
                self.progress.skipped += 1;
            }
            if pending.is_empty() {
                break;
            }
            let mut set = JoinSet::new();
            for job in pending.drain(..) {
                let item = job.active_item(0);
                self.handle.report_file_progress(&self.app, item);
                let client = self.client.clone();
                let handle = self.handle.clone();
                let app = self.app.clone();
                set.spawn(async move { execute_media_job(client, job, handle, app).await });
            }
            let mut retry = Vec::new();
            let mut flood = None;
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(done)) => {
                        self.handle
                            .clear_file_progress(&self.app, &done.job.file_id);
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
                    }
                    Ok(Err((job, DownloadTaskError::Flood(secs)))) => {
                        flood = Some(flood.map_or(secs, |prev: u64| prev.max(secs)));
                        retry.push(job);
                    }
                    Ok(Err((job, DownloadTaskError::Cancelled { .. }))) => {
                        leftover = true;
                        self.handle.clear_file_progress(&self.app, &job.file_id);
                        self.progress.skipped += 1;
                    }
                    Ok(Err((job, DownloadTaskError::Other(err)))) => {
                        leftover = true;
                        self.handle.clear_file_progress(&self.app, &job.file_id);
                        log::warn!("download media failed: {err}");
                    }
                    Err(err) => log::warn!("download task: {err}"),
                }
            }
            if retry.is_empty() {
                break;
            }
            if self.handle.is_paused() {
                leftover = true;
                for job in retry {
                    self.handle.clear_file_progress(&self.app, &job.file_id);
                    self.progress.skipped += 1;
                }
                break;
            }
            self.sleep_flood(flood.unwrap_or(15)).await;
            pending = retry;
        }
        self.sync_progress();
        (any, leftover)
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
        self.index
            .remember(file_id, dest, kind, size, chat_id.clone(), chat_title);
        let _ = self.index.save(&self.paths.root);
        if let Some(chat_id) = chat_id {
            self.notify_chat_ingested(&chat_id);
        }
    }

    fn notify_chat_ingested(&self, chat_id: &str) {
        if chat_id.is_empty() {
            return;
        }
        let _ = ChatIngested {
            chat_id: chat_id.to_string(),
        }
        .emit(&self.app);
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

    fn sync_progress(&mut self) {
        self.progress.paused = self.handle.is_paused();
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
        );
        self.handle.patch_status(&self.app, |status| {
            status.phase = counters.0;
            status.chat_id = counters.1.clone();
            status.chat_title = counters.2.clone();
            status.processed = counters.3;
            status.downloaded = counters.4;
            status.skipped = counters.5;
            status.flood_wait_secs = counters.6;
            status.detail = counters.7.clone();
            status.paused = counters.8;
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

    async fn sleep_flood(&mut self, secs: u64) {
        let pause = if secs > FLOOD_LONG_SECS {
            FLOOD_PAUSE_SECS.max(secs + 5)
        } else {
            secs + 5
        };
        self.progress.flood_wait_secs = Some(pause as u32);
        self.emit(
            DownloadPhase::FloodWait,
            None,
            None,
            Some(format!("Telegram 限流，等待 {pause} 秒")),
        );
        let until = Instant::now() + Duration::from_secs(pause);
        loop {
            if self.handle.should_stop() {
                return;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
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

struct MediaJob {
    file_id: String,
    kind: MediaKind,
    media: Media,
    dest: PathBuf,
    chat_id: String,
    title: String,
    total: Option<u64>,
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
}

struct FinishedMedia {
    job: MediaJob,
    size: Option<u64>,
}

enum DownloadTaskError {
    Flood(u64),
    Cancelled { keep_part: bool },
    Other(String),
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
    job: MediaJob,
    handle: SyncHandle,
    app: AppHandle,
) -> Result<FinishedMedia, (MediaJob, DownloadTaskError)> {
    if handle.should_cancel_job(&job.file_id, &job.chat_id) {
        return Err((job, cancel_error(&handle)));
    }

    let tmp = part_path(&job.dest);
    let existing = std::fs::metadata(&tmp)
        .ok()
        .map(|meta| meta.len())
        .unwrap_or(0);
    if job
        .total
        .is_some_and(|total| total > 0 && existing >= total)
    {
        if let Err(err) = finish_part(&tmp, &job.dest) {
            return Err((job, DownloadTaskError::Other(err)));
        }
        let size = std::fs::metadata(&job.dest)
            .ok()
            .map(|meta| meta.len())
            .or(job.total);
        return Ok(FinishedMedia { job, size });
    }

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
    handle.report_file_progress(&app, job.active_item(downloaded));

    let mut iter = client.iter_download(&job.media);
    let skip = skip_chunks(aligned);
    if skip > 0 {
        iter = iter.skip_chunks(skip);
    }
    let result = loop {
        if handle.should_cancel_job(&job.file_id, &job.chat_id) {
            break Err(cancel_error(&handle));
        }
        match iter.next().await {
            Ok(Some(chunk)) => {
                if let Err(err) = file.write_all(&chunk) {
                    break Err(DownloadTaskError::Other(err.to_string()));
                }
                downloaded += chunk.len() as u64;
                if last_emit.elapsed() >= PROGRESS_EVERY {
                    handle.report_file_progress(&app, job.active_item(downloaded));
                    last_emit = Instant::now();
                }
            }
            Ok(None) => break Ok(()),
            Err(err) => {
                break Err(if let Some(secs) = flood_wait_secs(&err) {
                    DownloadTaskError::Flood(secs)
                } else {
                    DownloadTaskError::Other(err.to_string())
                });
            }
        }
    };

    drop(file);
    match result {
        Ok(()) => {
            handle.report_file_progress(&app, job.active_item(downloaded));
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
                    | DownloadTaskError::Other(_)
                    | DownloadTaskError::Cancelled { keep_part: true }
            );
            if !keep {
                let _ = std::fs::remove_file(&tmp);
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
    fn pause_does_not_clear_cancelled_chats() {
        let handle = SyncHandle::new();
        handle.cancel_chat("c1".into());
        handle.set_paused(true);
        handle.set_paused(false);
        assert!(handle.should_cancel_job("f1", "c1"));
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
}
