use std::path::{Path, PathBuf};

use crate::commands::{
    AppInfo, ChatUsage, DownloadItem, DownloadUsage, KindUsage, MessageItem, MessagePage,
    SearchCursor, TelegramStatus,
};
use crate::error::AppError;
use crate::runtime::AppCtx;
use crate::settings::{AppSettings, ChatDownloadTypes, GuestWatchEntry, ProxyConfig};
use crate::telegram::{
    delete_chat_media, emit_telegram_status, fetch_linked_discussion, file_mtime_unix,
    find_channel_ref, infer_from_path, notify_settings_changed, probe_public_history,
    reset_chat_cursor, resolve_public_chat, scan_download_dir, spawn_download_worker, ChatIngested,
    ChatItem, ChatKind, DownloadProgress, MediaIndex, MessageSearchCursor, SessionPaths,
    TelegramHandle, UsageKindBytes,
};

const MESSAGE_PAGE_DEFAULT: i32 = 50;
const MESSAGE_PAGE_MAX: i32 = 200;

#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub fn app_info() -> AppInfo {
    AppInfo {
        name: "纸飞机下载器".to_string(),
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
        min_media_mb: settings.effective_min_media_mb(),
        autostart,
        account: handle.account().cloned(),
        proxy: settings.proxy_config(),
        guest_watch: settings.guest_watch_status(),
        guest_watches: settings.guest_watches.clone(),
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

pub async fn list_chats(
    ctx: &AppCtx,
    refresh: bool,
) -> Result<Vec<crate::telegram::ChatItem>, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let dropped = settings.drop_user_watches();
    let mut dirty = !dropped.is_empty();
    for id in dropped {
        ctx.sync.cancel_chat(id);
    }
    let mut handle = ctx.telegram.lock().await;
    handle.ensure_connected(&paths).await?;
    handle.refresh_authorized().await?;
    let mut chats = handle.list_group_channels(refresh).await?;
    maybe_start_worker(ctx, &handle);
    drop(handle);
    let pending: Vec<String> = chats
        .iter()
        .filter(|chat| {
            chat.kind == ChatKind::Channel
                && settings.is_watched(&chat.id)
                && !settings.channel_discussion.contains_key(&chat.id)
        })
        .map(|chat| chat.id.clone())
        .collect();
    for id in pending {
        attach_channel_discussion(ctx, &mut settings, &id).await;
        if settings.channel_discussion.contains_key(&id) {
            dirty = true;
        }
    }
    let watched_channels: Vec<String> = chats
        .iter()
        .filter(|chat| chat.kind == ChatKind::Channel && settings.is_watched(&chat.id))
        .map(|chat| chat.id.clone())
        .collect();
    for id in watched_channels {
        if let Some(disc) = settings.discussion_id(&id) {
            if enable_auto_discussion(ctx, &mut settings, &id, &disc) {
                dirty = true;
            }
        }
    }
    if dirty {
        settings.save(&paths.root)?;
        notify_settings_changed(&ctx.sync);
    }
    apply_chat_settings(&mut chats, &settings);
    inject_guest_chats(&mut chats, &settings);
    Ok(chats)
}

fn apply_chat_settings(chats: &mut Vec<ChatItem>, settings: &AppSettings) {
    for chat in chats.iter_mut() {
        chat.watched = settings.is_watched(&chat.id);
        chat.types = settings.chat_types(&chat.id);
        chat.backfill_days = settings.effective_backfill_days(&chat.id);
        chat.backfill_days_override = settings.chat_backfill_override(&chat.id);
        chat.alias = settings.chat_alias(&chat.id);
        chat.guest = settings.is_guest_slot(&chat.id);
    }
    inject_auto_comment_chats(chats, settings);
    decorate_discussion_links(chats, settings);
}

