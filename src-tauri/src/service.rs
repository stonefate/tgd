use std::path::{Path, PathBuf};

use crate::commands::{
    AppInfo, ChatUsage, DownloadItem, DownloadUsage, KindUsage, MessageItem, MessagePage,
    SearchCursor, TelegramStatus,
};
use crate::error::AppError;
use crate::runtime::AppCtx;
use crate::settings::{AppSettings, ChatDownloadTypes, ProxyConfig};
use crate::telegram::{
    emit_telegram_status, file_mtime_unix, infer_from_path, notify_settings_changed,
    reset_chat_cursor, scan_download_dir, spawn_download_worker, DownloadProgress, MediaIndex,
    MessageSearchCursor, MessageStore, SessionPaths, TelegramHandle, UsageKindBytes,
};

const MESSAGE_PAGE_DEFAULT: i32 = 50;
const MESSAGE_PAGE_MAX: i32 = 200;

#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub fn app_info() -> AppInfo {
    AppInfo {
        name: env!("CARGO_PKG_NAME").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        identifier: "com.tgd.app".into(),
    }
}

pub fn status_from(
    handle: &TelegramHandle,
    paths: &SessionPaths,
    autostart: bool,
) -> TelegramStatus {
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
        autostart,
        account: handle.account().cloned(),
        proxy: settings.proxy_config(),
    }
}

pub fn maybe_start_worker(ctx: &AppCtx, handle: &TelegramHandle) {
    if !handle.is_authorized() || handle.client().is_none() {
        return;
    }
    spawn_download_worker(ctx.clone());
}

fn emit_status(ctx: &AppCtx, handle: &TelegramHandle) {
    emit_telegram_status(&ctx.events, handle);
}

pub async fn get_telegram_status(
    ctx: &AppCtx,
    autostart: bool,
) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    paths.ensure_dirs()?;
    let mut handle = ctx.telegram.lock().await;
    maybe_start_worker(ctx, &handle);
    handle.ensure_account().await;
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn connect_telegram(ctx: &AppCtx, autostart: bool) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    paths.ensure_dirs()?;
    let mut handle = ctx.telegram.lock().await;
    handle.ensure_connected(&paths).await?;
    maybe_start_worker(ctx, &handle);
    emit_status(ctx, &handle);
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn request_login_code(
    ctx: &AppCtx,
    phone: String,
    autostart: bool,
) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    let mut handle = ctx.telegram.lock().await;
    handle.request_login_code(&paths, &phone).await?;
    maybe_start_worker(ctx, &handle);
    emit_status(ctx, &handle);
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn submit_login_code(
    ctx: &AppCtx,
    code: String,
    autostart: bool,
) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    let mut handle = ctx.telegram.lock().await;
    handle.submit_login_code(&code).await?;
    maybe_start_worker(ctx, &handle);
    emit_status(ctx, &handle);
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn submit_password(
    ctx: &AppCtx,
    password: String,
    autostart: bool,
) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    let mut handle = ctx.telegram.lock().await;
    handle.submit_password(&password).await?;
    maybe_start_worker(ctx, &handle);
    emit_status(ctx, &handle);
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn logout(ctx: &AppCtx, autostart: bool) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    ctx.sync.request_stop();
    {
        let mut handle = ctx.telegram.lock().await;
        handle.logout(&paths).await?;
        emit_status(ctx, &handle);
    }
    ctx.sync.wait_stopped().await;
    ctx.sync.reset_progress(&ctx.events);
    let handle = ctx.telegram.lock().await;
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn list_chats(ctx: &AppCtx) -> Result<Vec<crate::telegram::ChatItem>, AppError> {
    let paths = ctx.paths();
    let settings = AppSettings::load(&paths.root);
    let mut handle = ctx.telegram.lock().await;
    handle.ensure_connected(&paths).await?;
    handle.refresh_authorized().await?;
    let mut chats = handle.list_group_channels().await?;
    maybe_start_worker(ctx, &handle);
    for chat in &mut chats {
        chat.watched = settings.is_watched(&chat.id);
        chat.types = settings.chat_types(&chat.id);
        chat.backfill_days = settings.effective_backfill_days(&chat.id);
        chat.backfill_days_override = settings.chat_backfill_override(&chat.id);
        chat.alias = settings.chat_alias(&chat.id);
    }
    Ok(chats)
}

