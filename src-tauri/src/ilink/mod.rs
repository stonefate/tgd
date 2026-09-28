mod bot;
mod notify;
mod protocol;
mod session;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use specta::Type;
#[cfg(feature = "desktop")]
use tauri_specta::Event;
use tokio::sync::Notify;

use crate::error::AppError;
use crate::runtime::{AppCtx, EventHub};
use crate::settings::AppSettings;

use notify::NotifyTracker;
use protocol::{IlinkHttp, ProtocolError, QrPoll};
use session::IlinkSession;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum IlinkQrState {
    Idle,
    Wait,
    Scanned,
    Confirmed,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[cfg_attr(feature = "desktop", derive(Event))]
#[serde(rename_all = "camelCase")]
pub struct IlinkStatus {
    pub enabled: bool,
    pub logged_in: bool,
    pub bound: bool,
    /// 登录二维码图片（SVG data URI），后端由微信返回的待编码内容渲染而成
    pub qr_url: Option<String>,
    pub qr_state: IlinkQrState,
    pub last_error: Option<String>,
    pub bound_user_hint: Option<String>,
}

impl Default for IlinkStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            logged_in: false,
            bound: false,
            qr_url: None,
            qr_state: IlinkQrState::Idle,
            last_error: None,
            bound_user_hint: None,
        }
    }
}

#[derive(Clone)]
pub struct IlinkHandle {
    inner: Arc<IlinkInner>,
}

struct IlinkInner {
    status: Mutex<IlinkStatus>,
    session: Mutex<IlinkSession>,
    login_qr: Mutex<Option<String>>,
    wake: Notify,
    login_cancel: AtomicBool,
    generation: AtomicU64,
}