fn inject_guest_chats(chats: &mut Vec<ChatItem>, settings: &AppSettings) {
    for entry in &settings.guest_watches {
        let id = &entry.chat_id;
        if let Some(pos) = chats.iter().position(|chat| chat.id == *id) {
            chats[pos].guest = true;
            continue;
        }
        let username = {
            let name = entry.username.trim();
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        };
        let title = {
            let title = entry.title.trim();
            if title.is_empty() {
                format!("#{id}")
            } else {
                title.to_string()
            }
        };
        chats.insert(
            0,
            ChatItem {
                id: id.clone(),
                kind: settings.guest_chat_kind(id),
                title,
                username,
                watched: settings.is_watched(id),
                types: settings.chat_types(id),
                backfill_days: settings.effective_backfill_days(id),
                backfill_days_override: settings.chat_backfill_override(id),
                alias: settings.chat_alias(id),
                comment_of_id: None,
                comment_of_title: None,
                discussion_id: None,
                discussion_joined: None,
                guest: true,
            },
        );
    }
}

fn inject_auto_comment_chats(chats: &mut Vec<ChatItem>, settings: &AppSettings) {
    let existing: std::collections::HashSet<String> =
        chats.iter().map(|chat| chat.id.clone()).collect();
    let titles: std::collections::HashMap<String, String> = chats
        .iter()
        .map(|chat| (chat.id.clone(), chat.title.clone()))
        .collect();
    let extras: Vec<ChatItem> = settings
        .auto_comment_chats
        .iter()
        .filter(|id| !existing.contains(*id))
        .map(|id| {
            let channel_title = settings
                .comment_channel_of(id)
                .and_then(|channel_id| titles.get(&channel_id).cloned())
                .unwrap_or_else(|| "频道".into());
            ChatItem {
                id: id.clone(),
                kind: ChatKind::Group,
                title: format!("{channel_title} 的评论"),
                username: None,
                watched: settings.is_watched(id),
                types: settings.chat_types(id),
                backfill_days: settings.effective_backfill_days(id),
                backfill_days_override: settings.chat_backfill_override(id),
                alias: settings.chat_alias(id),
                comment_of_id: settings.comment_channel_of(id),
                comment_of_title: settings
                    .comment_channel_of(id)
                    .and_then(|channel_id| titles.get(&channel_id).cloned()),
                discussion_id: None,
                discussion_joined: Some(false),
                guest: false,
            }
        })
        .collect();
    chats.extend(extras);
}

fn decorate_discussion_links(chats: &mut [ChatItem], settings: &AppSettings) {
    let titles: std::collections::HashMap<String, String> = chats
        .iter()
        .map(|chat| (chat.id.clone(), chat.title.clone()))
        .collect();
    let present: std::collections::HashSet<String> = titles.keys().cloned().collect();
    for chat in chats.iter_mut() {
        if chat.kind == ChatKind::Channel {
            if let Some(disc) = settings.discussion_id(&chat.id) {
                chat.discussion_id = Some(disc.clone());
                chat.discussion_joined = Some(present.contains(&disc));
            }
        }
        if let Some(channel_id) = settings.comment_channel_of(&chat.id) {
            chat.comment_of_id = Some(channel_id.clone());
            chat.comment_of_title = titles.get(&channel_id).cloned();
        }
    }
}

fn sync_auto_discussion_types(
    settings: &mut AppSettings,
    channel_id: &str,
    types: ChatDownloadTypes,
) {
    if let Some(disc) = settings.auto_discussion_of(channel_id) {
        let _ = settings.set_chat_types(disc, types);
    }
}

fn sync_auto_discussion_backfill(settings: &mut AppSettings, channel_id: &str, days: Option<i32>) {
    if let Some(disc) = settings.auto_discussion_of(channel_id) {
        let _ = settings.set_chat_backfill_days(disc, days);
    }
}