pub fn set_chat_watched(ctx: &AppCtx, chat_id: String, watched: bool) -> Result<bool, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let id = chat_id.trim().to_string();
    settings.set_chat_watched(chat_id, watched)?;
    settings.save(&paths.root)?;
    if watched && settings.should_sync_chat(&id) {
        ctx.sync.allow_chat(&id);
    } else if !watched {
        ctx.sync.cancel_chat(id);
    }
    notify_settings_changed(&ctx.sync);
    Ok(watched)
}

pub fn set_chat_download_types(
    ctx: &AppCtx,
    chat_id: String,
    types: ChatDownloadTypes,
) -> Result<ChatDownloadTypes, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let id = chat_id.trim().to_string();
    let types = settings.set_chat_types(chat_id, types)?;
    settings.save(&paths.root)?;
    if settings.should_sync_chat(&id) {
        ctx.sync.allow_chat(&id);
    } else {
        ctx.sync.cancel_chat(id);
    }
    notify_settings_changed(&ctx.sync);
    Ok(types)
}

pub fn set_backfill_days(ctx: &AppCtx, days: i32) -> Result<i32, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    settings.set_backfill_days(days);
    settings.save(&paths.root)?;
    notify_settings_changed(&ctx.sync);
    Ok(settings.backfill_days)
}

pub fn set_chat_backfill_days(
    ctx: &AppCtx,
    chat_id: String,
    days: Option<i32>,
) -> Result<i32, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    settings.set_chat_backfill_days(chat_id.clone(), days)?;
    settings.save(&paths.root)?;
    notify_settings_changed(&ctx.sync);
    Ok(settings.effective_backfill_days(&chat_id))
}

pub fn set_chat_alias(
    ctx: &AppCtx,
    chat_id: String,
    alias: Option<String>,
) -> Result<Option<String>, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let alias = settings.set_chat_alias(chat_id, alias)?;
    settings.save(&paths.root)?;
    notify_settings_changed(&ctx.sync);
    Ok(alias)
}

pub fn set_show_media(ctx: &AppCtx, enabled: bool) -> Result<bool, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    settings.show_media = enabled;
    settings.save(&paths.root)?;
    Ok(settings.show_media)
}

pub fn set_download_concurrency(ctx: &AppCtx, n: u32) -> Result<u32, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let n = settings.set_download_concurrency(n);
    settings.save(&paths.root)?;
    notify_settings_changed(&ctx.sync);
    Ok(n)
}

fn visible_download_roots() -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |path: &str| {
        if path.is_empty() {
            return;
        }
        if !out.iter().any(|existing| existing == path) {
            out.push(path.to_string());
        }
    };
    push("/downloads");
    for n in 1..=6 {
        let root = format!("/vol{n}");
        if Path::new(&root).is_dir() {
            push(&root);
        }
    }
    if let Ok(text) = std::fs::read_to_string("/proc/self/mounts") {
        for line in text.lines() {
            let Some(dest) = line.split_whitespace().nth(1) else {
                continue;
            };
            if dest.starts_with("/vol") || dest.starts_with("/mnt") || dest.starts_with("/share") {
                push(dest);
            }
        }
    }
    out
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadDirOptions {
    pub current: String,
    pub available: Vec<String>,
}

pub fn download_dir_options(ctx: &AppCtx) -> DownloadDirOptions {
    DownloadDirOptions {
        current: ctx.paths().download_dir.to_string_lossy().into_owned(),
        available: visible_download_roots(),
    }
}

fn prepare_download_dir(raw: &str) -> Result<PathBuf, AppError> {
    let path = SessionPaths::parse_download_dir(raw)?;
    #[cfg(not(feature = "desktop"))]
    {
        let downloads = PathBuf::from("/downloads");
        if path == downloads || path.starts_with(&downloads) {
            return Ok(path);
        }
        if path.is_dir() {
            return Ok(path);
        }
        let available = visible_download_roots();
        let hint = if available.iter().any(|item| item.starts_with("/vol")) {
            format!(
                "容器内没有「{}」。已挂载：{}。请确认完整路径（飞牛「wj 的文件」一般是 /vol1/<数字>/tgd）。",
                path.display(),
                available.join("、")
            )
        } else {
            format!(
                "容器内没有「{}」，也还没挂上 /vol1。请升级应用后重启；仍不行就到飞牛应用设置里再保存一次「访问权限」。",
                path.display()
            )
        };
        return Err(AppError::Io(hint));
    }
    #[cfg(feature = "desktop")]
    Ok(path)
}

