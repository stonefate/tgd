use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_specta::Event;

use crate::{
    error::AppError,
    settings::{AppSettings, ChatDownloadTypes},
    telegram::{
        emit_telegram_status, file_mtime_unix, infer_from_path, notify_settings_changed,
        reset_chat_cursor, sanitize_http_url, scan_download_dir, spawn_download_worker,
        AccountInfo, ChatItem, DownloadProgress, LoginStep, MediaIndex, MediaKind, MessageLink,
        MessageRecord, MessageSearchCursor, MessageStore, SessionPaths, TelegramHandle,
        UsageKindBytes,
    },
    tray, AppState,
};

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub identifier: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TelegramStatus {
    pub connected: bool,
    pub authorized: bool,
    pub session_exists: bool,
    pub login_step: LoginStep,
    pub password_hint: Option<String>,
    pub download_dir: String,
    pub backfill_days: i32,
    pub show_media: bool,
    pub download_concurrency: u32,
    pub autostart: bool,
    pub account: Option<AccountInfo>,
}

#[tauri::command]
#[specta::specta]
pub fn get_app_info(app: AppHandle) -> AppInfo {
    let package = app.package_info();
    AppInfo {
        name: package.name.clone(),
        version: package.version.to_string(),
        identifier: app.config().identifier.clone(),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn get_telegram_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    paths.ensure_dirs()?;

    let mut handle = state.telegram.lock().await;
    maybe_start_worker(&app, &mut handle);
    handle.ensure_account().await;
    Ok(status_from(&app, &handle, &paths))
}

#[tauri::command]
#[specta::specta]
pub async fn connect_telegram(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    paths.ensure_dirs()?;

    let mut handle = state.telegram.lock().await;
    handle.ensure_connected(&app).await?;
    maybe_start_worker(&app, &mut handle);
    emit_status(&app, &handle);
    Ok(status_from(&app, &handle, &paths))
}

#[tauri::command]
#[specta::specta]
pub async fn request_login_code(
    phone: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut handle = state.telegram.lock().await;
    handle.request_login_code(&app, &phone).await?;
    maybe_start_worker(&app, &mut handle);
    emit_status(&app, &handle);
    Ok(status_from(&app, &handle, &paths))
}

#[tauri::command]
#[specta::specta]
pub async fn submit_login_code(
    code: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut handle = state.telegram.lock().await;
    handle.submit_login_code(&code).await?;
    maybe_start_worker(&app, &mut handle);
    emit_status(&app, &handle);
    Ok(status_from(&app, &handle, &paths))
}

#[tauri::command]
#[specta::specta]
pub async fn submit_password(
    password: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut handle = state.telegram.lock().await;
    handle.submit_password(&password).await?;
    maybe_start_worker(&app, &mut handle);
    emit_status(&app, &handle);
    Ok(status_from(&app, &handle, &paths))
}

#[tauri::command]
#[specta::specta]
pub async fn logout(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    state.sync.request_stop();
    {
        let mut handle = state.telegram.lock().await;
        handle.logout(&app).await?;
        emit_status(&app, &handle);
    }
    state.sync.wait_stopped().await;
    state.sync.reset_progress(&app);
    let handle = state.telegram.lock().await;
    Ok(status_from(&app, &handle, &paths))
}

#[tauri::command]
#[specta::specta]
pub async fn list_chats(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<ChatItem>, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let settings = AppSettings::load(&paths.root);

    let mut handle = state.telegram.lock().await;
    handle.ensure_connected(&app).await?;
    handle.refresh_authorized().await?;
    let mut chats = handle.list_group_channels().await?;
    maybe_start_worker(&app, &mut handle);
    for chat in &mut chats {
        chat.watched = settings.is_watched(&chat.id);
        chat.types = settings.chat_types(&chat.id);
        chat.backfill_days = settings.effective_backfill_days(&chat.id);
        chat.backfill_days_override = settings.chat_backfill_override(&chat.id);
        chat.alias = settings.chat_alias(&chat.id);
    }
    Ok(chats)
}

#[tauri::command]
#[specta::specta]
pub fn set_chat_watched(chat_id: String, watched: bool, app: AppHandle) -> Result<bool, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    let id = chat_id.trim().to_string();
    settings.set_chat_watched(chat_id, watched)?;
    settings.save(&paths.root)?;
    if let Some(state) = app.try_state::<AppState>() {
        if watched && settings.should_sync_chat(&id) {
            state.sync.allow_chat(&id);
        } else if !watched {
            state.sync.cancel_chat(id);
        }
    }
    notify_settings_changed(&app);
    Ok(watched)
}

