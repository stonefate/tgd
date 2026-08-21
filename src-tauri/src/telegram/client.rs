use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::peer::Peer;
use grammers_client::sender::ConnectionParams;
use grammers_client::session::updates::UpdatesLike;
use grammers_client::{Client, SenderPool, SignInError};
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
use crate::telegram::session::SessionPaths;

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

    pub async fn list_group_channels(&self) -> Result<Vec<ChatItem>, AppError> {
        let client = self
            .client
            .clone()
            .ok_or_else(|| AppError::Telegram("尚未连接 Telegram".into()))?;
        if !self.authorized {
            return Err(AppError::Telegram("尚未登录 Telegram".into()));
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
                    chats.push(ChatItem {
                        id,
                        kind: ChatKind::Group,
                        title,
                        username: group.username().map(str::to_string),
                        watched: false,
                        types: ChatDownloadTypes::default(),
                        backfill_days: 0,
                        backfill_days_override: None,
                        alias: None,
                    });
                }
                Peer::Channel(channel) => {
                    let id = channel.id().to_string();
                    let raw_title = channel.title().trim();
                    let title = if raw_title.is_empty() {
                        format!("#{id}")
                    } else {
                        raw_title.to_string()
                    };
                    chats.push(ChatItem {
                        id,
                        kind: ChatKind::Channel,
                        title,
                        username: channel.username().map(str::to_string),
                        watched: false,
                        types: ChatDownloadTypes::default(),
                        backfill_days: 0,
                        backfill_days_override: None,
                        alias: None,
                    });
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
                    chats.push(ChatItem {
                        id,
                        kind: ChatKind::Bot,
                        title,
                        username: user.username().map(str::to_string),
                        watched: false,
                        types: ChatDownloadTypes::default(),
                        backfill_days: 0,
                        backfill_days_override: None,
                        alias: None,
                    });
                }
            }
        }

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

#[derive(Debug, Clone, Copy, serde::Serialize, specta::Type)]
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
}

#[cfg(test)]
mod tests {
    use super::{normalize_env_value, TelegramApi};

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
}