pub async fn set_download_dir(
    ctx: &AppCtx,
    autostart: bool,
    dir: String,
) -> Result<TelegramStatus, AppError> {
    let dir = prepare_download_dir(&dir)?;
    let mut paths = ctx.paths();
    paths.set_download_dir(dir)?;
    notify_settings_changed(&ctx.sync);
    let handle = ctx.telegram.lock().await;
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn set_proxy(
    ctx: &AppCtx,
    autostart: bool,
    config: ProxyConfig,
) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    settings.set_proxy(config)?;
    settings.save(&paths.root)?;
    {
        let mut handle = ctx.telegram.lock().await;
        handle.reset_transport().await;
    }
    notify_settings_changed(&ctx.sync);
    connect_telegram(ctx, autostart).await
}

pub fn get_download_status(ctx: &AppCtx) -> DownloadProgress {
    ctx.sync.snapshot()
}

pub fn set_download_paused(ctx: &AppCtx, paused: bool) -> Result<bool, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    settings.download_paused = paused;
    settings.save(&paths.root)?;
    ctx.sync.set_paused(paused);
    ctx.events.emit_download_progress(ctx.sync.snapshot());
    Ok(paused)
}

pub fn cancel_download(ctx: &AppCtx, file_id: String) -> Result<bool, AppError> {
    let file_id = file_id.trim();
    if file_id.is_empty() {
        return Err(AppError::Io("文件 id 不能为空".into()));
    }
    ctx.sync.cancel_file(file_id.to_string());
    Ok(true)
}

pub fn list_downloads(ctx: &AppCtx) -> Result<Vec<DownloadItem>, AppError> {
    let paths = ctx.paths();
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

fn kind_usage_from_bytes(bytes: &UsageKindBytes) -> KindUsage {
    KindUsage {
        photo: bytes.photo.to_string(),
        video: bytes.video.to_string(),
        audio: bytes.audio.to_string(),
        document: bytes.document.to_string(),
        other: bytes.other.to_string(),
    }
}

pub fn get_download_usage(ctx: &AppCtx) -> Result<DownloadUsage, AppError> {
    let paths = ctx.paths();
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
            kinds: kind_usage_from_bytes(&chat.kinds),
        })
        .collect();
    Ok(DownloadUsage {
        total: scanned.total().to_string(),
        parts: scanned.parts.to_string(),
        kinds: kind_usage_from_bytes(&scanned.kinds),
        chats,
    })
}

pub async fn list_messages(
    ctx: &AppCtx,
    chat_id: String,
    query: Option<String>,
    before_message_id: Option<i32>,
    limit: Option<i32>,
) -> Result<MessagePage, AppError> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return Err(AppError::Config("chat_id 不能为空".into()));
    }
    let paths = ctx.paths();
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
    let index = MediaIndex::load(&paths.root);
    Ok(MessagePage {
        items: items
            .into_iter()
            .map(|record| MessageItem::from_record(record, &index))
            .collect(),
        has_more,
    })
}

pub async fn search_messages(
    ctx: &AppCtx,
    query: String,
    cursor: Option<SearchCursor>,
    limit: Option<i32>,
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
    let paths = ctx.paths();
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
    let index = MediaIndex::load(&paths.root);
    Ok(MessagePage {
        items: items
            .into_iter()
            .map(|record| MessageItem::from_record(record, &index))
            .collect(),
        has_more,
    })
}

pub async fn clear_chat_messages(ctx: &AppCtx, chat_id: String) -> Result<u32, AppError> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return Err(AppError::Config("chat_id 不能为空".into()));
    }
    let paths = ctx.paths();
    paths.ensure_dirs()?;
    let store = MessageStore::open(&paths.root).await?;
    let deleted = store.delete_chat(chat_id).await?;
    reset_chat_cursor(&paths.root, chat_id)?;
    let mut index = MediaIndex::load(&paths.root);
    index.forget_missing_for_chat(chat_id);
    index.save(&paths.root)?;
    ctx.sync.request_reset(chat_id.to_string());
    Ok(deleted)
}

/// 只允许打开 `root` 目录内已存在的路径（canonicalize 后再比前缀）。
pub fn resolve_under_dir(root: &Path, raw: &str) -> Result<PathBuf, AppError> {
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