#[tauri::command]
#[specta::specta]
pub fn set_chat_download_types(
    chat_id: String,
    types: ChatDownloadTypes,
    app: AppHandle,
) -> Result<ChatDownloadTypes, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    let id = chat_id.trim().to_string();
    let types = settings.set_chat_types(chat_id, types)?;
    settings.save(&paths.root)?;
    if let Some(state) = app.try_state::<AppState>() {
        if settings.should_sync_chat(&id) {
            state.sync.allow_chat(&id);
        } else {
            state.sync.cancel_chat(id);
        }
    }
    notify_settings_changed(&app);
    Ok(types)
}

#[tauri::command]
#[specta::specta]
pub fn set_backfill_days(days: i32, app: AppHandle) -> Result<i32, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    settings.set_backfill_days(days);
    settings.save(&paths.root)?;
    notify_settings_changed(&app);
    Ok(settings.backfill_days)
}

#[tauri::command]
#[specta::specta]
pub fn set_chat_backfill_days(
    chat_id: String,
    days: Option<i32>,
    app: AppHandle,
) -> Result<i32, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    settings.set_chat_backfill_days(chat_id.clone(), days)?;
    settings.save(&paths.root)?;
    notify_settings_changed(&app);
    Ok(settings.effective_backfill_days(&chat_id))
}

#[tauri::command]
#[specta::specta]
pub fn set_chat_alias(
    chat_id: String,
    alias: Option<String>,
    app: AppHandle,
) -> Result<Option<String>, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    let alias = settings.set_chat_alias(chat_id, alias)?;
    settings.save(&paths.root)?;
    notify_settings_changed(&app);
    Ok(alias)
}

#[tauri::command]
#[specta::specta]
pub fn set_show_media(enabled: bool, app: AppHandle) -> Result<bool, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    settings.show_media = enabled;
    settings.save(&paths.root)?;
    Ok(settings.show_media)
}

#[tauri::command]
#[specta::specta]
pub fn set_autostart(enabled: bool, app: AppHandle) -> Result<bool, AppError> {
    apply_autostart(&app, enabled)?;
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    settings.autostart = enabled;
    settings.save(&paths.root)?;
    Ok(enabled)
}

#[tauri::command]
#[specta::specta]
pub fn set_download_concurrency(n: u32, app: AppHandle) -> Result<u32, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    let n = settings.set_download_concurrency(n);
    settings.save(&paths.root)?;
    notify_settings_changed(&app);
    Ok(n)
}

#[tauri::command]
#[specta::specta]
pub fn get_download_status(app: AppHandle) -> DownloadProgress {
    app.state::<AppState>().sync.snapshot()
}

#[tauri::command]
#[specta::specta]
pub fn set_download_paused(paused: bool, app: AppHandle) -> Result<bool, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let mut settings = AppSettings::load(&paths.root);
    settings.download_paused = paused;
    settings.save(&paths.root)?;

    let sync = app.state::<AppState>().sync.clone();
    sync.set_paused(paused);
    let snap = sync.snapshot();
    let _ = snap.emit(&app);
    Ok(paused)
}