async fn attach_channel_discussion(ctx: &AppCtx, settings: &mut AppSettings, channel_id: &str) {
    let handle = ctx.telegram.lock().await;
    let Some(client) = handle.client() else {
        return;
    };
    if !handle.is_authorized() {
        return;
    }
    drop(handle);
    let Ok(Some(channel)) = find_channel_ref(&client, channel_id).await else {
        return;
    };
    match fetch_linked_discussion(&client, channel).await {
        Ok(Some(linked)) => {
            settings.set_channel_discussion(channel_id.to_string(), Some(linked.group_id.clone()));
            enable_auto_discussion(ctx, settings, channel_id, &linked.group_id);
        }
        Ok(None) => {
            settings.set_channel_discussion(channel_id.to_string(), None);
        }
        Err(err) => log::warn!("linked discussion {channel_id}: {err}"),
    }
}

/// 评论组：勾监听、同步类型/天数。未加入也挂钩，评论靠频道帖 getReplies，不 JoinChannel。
fn enable_auto_discussion(
    ctx: &AppCtx,
    settings: &mut AppSettings,
    channel_id: &str,
    group_id: &str,
) -> bool {
    if settings.is_watched(group_id) || settings.is_auto_comment_skipped(group_id) {
        return false;
    }
    if settings
        .set_chat_watched(group_id.to_string(), true)
        .is_err()
    {
        return false;
    }
    let types = settings.chat_types(channel_id);
    let _ = settings.set_chat_types(group_id.to_string(), types);
    if let Some(days) = settings.chat_backfill_override(channel_id) {
        let _ = settings.set_chat_backfill_days(group_id.to_string(), Some(days));
    }
    settings.mark_auto_comment(group_id.to_string());
    if settings.should_sync_chat(group_id) {
        ctx.sync.allow_chat(group_id);
    }
    true
}

pub async fn set_chat_watched(
    ctx: &AppCtx,
    chat_id: String,
    watched: bool,
) -> Result<bool, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let id = chat_id.trim().to_string();
    let was_watched = settings.is_watched(&id);
    settings.set_chat_watched(chat_id, watched)?;
    if settings.is_guest_slot(&id) {
        settings.set_guest_watch_enabled(&id, watched);
    }
    if watched {
        settings.unskip_auto_comment(&id);
        if !was_watched {
            if let Some(disc) = settings.discussion_id(&id) {
                settings.unskip_auto_comment(&disc);
            }
        }
        attach_channel_discussion(ctx, &mut settings, &id).await;
        if settings.should_sync_chat(&id) {
            ctx.sync.allow_chat(&id);
        }
    } else {
        if settings.comment_channel_of(&id).is_some() {
            settings.skip_auto_comment(&id);
        } else {
            settings.unmark_auto_comment(&id);
        }
        if let Some(disc) = settings.take_auto_discussion(&id) {
            let _ = settings.set_chat_watched(disc.clone(), false);
            ctx.sync.cancel_chat(disc);
        }
        ctx.sync.cancel_chat(id);
    }
    settings.save(&paths.root)?;
    notify_settings_changed(&ctx.sync);
    Ok(watched)
}

