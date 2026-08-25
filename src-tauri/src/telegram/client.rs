use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::media::Media;
use grammers_client::message::Message;
use grammers_client::peer::Peer;
use grammers_client::sender::ConnectionParams;
use grammers_client::session::types::{PeerId, PeerRef};
use grammers_client::session::updates::UpdatesLike;
use grammers_client::{tl, Client, InvocationError, SenderPool, SignInError};
use grammers_session::storages::SqliteSession;
use serde::Serialize;
use specta::Type;
#[cfg(feature = "desktop")]
use tauri_specta::Event;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::AppError;
use crate::runtime::EventHub;
use crate::settings::{AppSettings, ChatDownloadTypes};
use crate::telegram::links::links_from_entities;
use crate::telegram::session::SessionPaths;
use crate::telegram::store::MessageLink;

pub fn load_dotenv() {
    let from_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".env");
    if from_manifest.is_file() {
        let _ = dotenvy::from_path(&from_manifest);
    }
    let _ = dotenvy::dotenv();
}

pub struct TelegramApi {
    pub api_id: i32,
    pub api_hash: String,
}

/// 去掉 BOM、空白和成对引号。空的 `TELEGRAM_API_ID=` 不能当数字解析。
pub(crate) fn normalize_env_value(raw: &str) -> String {
    let s = raw.trim_start_matches('\u{feff}').trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].trim().to_string();
        }
    }
    s.to_string()
}

fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Ok(raw) = std::env::var(key) {
            let value = normalize_env_value(&raw);
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

impl TelegramApi {
    pub fn from_env() -> Result<Self, AppError> {
        // 飞牛向导字段名是 wizard_*；占位 env 里空的 TELEGRAM_API_ID= 要跳过。
        let api_id = first_nonempty_env(&["TELEGRAM_API_ID", "wizard_telegram_api_id"])
            .ok_or_else(|| {
                AppError::Config("未找到 TELEGRAM_API_ID，请在设置或安装向导里填写".into())
            })?;
        let api_hash = first_nonempty_env(&["TELEGRAM_API_HASH", "wizard_telegram_api_hash"])
            .ok_or_else(|| {
                AppError::Config("未找到 TELEGRAM_API_HASH，请在设置或安装向导里填写".into())
            })?;

        let api_id = api_id.parse::<i32>().map_err(|_| {
            AppError::Config("TELEGRAM_API_ID 必须是数字，请检查飞牛运行设置里填写的 API ID".into())
        })?;
        if api_hash.is_empty() {
            return Err(AppError::Config("TELEGRAM_API_HASH 为空".into()));
        }

        Ok(Self { api_id, api_hash })
    }
}

/// 会话列表短缓存，避免每次打开都 `iter_dialogs`。
const CHAT_LIST_TTL: Duration = Duration::from_secs(60 * 60);

struct ChatListCache {
    at: Instant,
    chats: Vec<ChatItem>,
}

/// 持有 grammers 连接与登录中间态。
///
/// `LoginToken` / `PasswordToken` 不能跨命令序列化，只留在 Rust 侧。
pub struct TelegramHandle {
    client: Option<Client>,
    pool: Option<grammers_client::sender::SenderPoolFatHandle>,
    updates: Option<mpsc::UnboundedReceiver<UpdatesLike>>,
    _runner: Option<JoinHandle<()>>,
    login_token: Option<LoginToken>,
    password_token: Option<PasswordToken>,
    password_hint: Option<String>,
    connected: bool,
    authorized: bool,
    account: Option<AccountInfo>,
    chat_list: Option<ChatListCache>,
}

impl TelegramHandle {
    pub fn new() -> Self {
        Self {
            client: None,
            pool: None,
            updates: None,
            _runner: None,
            login_token: None,
            password_token: None,
            password_hint: None,
            connected: false,
            authorized: false,
            account: None,
            chat_list: None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn is_authorized(&self) -> bool {
        self.authorized
    }

    pub fn account(&self) -> Option<&AccountInfo> {
        self.account.as_ref()
    }

    /// 已登录且还没缓存身份时拉一次 `get_me`。失败不阻断连接。
    pub async fn ensure_account(&mut self) {
        if !self.authorized || self.account.is_some() {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        match client.get_me().await {
            Ok(user) => self.account = Some(AccountInfo::from_user(&user)),
            Err(err) => log::warn!("get_me failed: {err}"),
        }
    }

    pub fn session_exists(&self, paths: &SessionPaths) -> bool {
        paths.session_exists()
    }

    pub fn password_hint(&self) -> Option<&str> {
        self.password_hint.as_deref()
    }

    pub fn login_step(&self) -> LoginStep {
        if self.authorized {
            LoginStep::Authorized
        } else if self.password_token.is_some() {
            LoginStep::NeedPassword
        } else if self.login_token.is_some() {
            LoginStep::NeedCode
        } else {
            LoginStep::Idle
        }
    }

    pub fn client(&self) -> Option<Client> {
        self.client.clone()
    }

    pub fn take_updates(&mut self) -> Option<mpsc::UnboundedReceiver<UpdatesLike>> {
        self.updates.take()
    }

    pub fn take_worker_session(
        &mut self,
    ) -> Option<(Client, mpsc::UnboundedReceiver<UpdatesLike>)> {
        let client = self.client.clone()?;
        let updates = self.updates.take()?;
        Some((client, updates))
    }

    /// 拆掉已死的传输再连。保留 session 文件，重连后复用授权。
    pub async fn reconnect_transport(
        &mut self,
        paths: &SessionPaths,
    ) -> Result<(Client, mpsc::UnboundedReceiver<UpdatesLike>), AppError> {
        self.reset_transport().await;
        let client = self.ensure_connected(paths).await?;
        let updates = self
            .updates
            .take()
            .ok_or_else(|| AppError::Telegram("重连后未拿到更新通道".into()))?;
        Ok((client, updates))
    }

    /// 向 Telegram 注销并删本地 session。消息库和已下载文件不动。
    pub async fn logout(&mut self, paths: &SessionPaths) -> Result<(), AppError> {
        if let Some(client) = self.client.clone() {
            let _ = tokio::time::timeout(Duration::from_secs(5), client.sign_out()).await;
        }
        self.reset_connection().await;
        paths.remove_session()?;
        Ok(())
    }

    pub async fn ensure_connected(&mut self, paths: &SessionPaths) -> Result<Client, AppError> {
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }

        let api = TelegramApi::from_env()?;
        paths.ensure_dirs()?;

        let session = Arc::new(
            SqliteSession::open(&paths.session_file)
                .await
                .map_err(|err| AppError::Io(err.to_string()))?,
        );

        let proxy_url = AppSettings::load(&paths.root).effective_proxy_url();
        let params = ConnectionParams {
            proxy_url: proxy_url.clone(),
            ..ConnectionParams::default()
        };
        if proxy_url.is_some() {
            log::info!("telegram connecting via socks5 proxy");
        }
        let SenderPool {
            runner,
            handle,
            updates,
        } = SenderPool::with_configuration(session, api.api_id, params);
        let client = Client::new(handle.clone());
        let runner = tokio::spawn(runner.run());

        self.pool = Some(handle);
        self.updates = Some(updates);
        self._runner = Some(runner);
        self.client = Some(client.clone());
        self.connected = true;

        match tokio::time::timeout(Duration::from_secs(25), client.is_authorized()).await {
            Ok(Ok(authorized)) => {
                self.authorized = authorized;
                if authorized {
                    self.login_token = None;
                    self.password_token = None;
                    self.password_hint = None;
                } else {
                    self.account = None;
                }
            }
            Ok(Err(err)) => {
                self.reset_connection().await;
                return Err(err.into());
            }
            Err(_) => {
                self.reset_connection().await;
                return Err(AppError::Telegram(
                    "连接 Telegram 超时，飞牛需能访问 Telegram 网络（或给容器配代理）".into(),
                ));
            }
        }

        self.ensure_account().await;
        Ok(client)
    }

    pub async fn refresh_authorized(&mut self) -> Result<bool, AppError> {
        let Some(client) = &self.client else {
            self.authorized = false;
            self.chat_list = None;
            return Ok(false);
        };

        let authorized = client.is_authorized().await?;
        self.authorized = authorized;
        if authorized {
            self.login_token = None;
            self.password_token = None;
            self.password_hint = None;
            self.ensure_account().await;
        } else {
            self.account = None;
            self.chat_list = None;
        }
        Ok(authorized)
    }

    pub async fn request_login_code(
        &mut self,
        paths: &SessionPaths,
        phone: &str,
    ) -> Result<(), AppError> {
        let phone = phone.trim();
        if phone.is_empty() {
            return Err(AppError::Telegram(
                "请输入手机号（国际格式，如 +86…）".into(),
            ));
        }

        let api = TelegramApi::from_env()?;
        let client = self.ensure_connected(paths).await?;
        if self.refresh_authorized().await? {
            return Ok(());
        }

        match client.request_login_code(phone, &api.api_hash).await {
            Ok(token) => {
                self.store_login_token(token);
                Ok(())
            }
            Err(err) if err.is("AUTH_RESTART*") => {
                log::warn!("auth.sendCode 返回 AUTH_RESTART，重建 session 后重试");
                self.restart_unauthorized_session(paths).await?;
                let client = self.ensure_connected(paths).await?;
                let token = client
                    .request_login_code(phone, &api.api_hash)
                    .await
                    .map_err(map_send_code_error)?;
                self.store_login_token(token);
                Ok(())
            }
            Err(err) => Err(map_send_code_error(err)),
        }
    }

    pub async fn submit_login_code(&mut self, code: &str) -> Result<(), AppError> {
        let code = code.trim();
        if code.is_empty() {
            return Err(AppError::Telegram("请输入验证码".into()));
        }

        let client = self
            .client
            .clone()
            .ok_or_else(|| AppError::Telegram("尚未连接 Telegram".into()))?;
        let token = self
            .login_token
            .as_ref()
            .ok_or_else(|| AppError::Telegram("请先发送验证码".into()))?;

        match client.sign_in(token, code).await {
            Ok(_) => {
                self.authorized = true;
                self.login_token = None;
                self.password_token = None;
                self.password_hint = None;
                self.ensure_account().await;
                Ok(())
            }
            Err(SignInError::PasswordRequired(token)) => {
                self.store_password_token(token);
                Ok(())
            }
            Err(SignInError::InvalidCode) => Err(AppError::Telegram("验证码不正确".into())),
            Err(SignInError::SignUpRequired) => Err(AppError::Telegram(
                "该手机号尚未注册，请先用官方客户端注册".into(),
            )),
            Err(SignInError::InvalidPassword(token)) => {
                self.password_token = Some(token);
                Err(AppError::Telegram("两步验证密码不正确".into()))
            }
            Err(SignInError::Other(err)) => Err(err.into()),
        }
    }

    pub async fn submit_password(&mut self, password: &str) -> Result<(), AppError> {
        let password = password.trim();
        if password.is_empty() {
            return Err(AppError::Telegram("请输入两步验证密码".into()));
        }

        let client = self
            .client
            .clone()
            .ok_or_else(|| AppError::Telegram("尚未连接 Telegram".into()))?;
        let token = self
            .password_token
            .take()
            .ok_or_else(|| AppError::Telegram("当前不需要两步验证".into()))?;

        match client.check_password(token, password).await {
            Ok(_) => {
                self.authorized = true;
                self.login_token = None;
                self.password_token = None;
                self.password_hint = None;
                self.ensure_account().await;
                Ok(())
            }
            Err(SignInError::InvalidPassword(token)) => {
                self.store_password_token(token);
                Err(AppError::Telegram("两步验证密码不正确".into()))
            }
            Err(SignInError::PasswordRequired(token)) => {
                self.store_password_token(token);
                Err(AppError::Telegram("仍需要两步验证密码".into()))
            }
            Err(SignInError::InvalidCode) => Err(AppError::Telegram("验证码不正确".into())),
            Err(SignInError::SignUpRequired) => Err(AppError::Telegram(
                "该手机号尚未注册，请先用官方客户端注册".into(),
            )),
            Err(SignInError::Other(err)) => Err(AppError::Telegram(format!(
                "两步验证失败，请重新提交验证码：{err}"
            ))),
        }
    }

    pub async fn list_group_channels(&mut self, refresh: bool) -> Result<Vec<ChatItem>, AppError> {
        let client = self
            .client
            .clone()
            .ok_or_else(|| AppError::Telegram("尚未连接 Telegram".into()))?;
        if !self.authorized {
            return Err(AppError::Telegram("尚未登录 Telegram".into()));
        }
        if let Some(cache) = &self.chat_list {
            if chat_list_cache_hit(refresh, Some(cache.at.elapsed()), CHAT_LIST_TTL) {
                return Ok(cache.chats.clone());
            }
        }

        let mut chats = Vec::new();
        let mut dialogs = client.iter_dialogs();
        while let Some(dialog) = dialogs.next().await? {
            match dialog.peer() {
                Peer::Group(group) => {
                    let id = group.id().to_string();
                    let title = group
                        .title()
                        .map(str::trim)
                        .filter(|title| !title.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("#{id}"));
                    chats.push(chat_item(
                        id,
                        ChatKind::Group,
                        title,
                        group.username().map(str::to_string),
                    ));
                }
                Peer::Channel(channel) => {
                    let id = channel.id().to_string();
                    let raw_title = channel.title().trim();
                    let title = if raw_title.is_empty() {
                        format!("#{id}")
                    } else {
                        raw_title.to_string()
                    };
                    chats.push(chat_item(
                        id,
                        ChatKind::Channel,
                        title,
                        channel.username().map(str::to_string),
                    ));
                }
                Peer::User(user) => {
                    if !user.is_bot() || user.deleted() {
                        continue;
                    }
                    let id = user.id().to_string();
                    let full_name = user.full_name();
                    let trimmed = full_name.trim();
                    let title = if !trimmed.is_empty() {
                        trimmed.to_string()
                    } else {
                        user.username()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("#{id}"))
                    };
                    chats.push(chat_item(
                        id,
                        ChatKind::Bot,
                        title,
                        user.username().map(str::to_string),
                    ));
                }
            }
        }

        self.chat_list = Some(ChatListCache {
            at: Instant::now(),
            chats: chats.clone(),
        });
        Ok(chats)
    }

    fn store_login_token(&mut self, token: LoginToken) {
        self.login_token = Some(token);
        self.password_token = None;
        self.password_hint = None;
    }

    fn store_password_token(&mut self, token: PasswordToken) {
        self.password_hint = token
            .hint()
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(str::to_string);
        self.password_token = Some(token);
    }

    async fn restart_unauthorized_session(&mut self, paths: &SessionPaths) -> Result<(), AppError> {
        self.reset_connection().await;
        paths.remove_session()?;
        Ok(())
    }

    pub(crate) async fn reset_transport(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.quit();
        }
        self.client = None;
        self.updates = None;
        if let Some(runner) = self._runner.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), runner).await;
        }
        self.connected = false;
    }

    async fn reset_connection(&mut self) {
        self.reset_transport().await;
        self.login_token = None;
        self.password_token = None;
        self.password_hint = None;
        self.authorized = false;
        self.account = None;
        self.chat_list = None;
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[cfg_attr(feature = "desktop", derive(Event))]
#[serde(rename_all = "camelCase")]
pub struct TelegramStatusChanged {
    pub connected: bool,
    pub authorized: bool,
}

pub fn emit_telegram_status(events: &EventHub, handle: &TelegramHandle) {
    events.emit_telegram_status(TelegramStatusChanged {
        connected: handle.is_connected(),
        authorized: handle.is_authorized(),
    });
}

fn map_send_code_error(err: grammers_client::InvocationError) -> AppError {
    if err.is("AUTH_RESTART*") {
        AppError::Telegram("Telegram 要求重启登录，已重置本地会话。请再点一次发送验证码。".into())
    } else if err.is("PHONE_NUMBER_INVALID") {
        AppError::Telegram("手机号格式无效，请使用国际格式，如 +86…".into())
    } else if err.is("PHONE_NUMBER_BANNED") {
        AppError::Telegram("该手机号已被 Telegram 封禁".into())
    } else if err.is("PHONE_NUMBER_FLOOD") || err.is("FLOOD_WAIT*") {
        AppError::Telegram(format!("发送验证码过于频繁：{err}"))
    } else if err.is("API_ID_INVALID") || err.is("API_ID_PUBLISHED_FLOOD") {
        AppError::Telegram("api_id / api_hash 无效或已被限制，请检查 .env".into())
    } else {
        err.into()
    }
}

impl Default for TelegramHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TelegramHandle {
    fn drop(&mut self) {
        if let Some(pool) = &self.pool {
            pool.quit();
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub name: String,
    pub username: Option<String>,
    pub phone: Option<String>,
}

impl AccountInfo {
    fn from_user(user: &grammers_client::peer::User) -> Self {
        let username = user
            .username()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let name = user.full_name();
        let name = if name.trim().is_empty() {
            username
                .as_deref()
                .map(|name| format!("@{name}"))
                .unwrap_or_else(|| "已登录".into())
        } else {
            name
        };
        Self {
            name,
            username,
            phone: user
                .phone()
                .map(str::trim)
                .filter(|phone| !phone.is_empty())
                .map(str::to_string),
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum LoginStep {
    Idle,
    NeedCode,
    NeedPassword,
    Authorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ChatKind {
    Group,
    Channel,
    Bot,
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChatItem {
    pub id: String,
    pub kind: ChatKind,
    pub title: String,
    pub username: Option<String>,
    /// 是否在监听下载名单里。由 `list_chats` 根据 `settings.json` 填入。
    pub watched: bool,
    pub types: ChatDownloadTypes,
    /// 有效回爬天数（覆盖或全局）。
    pub backfill_days: i32,
    /// 该群单独设置的天数；`None` 表示跟全局。
    pub backfill_days_override: Option<i32>,
    /// 本地别名；`None` 表示用 Telegram 原名。
    pub alias: Option<String>,
    /// 若这是频道评论组，对应的频道 id。
    pub comment_of_id: Option<String>,
    /// 对应频道标题，列表副标题用。
    pub comment_of_title: Option<String>,
    /// 若这是频道，关联讨论组 id。
    pub discussion_id: Option<String>,
    /// 账号是否已加入该讨论组。没有评论组时为 `None`。
    pub discussion_joined: Option<bool>,
    /// 未加入的公开预览槽位（全局最多一个）。
    #[serde(default)]
    pub guest: bool,
}

fn chat_item(id: String, kind: ChatKind, title: String, username: Option<String>) -> ChatItem {
    ChatItem {
        id,
        kind,
        title,
        username,
        watched: false,
        types: ChatDownloadTypes::default(),
        backfill_days: 0,
        backfill_days_override: None,
        alias: None,
        comment_of_id: None,
        comment_of_title: None,
        discussion_id: None,
        discussion_joined: None,
        guest: false,
    }
}

/// 频道关联的讨论组（评论区）。
#[derive(Debug, Clone)]
pub struct LinkedDiscussion {
    pub group_id: String,
    pub title: String,
    pub joined: bool,
    pub peer: Option<PeerRef>,
}

fn peer_left(peer: &Peer) -> bool {
    match peer {
        Peer::User(_) => false,
        Peer::Channel(channel) => channel.raw.left,
        Peer::Group(group) => match &group.raw {
            tl::enums::Chat::Channel(channel) => channel.left,
            tl::enums::Chat::Chat(chat) => chat.left,
            tl::enums::Chat::Forbidden(_)
            | tl::enums::Chat::ChannelForbidden(_)
            | tl::enums::Chat::Empty(_) => true,
        },
    }
}

const COMMENT_PAGE: i32 = 100;
const COMMENT_MAX_PAGES: usize = 10;

/// 频道帖评论。不入群，走 getDiscussionMessage / getReplies。
#[derive(Clone)]
pub struct CommentMessage {
    pub chat_id: String,
    pub message_id: i32,
    pub date_unix: i64,
    pub sender: String,
    pub text: String,
    pub media: Option<Media>,
    pub links: Vec<MessageLink>,
}

/// 频道帖的评论条数。只有 `comments` 标记的才是频道评论区。
pub fn channel_comment_count(message: &Message) -> Option<i32> {
    comment_count_from_raw(&message.raw)
}

pub(crate) fn comment_count_from_raw(raw: &tl::enums::Message) -> Option<i32> {
    match raw {
        tl::enums::Message::Message(message) => match &message.replies {
            Some(tl::enums::MessageReplies::Replies(replies))
                if replies.comments && replies.replies > 0 =>
            {
                Some(replies.replies)
            }
            _ => None,
        },
        _ => None,
    }
}

/// 拉某条频道帖的评论。不 JoinChannel。消息里的群链接也不会加入。
pub async fn fetch_post_comments(
    client: &Client,
    channel: PeerRef,
    post_id: i32,
) -> Result<Vec<CommentMessage>, InvocationError> {
    let _ = client
        .invoke(&tl::functions::messages::GetDiscussionMessage {
            peer: channel.into(),
            msg_id: post_id,
        })
        .await?;

    let mut out = Vec::new();
    let mut offset_id = 0;
    for _ in 0..COMMENT_MAX_PAGES {
        let result = client
            .invoke(&tl::functions::messages::GetReplies {
                peer: channel.into(),
                msg_id: post_id,
                offset_id,
                offset_date: 0,
                add_offset: 0,
                limit: COMMENT_PAGE,
                max_id: 0,
                min_id: 0,
                hash: 0,
            })
            .await?;
        let (messages, users, _chats) = unpack_messages(result);
        if messages.is_empty() {
            break;
        }
        let names = user_names(&users);
        let mut oldest = i32::MAX;
        let count = messages.len();
        for raw in messages {
            if let tl::enums::Message::Message(message) = &raw {
                oldest = oldest.min(message.id);
            }
            if let Some(comment) = parse_comment(raw, &names) {
                out.push(comment);
            }
        }
        if count < COMMENT_PAGE as usize || oldest == i32::MAX || oldest == offset_id {
            break;
        }
        offset_id = oldest;
    }
    Ok(out)
}

fn unpack_messages(
    res: tl::enums::messages::Messages,
) -> (
    Vec<tl::enums::Message>,
    Vec<tl::enums::User>,
    Vec<tl::enums::Chat>,
) {
    use tl::enums::messages::Messages;
    match res {
        Messages::Messages(m) => (m.messages, m.users, m.chats),
        Messages::Slice(m) => (m.messages, m.users, m.chats),
        Messages::ChannelMessages(m) => (m.messages, m.users, m.chats),
        Messages::NotModified(_) => (Vec::new(), Vec::new(), Vec::new()),
    }
}

fn user_names(users: &[tl::enums::User]) -> std::collections::HashMap<i64, String> {
    let mut names = std::collections::HashMap::new();
    for user in users {
        let tl::enums::User::User(user) = user else {
            continue;
        };
        let mut name = String::new();
        if let Some(first) = user.first_name.as_deref() {
            name.push_str(first.trim());
        }
        if let Some(last) = user.last_name.as_deref() {
            let last = last.trim();
            if !last.is_empty() {
                if !name.is_empty() {
                    name.push(' ');
                }
                name.push_str(last);
            }
        }
        if name.is_empty() {
            if let Some(username) = user.username.as_deref() {
                name = format!("@{username}");
            } else {
                name = user.id.to_string();
            }
        }
        names.insert(user.id, name);
    }
    names
}

fn parse_comment(
    raw: tl::enums::Message,
    users: &std::collections::HashMap<i64, String>,
) -> Option<CommentMessage> {
    let tl::enums::Message::Message(message) = raw else {
        return None;
    };
    if message.post {
        return None;
    }
    let chat_id = PeerId::from(message.peer_id).to_string();
    let text = message.message;
    let links = message
        .entities
        .as_deref()
        .map(|entities| links_from_entities(&text, entities))
        .unwrap_or_default();
    let sender = match message.from_id.as_ref() {
        Some(tl::enums::Peer::User(user)) => users
            .get(&user.user_id)
            .cloned()
            .unwrap_or_else(|| user.user_id.to_string()),
        Some(tl::enums::Peer::Channel(channel)) => format!("#{}", channel.channel_id),
        Some(tl::enums::Peer::Chat(chat)) => format!("#{}", chat.chat_id),
        None => "unknown".into(),
    };
    Some(CommentMessage {
        chat_id,
        message_id: message.id,
        date_unix: i64::from(message.date),
        sender,
        text,
        media: message.media.and_then(Media::from_raw),
        links,
    })
}

/// 查频道的关联讨论组。不加入该群。
pub async fn fetch_linked_discussion(
    client: &Client,
    channel: PeerRef,
) -> Result<Option<LinkedDiscussion>, AppError> {
    let full = client
        .invoke(&tl::functions::channels::GetFullChannel {
            channel: channel.into(),
        })
        .await
        .map_err(|err| AppError::Telegram(err.to_string()))?;
    let tl::enums::messages::ChatFull::Full(full) = full;
    let linked_bare = match full.full_chat {
        tl::enums::ChatFull::ChannelFull(ch) => ch.linked_chat_id,
        _ => None,
    };
    let Some(bare) = linked_bare.filter(|id| *id > 0) else {
        return Ok(None);
    };
    let Some(peer_id) = PeerId::channel(bare) else {
        return Ok(None);
    };
    let group_id = peer_id.to_string();
    for chat in full.chats {
        let peer = Peer::from_raw(client, chat);
        if peer.id().to_string() != group_id {
            continue;
        }
        let title = peer
            .name()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("#{group_id}"));
        let joined = !peer_left(&peer);
        let peer_ref = peer.to_ref().await.ok().flatten();
        return Ok(Some(LinkedDiscussion {
            group_id,
            title,
            joined,
            peer: peer_ref,
        }));
    }
    Ok(Some(LinkedDiscussion {
        group_id,
        title: format!("#{peer_id}"),
        joined: false,
        peer: None,
    }))
}

/// 解析出的未加入公开群/频道。
pub struct PublicChatRef {
    pub chat_id: String,
    pub kind: ChatKind,
    pub title: String,
    pub username: Option<String>,
    pub peer: PeerRef,
}

const TELEGRAM_LINK_PREFIXES: &[&str] = &[
    "https://t.me/",
    "http://t.me/",
    "t.me/",
    "https://www.t.me/",
    "http://www.t.me/",
    "www.t.me/",
    "https://telegram.me/",
    "http://telegram.me/",
    "telegram.me/",
    "https://www.telegram.me/",
    "http://www.telegram.me/",
    "www.telegram.me/",
    "https://telegram.dog/",
    "http://telegram.dog/",
    "telegram.dog/",
];

/// 从 `@用户名` 或 `https://t.me/xxx` 取出公开用户名。邀请链接和 `t.me/c/` 不支持。
pub fn parse_public_username(input: &str) -> Result<String, AppError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(AppError::Config("请填写公开用户名或 t.me 链接".into()));
    }
    if let Some(name) = raw.strip_prefix('@') {
        return validate_public_username(name);
    }

    let lower = raw.to_ascii_lowercase();
    let path = TELEGRAM_LINK_PREFIXES
        .iter()
        .find_map(|prefix| lower.starts_with(prefix).then(|| &raw[prefix.len()..]));
    let Some(path) = path else {
        if raw.contains(['/', ':', '?', '#']) {
            return Err(AppError::Config("只支持公开用户名或 t.me 链接".into()));
        }
        return validate_public_username(raw);
    };

    let path = path.trim_start_matches('/');
    let mut segs = path.split(['/', '?', '#']).filter(|s| !s.is_empty());
    let first = segs.next().unwrap_or("");
    if first.starts_with('+') || first.eq_ignore_ascii_case("joinchat") {
        return Err(AppError::Config(
            "邀请链接需要加入，不支持未入群预览".into(),
        ));
    }
    if first.eq_ignore_ascii_case("c") {
        return Err(AppError::Config(
            "t.me/c/ 私密链接需要加入，不支持未入群预览".into(),
        ));
    }
    if is_reserved_telegram_path(first) {
        return Err(AppError::Config("这不是公开群组或频道链接".into()));
    }
    let username = if first.eq_ignore_ascii_case("s") {
        segs.next().unwrap_or("")
    } else {
        first
    };
    validate_public_username(username)
}

fn is_reserved_telegram_path(first: &str) -> bool {
    matches!(
        first.to_ascii_lowercase().as_str(),
        "addstickers"
            | "addemoji"
            | "proxy"
            | "socks"
            | "share"
            | "boost"
            | "invoice"
            | "giftcode"
            | "login"
            | "setlanguage"
            | "confirmphone"
            | "iv"
            | "embed"
            | "k"
            | "a"
    )
}

fn validate_public_username(name: &str) -> Result<String, AppError> {
    let name = name.trim().trim_start_matches('@');
    if name.len() < 4 || name.len() > 32 {
        return Err(AppError::Config("公开用户名无效".into()));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(AppError::Config("公开用户名无效".into()));
    };
    if !first.is_ascii_alphabetic() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(AppError::Config("公开用户名无效".into()));
    }
    Ok(name.to_string())
}

fn guest_rpc_error(err: &InvocationError) -> AppError {
    if err.is("CHANNEL_PRIVATE") || err.is("CHAT_PRIVATE") || err.is("CHANNEL_INVALID") {
        AppError::Telegram("该公开群/频道需要加入才能读消息，Telegram 不允许未入群预览".into())
    } else if err.is("USERNAME_INVALID") || err.is("USERNAME_NOT_OCCUPIED") {
        AppError::Telegram("找不到该公开链接或用户名".into())
    } else {
        AppError::Telegram(err.to_string())
    }
}

/// `resolveUsername`，不 JoinChannel。
pub async fn resolve_public_chat(client: &Client, input: &str) -> Result<PublicChatRef, AppError> {
    let username = parse_public_username(input)?;
    let peer = match client.resolve_username(&username).await {
        Ok(peer) => peer,
        Err(err) => return Err(guest_rpc_error(&err)),
    };
    let Some(peer) = peer else {
        return Err(AppError::Telegram("找不到该公开链接或用户名".into()));
    };
    if matches!(peer, Peer::User(_)) {
        return Err(AppError::Telegram(
            "这是用户账号，请填写公开群组或频道".into(),
        ));
    }
    let chat_id = peer.id().to_string();
    let kind = match &peer {
        Peer::Group(_) => ChatKind::Group,
        Peer::Channel(_) => ChatKind::Channel,
        Peer::User(_) => ChatKind::Bot,
    };
    let title = peer
        .name()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("#{chat_id}"));
    let username = peer.username().map(str::to_string);
    let peer_ref = peer
        .to_ref()
        .await
        .ok()
        .flatten()
        .ok_or_else(|| AppError::Telegram("无法访问该会话（可能需要加入）".into()))?;
    Ok(PublicChatRef {
        chat_id,
        kind,
        title,
        username,
        peer: peer_ref,
    })
}

pub async fn probe_public_history(client: &Client, peer: PeerRef) -> Result<(), AppError> {
    let mut iter = client.iter_messages(peer).limit(1);
    match iter.next().await {
        Ok(_) => Ok(()),
        Err(err) => Err(guest_rpc_error(&err)),
    }
}

pub async fn find_channel_ref(client: &Client, chat_id: &str) -> Result<Option<PeerRef>, AppError> {
    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs
        .next()
        .await
        .map_err(|err| AppError::Telegram(err.to_string()))?
    {
        if let Peer::Channel(channel) = dialog.peer() {
            if channel.id().to_string() == chat_id {
                return Ok(channel.to_ref().await.ok().flatten());
            }
        }
    }
    Ok(None)
}

fn chat_list_cache_hit(refresh: bool, age: Option<Duration>, ttl: Duration) -> bool {
    !refresh && age.is_some_and(|age| age < ttl)
}

#[cfg(test)]
mod tests {
    use super::{
        chat_list_cache_hit, comment_count_from_raw, normalize_env_value, parse_public_username,
        TelegramApi, CHAT_LIST_TTL,
    };
    use grammers_client::tl;
    use std::time::Duration;

    #[test]
    fn normalize_strips_quotes_bom_and_space() {
        assert_eq!(normalize_env_value("34932700"), "34932700");
        assert_eq!(normalize_env_value("  34932700  "), "34932700");
        assert_eq!(normalize_env_value("\"34932700\""), "34932700");
        assert_eq!(normalize_env_value("'34932700'"), "34932700");
        assert_eq!(normalize_env_value("\u{feff}34932700"), "34932700");
        assert_eq!(normalize_env_value(""), "");
        assert_eq!(normalize_env_value("   "), "");
        assert_eq!(normalize_env_value("\"\""), "");
    }

    #[test]
    fn from_env_skips_empty_and_uses_wizard_fields() {
        std::env::set_var("TELEGRAM_API_ID", "");
        std::env::set_var("TELEGRAM_API_HASH", "");
        std::env::set_var("wizard_telegram_api_id", "  \"123456\"  ");
        std::env::set_var(
            "wizard_telegram_api_hash",
            "abcdefabcdefabcdefabcdefabcdefab",
        );
        let api = TelegramApi::from_env().expect("wizard fallback");
        assert_eq!(api.api_id, 123456);
        std::env::remove_var("TELEGRAM_API_ID");
        std::env::remove_var("TELEGRAM_API_HASH");
        std::env::remove_var("wizard_telegram_api_id");
        std::env::remove_var("wizard_telegram_api_hash");
    }

    #[test]
    fn chat_list_cache_lasts_one_hour() {
        assert_eq!(CHAT_LIST_TTL, Duration::from_secs(60 * 60));
        assert!(chat_list_cache_hit(
            false,
            Some(Duration::from_secs(1)),
            CHAT_LIST_TTL
        ));
        assert!(chat_list_cache_hit(
            false,
            Some(CHAT_LIST_TTL - Duration::from_secs(1)),
            CHAT_LIST_TTL
        ));
        assert!(!chat_list_cache_hit(
            false,
            Some(CHAT_LIST_TTL),
            CHAT_LIST_TTL
        ));
        assert!(!chat_list_cache_hit(
            true,
            Some(Duration::ZERO),
            CHAT_LIST_TTL
        ));
        assert!(!chat_list_cache_hit(false, None, CHAT_LIST_TTL));
    }

    #[test]
    fn only_channel_comment_threads_count() {
        let comments = tl::enums::MessageReplies::Replies(tl::types::MessageReplies {
            comments: true,
            replies: 4,
            replies_pts: 1,
            recent_repliers: None,
            channel_id: Some(99),
            max_id: None,
            read_max_id: None,
        });
        let group_thread = tl::enums::MessageReplies::Replies(tl::types::MessageReplies {
            comments: false,
            replies: 8,
            replies_pts: 1,
            recent_repliers: None,
            channel_id: None,
            max_id: None,
            read_max_id: None,
        });
        let with_comments = dummy_message(Some(comments));
        let with_thread = dummy_message(Some(group_thread));
        let none = dummy_message(None);
        assert_eq!(comment_count_from_raw(&with_comments), Some(4));
        assert_eq!(comment_count_from_raw(&with_thread), None);
        assert_eq!(comment_count_from_raw(&none), None);
        assert_eq!(
            comment_count_from_raw(&tl::enums::Message::Empty(tl::types::MessageEmpty {
                id: 1,
                peer_id: None,
            })),
            None
        );
    }

    #[test]
    fn parse_public_username_accepts_links_and_at() {
        assert_eq!(
            parse_public_username("https://t.me/urxa3").unwrap(),
            "urxa3"
        );
        assert_eq!(
            parse_public_username("http://t.me/urxa3/123").unwrap(),
            "urxa3"
        );
        assert_eq!(parse_public_username("t.me/urxa3").unwrap(), "urxa3");
        assert_eq!(parse_public_username("@urxa3").unwrap(), "urxa3");
        assert_eq!(parse_public_username("urxa3").unwrap(), "urxa3");
        assert_eq!(
            parse_public_username("https://telegram.me/s/urxa3").unwrap(),
            "urxa3"
        );
    }

    #[test]
    fn parse_public_username_rejects_invite_and_private() {
        assert!(parse_public_username("").is_err());
        assert!(parse_public_username("https://t.me/+abc").is_err());
        assert!(parse_public_username("https://t.me/joinchat/abc").is_err());
        assert!(parse_public_username("https://t.me/c/123456/1").is_err());
        assert!(parse_public_username("https://t.me/addstickers/foo").is_err());
        assert!(parse_public_username("ab").is_err());
    }

    fn dummy_message(replies: Option<tl::enums::MessageReplies>) -> tl::enums::Message {
        tl::enums::Message::Message(tl::types::Message {
            out: false,
            mentioned: false,
            media_unread: false,
            silent: false,
            post: true,
            from_scheduled: false,
            legacy: false,
            edit_hide: false,
            pinned: false,
            noforwards: false,
            invert_media: false,
            offline: false,
            video_processing_pending: false,
            paid_suggested_post_stars: false,
            paid_suggested_post_ton: false,
            id: 1,
            from_id: None,
            from_boosts_applied: None,
            from_rank: None,
            peer_id: tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 1 }),
            saved_peer_id: None,
            fwd_from: None,
            via_bot_id: None,
            via_business_bot_id: None,
            guestchat_via_from: None,
            reply_to: None,
            date: 0,
            message: String::new(),
            media: None,
            reply_markup: None,
            entities: None,
            views: None,
            forwards: None,
            replies,
            edit_date: None,
            post_author: None,
            grouped_id: None,
            reactions: None,
            restriction_reason: None,
            ttl_period: None,
            quick_reply_shortcut_id: None,
            effect: None,
            factcheck: None,
            report_delivery_until_date: None,
            paid_message_stars: None,
            suggested_post: None,
            schedule_repeat_period: None,
            summary_from_language: None,
            rich_message: None,
        })
    }
}