impl IlinkHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(IlinkInner {
                status: Mutex::new(IlinkStatus::default()),
                session: Mutex::new(IlinkSession::default()),
                login_qr: Mutex::new(None),
                wake: Notify::new(),
                login_cancel: AtomicBool::new(false),
                generation: AtomicU64::new(1),
            }),
        }
    }

    pub fn snapshot(&self) -> IlinkStatus {
        self.inner
            .status
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn wake(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        // notify_one 会存许可，等待方尚未注册时唤醒也不丢
        self.inner.wake.notify_one();
    }

    fn session(&self) -> IlinkSession {
        self.inner
            .session
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn persist_status(
        &self,
        events: &EventHub,
        patch: impl FnOnce(&mut IlinkStatus, &IlinkSession),
    ) {
        let session = self.session();
        let snap = {
            let Ok(mut status) = self.inner.status.lock() else {
                return;
            };
            patch(&mut status, &session);
            status.logged_in = session.logged_in();
            status.bound = session.bound();
            status.bound_user_hint = session.user_hint();
            status.clone()
        };
        events.emit_ilink_status(snap);
    }

    fn replace_session(
        &self,
        root: &std::path::Path,
        session: IlinkSession,
    ) -> Result<(), AppError> {
        session.save(root)?;
        if let Ok(mut guard) = self.inner.session.lock() {
            *guard = session;
        }
        Ok(())
    }

    pub fn hydrate(&self, ctx: &AppCtx) {
        let paths = ctx.paths();
        let settings = AppSettings::load(&paths.root);
        let session = IlinkSession::load(&paths.root);
        if let Ok(mut guard) = self.inner.session.lock() {
            *guard = session.clone();
        }
        self.persist_status(&ctx.events, |status, session| {
            status.enabled = settings.ilink_notify_enabled;
            status.logged_in = session.logged_in();
            status.bound = session.bound();
            status.bound_user_hint = session.user_hint();
        });
    }

    pub async fn start_login(&self, ctx: &AppCtx) -> Result<IlinkStatus, AppError> {
        self.inner.login_cancel.store(true, Ordering::SeqCst);
        if let Ok(mut qr) = self.inner.login_qr.lock() {
            *qr = None;
        }
        let proxy = AppSettings::load(&ctx.paths().root).effective_proxy_url();
        let http = IlinkHttp::new(proxy.as_deref(), protocol::DEFAULT_BASE_URL, None)?;
        let qr = http.get_qrcode().await?;
        self.inner.login_cancel.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = self.inner.login_qr.lock() {
            *slot = Some(qr.qrcode.clone());
        }
        let image = protocol::qr_data_uri(&qr.content);
        self.persist_status(&ctx.events, |status, _| {
            status.qr_url = image.clone();
            status.qr_state = IlinkQrState::Wait;
            status.last_error = None;
        });
        let handle = self.clone();
        let ctx = ctx.clone();
        let qrcode = qr.qrcode;
        tokio::spawn(async move {
            poll_qr_login(handle, ctx, qrcode).await;
        });
        Ok(self.snapshot())
    }

    pub fn logout(&self, ctx: &AppCtx) -> Result<IlinkStatus, AppError> {
        self.inner.login_cancel.store(true, Ordering::SeqCst);
        if let Ok(mut qr) = self.inner.login_qr.lock() {
            *qr = None;
        }
        IlinkSession::delete(&ctx.paths().root);
        if let Ok(mut guard) = self.inner.session.lock() {
            *guard = IlinkSession::default();
        }
        self.persist_status(&ctx.events, |status, _| {
            status.qr_url = None;
            status.qr_state = IlinkQrState::Idle;
            status.last_error = None;
        });
        self.wake();
        Ok(self.snapshot())
    }

    pub fn set_enabled(&self, ctx: &AppCtx, enabled: bool) -> Result<IlinkStatus, AppError> {
        let paths = ctx.paths();
        let mut settings = AppSettings::load(&paths.root);
        settings.ilink_notify_enabled = enabled;
        settings.save(&paths.root)?;
        self.persist_status(&ctx.events, |status, _| {
            status.enabled = enabled;
        });
        self.inner.wake.notify_one();
        Ok(self.snapshot())
    }

    pub async fn send_test(&self, ctx: &AppCtx) -> Result<IlinkStatus, AppError> {
        match self.send_text(ctx, "[纸飞机下载器] 通知测试").await {
            Ok(()) => {
                self.persist_status(&ctx.events, |status, _| {
                    status.last_error = None;
                });
                Ok(self.snapshot())
            }
            Err(ProtocolError::Expired) => {
                self.mark_expired(ctx);
                Err(AppError::Config(ProtocolError::Expired.message()))
            }
            Err(err) => {
                let message = err.message();
                self.persist_status(&ctx.events, |status, _| {
                    status.last_error = Some(message.clone());
                });
                Err(AppError::Config(message))
            }
        }
    }

    async fn send_text(&self, ctx: &AppCtx, text: &str) -> Result<(), ProtocolError> {
        let session = self.session();
        let to = session
            .target_user_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or(ProtocolError::Http(
                "尚未绑定，请先给机器人发一条「绑定」".into(),
            ))?;
        let token = session
            .context_token
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or(ProtocolError::Http(
                "尚未绑定，请先给机器人发一条「绑定」".into(),
            ))?;
        send_to(ctx, &session, to, token, text).await
    }

    fn mark_expired(&self, ctx: &AppCtx) {
        IlinkSession::delete(&ctx.paths().root);
        if let Ok(mut guard) = self.inner.session.lock() {
            *guard = IlinkSession::default();
        }
        self.persist_status(&ctx.events, |status, _| {
            status.qr_url = None;
            status.qr_state = IlinkQrState::Idle;
            status.last_error = Some("微信登录已过期，请重新扫码".into());
        });
        self.wake();
    }
}

impl Default for IlinkHandle {
    fn default() -> Self {
        Self::new()
    }
}

pub fn spawn_ilink_worker(ctx: AppCtx) {
    ctx.ilink.hydrate(&ctx);
    let updates = ctx.clone();
    tokio::spawn(async move { run_updates(updates).await });
    let notify = ctx.clone();
    tokio::spawn(async move { run_notify(notify).await });
}

async fn poll_qr_login(handle: IlinkHandle, ctx: AppCtx, qrcode: String) {
    let proxy = AppSettings::load(&ctx.paths().root).effective_proxy_url();
    let Ok(http) = IlinkHttp::new(proxy.as_deref(), protocol::DEFAULT_BASE_URL, None) else {
        return;
    };
    loop {
        if handle.inner.login_cancel.load(Ordering::SeqCst) {
            return;
        }
        let still = handle
            .inner
            .login_qr
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        if still.as_deref() != Some(qrcode.as_str()) {
            return;
        }
        match http.poll_qr_status(&qrcode).await {
            Ok(login) => match login.poll {
                QrPoll::Wait => {
                    handle.persist_status(&ctx.events, |status, _| {
                        status.qr_state = IlinkQrState::Wait;
                    });
                }
                QrPoll::Scanned => {
                    handle.persist_status(&ctx.events, |status, _| {
                        status.qr_state = IlinkQrState::Scanned;
                    });
                }
                QrPoll::Expired => {
                    handle.persist_status(&ctx.events, |status, _| {
                        status.qr_state = IlinkQrState::Expired;
                        status.last_error = Some("二维码已过期".into());
                    });
                    return;
                }
                QrPoll::Confirmed => {
                    let token = login.bot_token.unwrap_or_default();
                    if token.is_empty() {
                        handle.persist_status(&ctx.events, |status, _| {
                            status.last_error = Some("扫码成功但未返回凭证".into());
                            status.qr_state = IlinkQrState::Expired;
                        });
                        return;
                    }
                    let session = IlinkSession {
                        bot_token: token,
                        baseurl: login.baseurl.unwrap_or_default(),
                        bot_id: login.bot_id.unwrap_or_default(),
                        owner_user_id: login.owner_user_id,
                        ..Default::default()
                    };
                    if let Err(err) = handle.replace_session(&ctx.paths().root, session) {
                        handle.persist_status(&ctx.events, |status, _| {
                            status.last_error = Some(err.to_string());
                        });
                        return;
                    }
                    if let Ok(mut qr) = handle.inner.login_qr.lock() {
                        *qr = None;
                    }
                    handle.persist_status(&ctx.events, |status, _| {
                        status.qr_url = None;
                        status.qr_state = IlinkQrState::Confirmed;
                        status.last_error = None;
                    });
                    handle.wake();
                    return;
                }
            },
            Err(err) => {
                handle.persist_status(&ctx.events, |status, _| {
                    status.last_error = Some(err.to_string());
                });
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn run_updates(ctx: AppCtx) {
    let handle = ctx.ilink.clone();
    loop {
        let gen = handle.inner.generation.load(Ordering::SeqCst);
        let session = handle.session();
        if !session.logged_in() {
            handle.inner.wake.notified().await;
            continue;
        }
        let proxy = AppSettings::load(&ctx.paths().root).effective_proxy_url();
        let http = match IlinkHttp::new(
            proxy.as_deref(),
            session.base_url(),
            Some(&session.bot_token),
        ) {
            Ok(http) => http,
            Err(err) => {
                log::warn!("ilink client: {err}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let mut buf = session.get_updates_buf.clone();
        loop {
            if handle.inner.generation.load(Ordering::SeqCst) != gen {
                break;
            }
            match http.get_updates(&buf).await {
                Ok(updates) => {
                    buf = updates.buf;
                    let mut session = handle.session();
                    session.get_updates_buf = buf.clone();
                    let mut bound_now = false;
                    for msg in updates.messages {
                        if msg.message_type == 2 {
                            continue;
                        }
                        if session.bind_inbound(&msg.from_user_id, &msg.context_token) {
                            bound_now = session.bound();
                        }
                        // bot 指令：bind_inbound 之后 target 已刷新，绑定首条指令即可响应
                        if session.target_user_id.as_deref() == Some(msg.from_user_id.as_str()) {
                            if let Some(text) = msg.text.as_deref() {
                                if let Some(reply) = bot::handle_command(&ctx, text).await {
                                    if let Err(err) = send_to(
                                        &ctx,
                                        &session,
                                        &msg.from_user_id,
                                        &msg.context_token,
                                        &reply,
                                    )
                                    .await
                                    {
                                        log::warn!("ilink bot reply: {}", err.message());
                                    }
                                }
                            }
                        }
                    }
                    if let Err(err) = handle.replace_session(&ctx.paths().root, session) {
                        log::warn!("ilink session save: {err}");
                    }
                    if bound_now {
                        handle.persist_status(&ctx.events, |status, _| {
                            status.qr_state = IlinkQrState::Idle;
                            status.last_error = None;
                        });
                    }
                }
                Err(ProtocolError::Expired) => {
                    handle.mark_expired(&ctx);
                    break;
                }
                Err(err) => {
                    log::warn!("ilink getupdates: {}", err.message());
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}

async fn run_notify(ctx: AppCtx) {
    let mut rx = ctx.events.subscribe_download_progress();
    let mut tracker = NotifyTracker::new();
    loop {
        match rx.recv().await {
            Ok(progress) => {
                let Some((_kind, text)) = tracker.observe(progress, std::time::Instant::now())
                else {
                    continue;
                };
                let settings = AppSettings::load(&ctx.paths().root);
                if !settings.ilink_notify_enabled {
                    continue;
                }
                if !ctx.ilink.session().bound() {
                    continue;
                }
                match ctx.ilink.send_text(&ctx, &text).await {
                    Ok(()) => {}
                    Err(ProtocolError::Expired) => ctx.ilink.mark_expired(&ctx),
                    Err(err) => log::warn!("ilink notify: {}", err.message()),
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// 用给定会话凭证发送文本。通知走 `IlinkHandle::send_text`（取 session 里的绑定对象），
/// bot 指令回执直接用消息自带的 from/token（比 session 里存的更新鲜）。
async fn send_to(
    ctx: &AppCtx,
    session: &IlinkSession,
    to_user_id: &str,
    context_token: &str,
    text: &str,
) -> Result<(), ProtocolError> {
    if session.bot_token.trim().is_empty() {
        return Err(ProtocolError::Http("尚未登录微信".into()));
    }
    let proxy = AppSettings::load(&ctx.paths().root).effective_proxy_url();
    let http = IlinkHttp::new(
        proxy.as_deref(),
        session.base_url(),
        Some(&session.bot_token),
    )
    .map_err(|err| ProtocolError::Http(err.to_string()))?;
    http.send_text(to_user_id, context_token, text).await
}