#[tauri::command]
#[specta::specta]
pub fn cancel_download(file_id: String, app: AppHandle) -> Result<bool, AppError> {
    let file_id = file_id.trim();
    if file_id.is_empty() {
        return Err(AppError::Io("文件 id 不能为空".into()));
    }
    app.state::<AppState>()
        .sync
        .cancel_file(file_id.to_string());
    Ok(true)
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DownloadItem {
    pub file_id: String,
    pub path: String,
    pub kind: MediaKind,
    pub size: Option<String>,
    pub chat_id: Option<String>,
    pub chat_title: Option<String>,
    pub alias: Option<String>,
    pub file_name: String,
    pub downloaded_at: Option<String>,
}

#[tauri::command]
#[specta::specta]
pub fn list_downloads(app: AppHandle) -> Result<Vec<DownloadItem>, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let settings = AppSettings::load(&paths.root);
    let index = MediaIndex::load(&paths.root);
    let mut items: Vec<DownloadItem> = index
        .files
        .iter()
        .filter(|(_, entry)| entry.path.exists())
        .map(|(file_id, entry)| {
            let inferred = infer_from_path(&entry.path);
            let chat_id = entry.chat_id.clone().or(inferred.chat_id);
            let chat_title = entry.chat_title.clone().or(inferred.chat_title);
            let file_name = entry
                .file_name
                .clone()
                .or(inferred.file_name)
                .unwrap_or_else(|| file_id.clone());
            let downloaded_at = entry.downloaded_at.or_else(|| file_mtime_unix(&entry.path));
            let alias = chat_id.as_deref().and_then(|id| settings.chat_alias(id));
            DownloadItem {
                file_id: file_id.clone(),
                path: entry.path.to_string_lossy().into_owned(),
                kind: entry.kind,
                size: entry.size.map(|n| n.to_string()),
                chat_id,
                chat_title,
                alias,
                file_name,
                downloaded_at: downloaded_at.map(|n| n.to_string()),
            }
        })
        .collect();
    items.sort_by(|left, right| {
        let ta = left
            .downloaded_at
            .as_deref()
            .and_then(|raw| raw.parse::<i64>().ok())
            .unwrap_or(0);
        let tb = right
            .downloaded_at
            .as_deref()
            .and_then(|raw| raw.parse::<i64>().ok())
            .unwrap_or(0);
        tb.cmp(&ta).then_with(|| left.file_id.cmp(&right.file_id))
    });
    Ok(items)
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KindUsage {
    pub photo: String,
    pub video: String,
    pub audio: String,
    pub document: String,
    pub other: String,
}

impl KindUsage {
    fn from_bytes(bytes: &UsageKindBytes) -> Self {
        Self {
            photo: bytes.photo.to_string(),
            video: bytes.video.to_string(),
            audio: bytes.audio.to_string(),
            document: bytes.document.to_string(),
            other: bytes.other.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ChatUsage {
    pub chat_id: Option<String>,
    pub chat_title: Option<String>,
    pub alias: Option<String>,
    pub bytes: String,
    pub kinds: KindUsage,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DownloadUsage {
    pub total: String,
    pub parts: String,
    pub kinds: KindUsage,
    pub chats: Vec<ChatUsage>,
}

#[tauri::command]
#[specta::specta]
pub fn get_download_usage(app: AppHandle) -> Result<DownloadUsage, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    let settings = AppSettings::load(&paths.root);
    let scanned = scan_download_dir(&paths.download_dir);
    let chats = scanned
        .chats
        .iter()
        .map(|chat| ChatUsage {
            alias: chat
                .chat_id
                .as_deref()
                .and_then(|id| settings.chat_alias(id)),
            chat_id: chat.chat_id.clone(),
            chat_title: chat.chat_title.clone(),
            bytes: chat.kinds.total().to_string(),
            kinds: KindUsage::from_bytes(&chat.kinds),
        })
        .collect();
    Ok(DownloadUsage {
        total: scanned.total().to_string(),
        parts: scanned.parts.to_string(),
        kinds: KindUsage::from_bytes(&scanned.kinds),
        chats,
    })
}

const MESSAGE_PAGE_DEFAULT: i32 = 50;
const MESSAGE_PAGE_MAX: i32 = 200;

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MessageItem {
    pub chat_id: String,
    pub message_id: i32,
    pub date_unix: String,
    pub sender: String,
    pub text: String,
    pub media_kind: Option<String>,
    pub media_path: Option<String>,
    pub links: Vec<MessageLink>,
}

impl MessageItem {
    fn from_record(record: MessageRecord, index: &crate::telegram::MediaIndex) -> Self {
        let media_path = record
            .media_file_id
            .as_deref()
            .and_then(|id| index.existing_path(id))
            .map(|path| path.to_string_lossy().into_owned());
        Self {
            chat_id: record.chat_id,
            message_id: record.message_id,
            date_unix: record.date_unix.to_string(),
            sender: record.sender,
            text: record.text,
            media_kind: record.media_kind,
            media_path,
            links: record.links,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    pub items: Vec<MessageItem>,
    pub has_more: bool,
}

#[tauri::command]
#[specta::specta]
pub async fn list_messages(
    chat_id: String,
    query: Option<String>,
    before_message_id: Option<i32>,
    limit: Option<i32>,
    app: AppHandle,
) -> Result<MessagePage, AppError> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return Err(AppError::Config("chat_id 不能为空".into()));
    }

    let paths = SessionPaths::resolve(&app)?;
    paths.ensure_dirs()?;
    let store = MessageStore::open(&paths.root).await?;
    let limit = limit
        .unwrap_or(MESSAGE_PAGE_DEFAULT)
        .clamp(1, MESSAGE_PAGE_MAX);
    let mut items = store
        .list_chat(chat_id, query.as_deref(), before_message_id, limit + 1)
        .await?;
    let has_more = i32::try_from(items.len()).unwrap_or(i32::MAX) > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let index = crate::telegram::MediaIndex::load(&paths.root);
    Ok(MessagePage {
        items: items
            .into_iter()
            .map(|record| MessageItem::from_record(record, &index))
            .collect(),
        has_more,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SearchCursor {
    pub date_unix: String,
    pub chat_id: String,
    pub message_id: i32,
}

#[tauri::command]
#[specta::specta]
pub async fn search_messages(
    query: String,
    cursor: Option<SearchCursor>,
    limit: Option<i32>,
    app: AppHandle,
) -> Result<MessagePage, AppError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(MessagePage {
            items: Vec::new(),
            has_more: false,
        });
    }

    let before = match cursor {
        Some(cursor) => Some(MessageSearchCursor {
            date_unix: cursor
                .date_unix
                .parse::<i64>()
                .map_err(|_| AppError::Config("date_unix 无效".into()))?,
            chat_id: cursor.chat_id,
            message_id: cursor.message_id,
        }),
        None => None,
    };

    let paths = SessionPaths::resolve(&app)?;
    paths.ensure_dirs()?;
    let store = MessageStore::open(&paths.root).await?;
    let limit = limit
        .unwrap_or(MESSAGE_PAGE_DEFAULT)
        .clamp(1, MESSAGE_PAGE_MAX);
    let mut items = store.search_all(query, before.as_ref(), limit + 1).await?;
    let has_more = i32::try_from(items.len()).unwrap_or(i32::MAX) > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let index = crate::telegram::MediaIndex::load(&paths.root);
    Ok(MessagePage {
        items: items
            .into_iter()
            .map(|record| MessageItem::from_record(record, &index))
            .collect(),
        has_more,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn clear_chat_messages(
    chat_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<u32, AppError> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return Err(AppError::Config("chat_id 不能为空".into()));
    }
    let paths = SessionPaths::resolve(&app)?;
    paths.ensure_dirs()?;
    let store = MessageStore::open(&paths.root).await?;
    let deleted = store.delete_chat(chat_id).await?;
    reset_chat_cursor(&paths.root, chat_id)?;
    state.sync.request_reset(chat_id.to_string());
    Ok(deleted)
}

#[tauri::command]
#[specta::specta]
pub fn open_url(url: String) -> Result<(), AppError> {
    let Some(url) = sanitize_http_url(&url) else {
        return Err(AppError::Config("只允许打开 http/https 链接".into()));
    };
    open::that(&url).map_err(|err| AppError::Io(err.to_string()))
}

#[tauri::command]
#[specta::specta]
pub fn open_path(path: String, app: AppHandle) -> Result<(), AppError> {
    let paths = SessionPaths::resolve(&app)?;
    paths.ensure_dirs()?;
    let target = resolve_under_dir(&paths.download_dir, &path)?;
    if !target.is_file() {
        return Err(AppError::Io("不是文件".into()));
    }
    open::that(&target).map_err(|err| AppError::Io(err.to_string()))
}

#[tauri::command]
#[specta::specta]
pub fn open_download_dir(app: AppHandle) -> Result<String, AppError> {
    let paths = SessionPaths::resolve(&app)?;
    paths.ensure_dirs()?;
    let dir = paths
        .download_dir
        .canonicalize()
        .unwrap_or_else(|_| paths.download_dir.clone());
    open::that(&dir).map_err(|err| AppError::Io(err.to_string()))?;
    Ok(dir.to_string_lossy().into_owned())
}

/// 只允许打开 `root` 目录内已存在的路径（canonicalize 后再比前缀）。
fn resolve_under_dir(root: &Path, raw: &str) -> Result<PathBuf, AppError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::Config("路径不能为空".into()));
    }
    let root = root
        .canonicalize()
        .map_err(|err| AppError::Io(err.to_string()))?;
    let target = PathBuf::from(raw);
    if !target.exists() {
        return Err(AppError::Io("文件不存在".into()));
    }
    let target = target
        .canonicalize()
        .map_err(|err| AppError::Io(err.to_string()))?;
    if !target.starts_with(&root) {
        return Err(AppError::Config("只能打开下载目录内的文件".into()));
    }
    Ok(target)
}

#[tauri::command]
#[specta::specta]
pub async fn pick_download_dir(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let mut paths = SessionPaths::resolve(&app)?;
    let current = paths.download_dir.clone();
    let window = app.get_webview_window("main");

    let mut dialog = app.dialog().file().set_title("选择下载目录");
    if current.exists() {
        dialog = dialog.set_directory(&current);
    }
    if let Some(window) = window.as_ref() {
        dialog = dialog.set_parent(window);
    }

    if let Some(folder) = dialog.blocking_pick_folder() {
        let dir = folder
            .into_path()
            .map_err(|err| AppError::Io(err.to_string()))?;
        paths.set_download_dir(dir)?;
        paths.allow_asset_access(&app);
        notify_settings_changed(&app);
    }

    let handle = state.telegram.lock().await;
    Ok(status_from(&app, &handle, &paths))
}

#[tauri::command]
#[specta::specta]
pub fn show_main_window(app: AppHandle) {
    tray::show_window(&app);
}

#[tauri::command]
#[specta::specta]
pub fn hide_main_window(app: AppHandle) {
    tray::hide_window(&app);
}

#[tauri::command]
#[specta::specta]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

fn status_from(app: &AppHandle, handle: &TelegramHandle, paths: &SessionPaths) -> TelegramStatus {
    let settings = AppSettings::load(&paths.root);
    TelegramStatus {
        connected: handle.is_connected(),
        authorized: handle.is_authorized(),
        session_exists: handle.session_exists(paths),
        login_step: handle.login_step(),
        password_hint: handle.password_hint().map(str::to_string),
        download_dir: paths.download_dir.to_string_lossy().into_owned(),
        backfill_days: settings.backfill_days,
        show_media: settings.show_media,
        download_concurrency: settings.effective_download_concurrency(),
        autostart: read_autostart(app, &settings),
        account: handle.account().cloned(),
    }
}

pub(crate) fn apply_autostart(app: &AppHandle, enabled: bool) -> Result<(), AppError> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled {
        manager
            .enable()
            .map_err(|err| AppError::Io(format!("无法开启开机自启：{err}")))?;
    } else {
        manager
            .disable()
            .map_err(|err| AppError::Io(format!("无法关闭开机自启：{err}")))?;
    }
    Ok(())
}

fn read_autostart(app: &AppHandle, settings: &AppSettings) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(settings.autostart)
}

fn maybe_start_worker(app: &AppHandle, handle: &mut TelegramHandle) {
    if !handle.is_authorized() {
        return;
    }
    if handle.client().is_none() {
        return;
    }
    spawn_download_worker(app);
}

fn emit_status(app: &AppHandle, handle: &TelegramHandle) {
    emit_telegram_status(app, handle);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tgd-open-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_under_dir_allows_file_inside() {
        let root = temp_dir("inside");
        let file = root.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let resolved = resolve_under_dir(&root, &file.to_string_lossy()).unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&root);
    }

    #[test]
    fn resolve_under_dir_rejects_outside() {
        let root = temp_dir("root");
        let outside = std::env::temp_dir().join(format!("tgd-open-outside-{}", std::process::id()));
        std::fs::write(&outside, "y").unwrap();
        let err = resolve_under_dir(&root, &outside.to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("只能打开下载目录内"));
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir(&root);
    }

    #[test]
    fn resolve_under_dir_rejects_empty_and_missing() {
        let root = temp_dir("missing");
        assert!(resolve_under_dir(&root, "   ").is_err());
        assert!(resolve_under_dir(&root, "/no/such/tgd-file").is_err());
        let _ = std::fs::remove_dir(&root);
    }
}