pub async fn add_guest_watch(
    ctx: &AppCtx,
    autostart: bool,
    query: String,
) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err(AppError::Config("请填写公开用户名或 t.me 链接".into()));
    }

    let mut handle = ctx.telegram.lock().await;
    handle.ensure_connected(&paths).await?;
    handle.refresh_authorized().await?;
    let client = handle
        .client()
        .ok_or_else(|| AppError::Telegram("尚未连接 Telegram".into()))?;
    if !handle.is_authorized() {
        return Err(AppError::Telegram("尚未登录 Telegram".into()));
    }
    drop(handle);

    let resolved = resolve_public_chat(&client, &query).await?;
    probe_public_history(&client, resolved.peer).await?;
    let kind = match resolved.kind {
        ChatKind::Group => "group",
        _ => "channel",
    };

    let mut settings = AppSettings::load(&paths.root);
    let entry = GuestWatchEntry {
        enabled: true,
        query,
        chat_id: resolved.chat_id.clone(),
        title: resolved.title,
        username: resolved.username.unwrap_or_default(),
        kind: kind.to_string(),
    };
    settings.add_guest_watch(entry);
    ctx.sync.allow_chat(&resolved.chat_id);
    settings.save(&paths.root)?;
    notify_settings_changed(&ctx.sync);

    let handle = ctx.telegram.lock().await;
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn remove_guest_watch(
    ctx: &AppCtx,
    autostart: bool,
    chat_id: String,
) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let id = chat_id.trim().to_string();
    if settings.remove_guest_watch(&id).is_some() {
        ctx.sync.cancel_chat(id);
        settings.save(&paths.root)?;
        notify_settings_changed(&ctx.sync);
    }
    let handle = ctx.telegram.lock().await;
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn set_guest_watch_enabled(
    ctx: &AppCtx,
    autostart: bool,
    chat_id: String,
    enabled: bool,
) -> Result<TelegramStatus, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let id = chat_id.trim().to_string();
    if settings.set_guest_watch_enabled(&id, enabled) {
        if enabled {
            if settings.should_sync_chat(&id) {
                ctx.sync.allow_chat(&id);
            }
        } else {
            ctx.sync.cancel_chat(id);
        }
        settings.save(&paths.root)?;
        notify_settings_changed(&ctx.sync);
    }
    let handle = ctx.telegram.lock().await;
    Ok(status_from(&handle, &paths, autostart))
}

