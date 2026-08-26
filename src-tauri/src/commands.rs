use serde::{Deserialize, Serialize};
use specta::Type;

use crate::telegram::{AccountInfo, LoginStep, MediaKind, MessageLink, MessageRecord};

#[cfg(feature = "desktop")]
use crate::{
    error::AppError,
    service,
    settings::{AppSettings, ChatDownloadTypes, ProxyConfig},
    telegram::{notify_settings_changed, sanitize_http_url, ChatItem, DownloadProgress},
};

#[cfg(feature = "desktop")]
use crate::{tray, AppState};
#[cfg(feature = "desktop")]
use tauri::{AppHandle, Manager, State};
#[cfg(feature = "desktop")]
use tauri_plugin_dialog::DialogExt;

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
    pub min_media_mb: f64,
    pub autostart: bool,
    pub account: Option<AccountInfo>,
    pub proxy: crate::settings::ProxyConfig,
    pub guest_watch: crate::settings::GuestWatchStatus,
}

#[cfg(feature = "desktop")]
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

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn get_telegram_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    service::get_telegram_status(&state.ctx, desktop_autostart(&app, &state.ctx)).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn connect_telegram(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    service::connect_telegram(&state.ctx, desktop_autostart(&app, &state.ctx)).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn request_login_code(
    phone: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    service::request_login_code(&state.ctx, phone, desktop_autostart(&app, &state.ctx)).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn submit_login_code(
    code: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    service::submit_login_code(&state.ctx, code, desktop_autostart(&app, &state.ctx)).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn submit_password(
    password: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    service::submit_password(&state.ctx, password, desktop_autostart(&app, &state.ctx)).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn logout(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    service::logout(&state.ctx, desktop_autostart(&app, &state.ctx)).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn list_chats(
    refresh: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<ChatItem>, AppError> {
    let _ = app;
    service::list_chats(&state.ctx, refresh).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn set_chat_watched(
    chat_id: String,
    watched: bool,
    app: AppHandle,
) -> Result<bool, AppError> {
    service::set_chat_watched(&app.state::<AppState>().ctx, chat_id, watched).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn set_guest_watch(
    enabled: bool,
    query: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    service::set_guest_watch(
        &state.ctx,
        desktop_autostart(&app, &state.ctx),
        enabled,
        query,
    )
    .await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_chat_download_types(
    chat_id: String,
    types: ChatDownloadTypes,
    app: AppHandle,
) -> Result<ChatDownloadTypes, AppError> {
    service::set_chat_download_types(&app.state::<AppState>().ctx, chat_id, types)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_backfill_days(days: i32, app: AppHandle) -> Result<i32, AppError> {
    service::set_backfill_days(&app.state::<AppState>().ctx, days)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_chat_backfill_days(
    chat_id: String,
    days: Option<i32>,
    app: AppHandle,
) -> Result<i32, AppError> {
    service::set_chat_backfill_days(&app.state::<AppState>().ctx, chat_id, days)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_chat_alias(
    chat_id: String,
    alias: Option<String>,
    app: AppHandle,
) -> Result<Option<String>, AppError> {
    service::set_chat_alias(&app.state::<AppState>().ctx, chat_id, alias)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_show_media(enabled: bool, app: AppHandle) -> Result<bool, AppError> {
    service::set_show_media(&app.state::<AppState>().ctx, enabled)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_autostart(enabled: bool, app: AppHandle) -> Result<bool, AppError> {
    apply_autostart(&app, enabled)?;
    let paths = app.state::<AppState>().ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    settings.autostart = enabled;
    settings.save(&paths.root)?;
    Ok(enabled)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_download_concurrency(n: u32, app: AppHandle) -> Result<u32, AppError> {
    service::set_download_concurrency(&app.state::<AppState>().ctx, n)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_min_media_mb(n: f64, app: AppHandle) -> Result<f64, AppError> {
    service::set_min_media_mb(&app.state::<AppState>().ctx, n)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn set_proxy(
    config: ProxyConfig,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    service::set_proxy(&state.ctx, desktop_autostart(&app, &state.ctx), config).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn get_download_status(app: AppHandle) -> DownloadProgress {
    service::get_download_status(&app.state::<AppState>().ctx)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn set_download_paused(paused: bool, app: AppHandle) -> Result<bool, AppError> {
    service::set_download_paused(&app.state::<AppState>().ctx, paused)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn cancel_download(file_id: String, app: AppHandle) -> Result<bool, AppError> {
    service::cancel_download(&app.state::<AppState>().ctx, file_id)
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

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn list_downloads(app: AppHandle) -> Result<Vec<DownloadItem>, AppError> {
    service::list_downloads(&app.state::<AppState>().ctx)
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

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn get_download_usage(app: AppHandle) -> Result<DownloadUsage, AppError> {
    service::get_download_usage(&app.state::<AppState>().ctx)
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MessageItem {
    pub chat_id: String,
    pub message_id: i32,
    pub date_unix: String,
    pub sender: String,
    pub text: String,
    pub media_kind: Option<String>,
    pub media_file_id: Option<String>,
    pub media_path: Option<String>,
    pub links: Vec<MessageLink>,
}

impl MessageItem {
    pub(crate) fn from_record(record: MessageRecord, index: &crate::telegram::MediaIndex) -> Self {
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
            media_file_id: record.media_file_id,
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

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn list_messages(
    chat_id: String,
    query: Option<String>,
    before_message_id: Option<i32>,
    limit: Option<i32>,
    app: AppHandle,
) -> Result<MessagePage, AppError> {
    service::list_messages(
        &app.state::<AppState>().ctx,
        chat_id,
        query,
        before_message_id,
        limit,
    )
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SearchCursor {
    pub date_unix: String,
    pub chat_id: String,
    pub message_id: i32,
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn search_messages(
    query: String,
    cursor: Option<SearchCursor>,
    limit: Option<i32>,
    app: AppHandle,
) -> Result<MessagePage, AppError> {
    service::search_messages(&app.state::<AppState>().ctx, query, cursor, limit).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn clear_chat_messages(
    chat_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<u32, AppError> {
    let _ = app;
    service::clear_chat_messages(&state.ctx, chat_id).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn check_chat_media(chat_id: String, app: AppHandle) -> Result<bool, AppError> {
    service::check_chat_media(&app.state::<AppState>().ctx, chat_id).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn redownload_message_media(
    chat_id: String,
    message_id: i32,
    app: AppHandle,
) -> Result<bool, AppError> {
    service::redownload_message_media(&app.state::<AppState>().ctx, chat_id, message_id).await
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn open_url(url: String) -> Result<(), AppError> {
    let Some(url) = sanitize_http_url(&url) else {
        return Err(AppError::Config("只允许打开 http/https 链接".into()));
    };
    open::that(&url).map_err(|err| AppError::Io(err.to_string()))
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn open_path(path: String, app: AppHandle) -> Result<(), AppError> {
    let paths = app.state::<AppState>().ctx.paths();
    paths.ensure_dirs()?;
    let target = service::resolve_under_dir(&paths.download_dir, &path)?;
    if !target.is_file() {
        return Err(AppError::Io("不是文件".into()));
    }
    open::that(&target).map_err(|err| AppError::Io(err.to_string()))
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn open_download_dir(app: AppHandle) -> Result<String, AppError> {
    let paths = app.state::<AppState>().ctx.paths();
    paths.ensure_dirs()?;
    let dir = paths
        .download_dir
        .canonicalize()
        .unwrap_or_else(|_| paths.download_dir.clone());
    open::that(&dir).map_err(|err| AppError::Io(err.to_string()))?;
    Ok(dir.to_string_lossy().into_owned())
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn pick_download_dir(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let mut paths = state.ctx.paths();
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
        notify_settings_changed(&state.ctx.sync);
    }

    let handle = state.ctx.telegram.lock().await;
    Ok(service::status_from(
        &handle,
        &paths,
        desktop_autostart(&app, &state.ctx),
    ))
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub async fn set_download_dir(
    dir: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TelegramStatus, AppError> {
    let status =
        service::set_download_dir(&state.ctx, desktop_autostart(&app, &state.ctx), dir).await?;
    state.ctx.paths().allow_asset_access(&app);
    Ok(status)
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn show_main_window(app: AppHandle) {
    tray::show_window(&app);
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn hide_main_window(app: AppHandle) {
    tray::hide_window(&app);
}

#[cfg(feature = "desktop")]
#[tauri::command]
#[specta::specta]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[cfg(feature = "desktop")]
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

#[cfg(feature = "desktop")]
fn read_autostart(app: &AppHandle, settings: &AppSettings) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(settings.autostart)
}

#[cfg(feature = "desktop")]
fn desktop_autostart(app: &AppHandle, ctx: &crate::runtime::AppCtx) -> bool {
    let settings = AppSettings::load(&ctx.paths().root);
    read_autostart(app, &settings)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::service::resolve_under_dir;

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