pub async fn set_guest_watch(
    ctx: &AppCtx,
    autostart: bool,
    enabled: bool,
    query: String,
) -> Result<TelegramStatus, AppError> {
    let query = query.trim().to_string();
    if enabled && !query.is_empty() {
        add_guest_watch(ctx, autostart, query).await
    } else {
        let paths = ctx.paths();
        let mut settings = AppSettings::load(&paths.root);
        if let Some(id) = settings.disable_guest_watch() {
            ctx.sync.cancel_chat(id);
            settings.save(&paths.root)?;
            notify_settings_changed(&ctx.sync);
        }
        let handle = ctx.telegram.lock().await;
        Ok(status_from(&handle, &paths, autostart))
    }
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
    sync_auto_discussion_types(&mut settings, &id, types);
    settings.save(&paths.root)?;
    if settings.should_sync_chat(&id) {
        ctx.sync.allow_chat(&id);
    } else {
        ctx.sync.cancel_chat(id.clone());
    }
    if let Some(disc) = settings.auto_discussion_of(&id) {
        if settings.should_sync_chat(&disc) {
            ctx.sync.allow_chat(&disc);
        } else {
            ctx.sync.cancel_chat(disc);
        }
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
    sync_auto_discussion_backfill(&mut settings, &chat_id, days);
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

pub fn set_min_media_mb(ctx: &AppCtx, n: f64) -> Result<f64, AppError> {
    let paths = ctx.paths();
    let mut settings = AppSettings::load(&paths.root);
    let n = settings.set_min_media_mb(n);
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
    if let Ok(dir) = std::env::var("TGD_DOWNLOAD_DIR") {
        push(dir.trim());
    }
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
                "没有「{}」。可访问：{}。飞牛「wj 的文件」一般是 /vol1/<数字>/tgd。",
                path.display(),
                available.join("、")
            )
        } else {
            format!(
                "没有「{}」。请确认路径存在；Compose 需把目录挂进容器，原生包可直接访问 /vol*。",
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
    ctx.ilink.wake();
    connect_telegram(ctx, autostart).await
}

pub fn get_ilink_status(ctx: &AppCtx) -> crate::ilink::IlinkStatus {
    ctx.ilink.snapshot()
}

pub async fn ilink_start_login(ctx: &AppCtx) -> Result<crate::ilink::IlinkStatus, AppError> {
    ctx.ilink.start_login(ctx).await
}

pub fn ilink_logout(ctx: &AppCtx) -> Result<crate::ilink::IlinkStatus, AppError> {
    ctx.ilink.logout(ctx)
}

pub fn set_ilink_notify_enabled(
    ctx: &AppCtx,
    enabled: bool,
) -> Result<crate::ilink::IlinkStatus, AppError> {
    ctx.ilink.set_enabled(ctx, enabled)
}

pub async fn ilink_send_test(ctx: &AppCtx) -> Result<crate::ilink::IlinkStatus, AppError> {
    ctx.ilink.send_test(ctx).await
}

pub fn get_download_status(ctx: &AppCtx) -> DownloadProgress {
    ctx.sync.snapshot()
}

pub fn get_recent_logs() -> Vec<crate::app_log::LogEntry> {
    crate::app_log::snapshot()
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
    ctx.events.emit_download_progress(ctx.sync.snapshot());
    Ok(true)
}

pub async fn check_chat_media(ctx: &AppCtx, chat_id: String) -> Result<bool, AppError> {
    {
        let handle = ctx.telegram.lock().await;
        if !handle.is_authorized() {
            return Err(AppError::Telegram("尚未登录".into()));
        }
    }
    let paths = ctx.paths();
    let settings = AppSettings::load(&paths.root);
    let ids = settings.media_check_ids(&chat_id)?;
    let mut queued = false;
    for id in ids {
        queued |= ctx.sync.request_check(id);
    }
    Ok(queued)
}

pub async fn redownload_message_media(
    ctx: &AppCtx,
    chat_id: String,
    message_id: i32,
) -> Result<bool, AppError> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return Err(AppError::Config("chat_id 不能为空".into()));
    }
    if message_id <= 0 {
        return Err(AppError::Config("消息 id 无效".into()));
    }
    {
        let handle = ctx.telegram.lock().await;
        if !handle.is_authorized() {
            return Err(AppError::Telegram("尚未登录".into()));
        }
    }
    let paths = ctx.paths();
    paths.ensure_dirs()?;
    let store = ctx.message_store().await?;
    let Some(record) = store.get(chat_id, message_id).await? else {
        return Err(AppError::Io("本地没有这条消息".into()));
    };
    if record.media_kind.is_none() && record.media_file_id.is_none() {
        return Err(AppError::Io("该消息没有媒体".into()));
    }
    let index = MediaIndex::load(&paths.root);
    let file_id = record.media_file_id.clone();
    let kind = record
        .media_kind
        .as_deref()
        .and_then(crate::telegram::MediaKind::from_name)
        .or_else(|| {
            file_id
                .as_deref()
                .and_then(|id| index.files.get(id).map(|entry| entry.kind))
        });
    let file_name = file_id.as_deref().and_then(|id| {
        index.files.get(id).and_then(|entry| {
            entry.file_name.clone().or_else(|| {
                entry
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
        })
    });
    Ok(ctx
        .sync
        .request_redownload(chat_id.to_string(), message_id, file_id, file_name, kind))
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
    let store = ctx.message_store().await?;
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
    let store = ctx.message_store().await?;
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
    let settings = AppSettings::load(&paths.root);
    let ids = settings.chats_to_wipe(chat_id);
    let store = ctx.message_store().await?;
    let mut index = MediaIndex::load(&paths.root);
    let mut deleted = 0;
    for id in &ids {
        deleted += store.delete_chat(id).await?;
        reset_chat_cursor(&paths.root, id)?;
        let media_paths = index.take_chat(id);
        delete_chat_media(&paths.download_dir, id, &media_paths);
        ctx.sync.cancel_chat(id.clone());
        ctx.sync.request_reset(id.clone());
        ctx.events.emit_chat_ingested(ChatIngested {
            chat_id: id.clone(),
        });
    }
    index.save(&paths.root)?;
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
