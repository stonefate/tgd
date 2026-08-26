use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::error::AppError;
use crate::telegram::MediaKind;

pub const DOWNLOAD_CONCURRENCY_DEFAULT: u32 = 2;
pub const DOWNLOAD_CONCURRENCY_MIN: u32 = 1;
pub const DOWNLOAD_CONCURRENCY_MAX: u32 = 8;

fn default_download_concurrency() -> u32 {
    DOWNLOAD_CONCURRENCY_DEFAULT
}

pub fn clamp_download_concurrency(n: u32) -> u32 {
    n.clamp(DOWNLOAD_CONCURRENCY_MIN, DOWNLOAD_CONCURRENCY_MAX)
}

/// 跳过小于该体积（MB）的媒体。0 = 不过滤，上限 4096。
pub fn clamp_min_media_mb(n: f64) -> f64 {
    if !n.is_finite() || n <= 0.0 {
        0.0
    } else {
        n.min(4096.0)
    }
}

/// 单个群组 / 频道要下载的类型。缺省全关。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ChatDownloadTypes {
    #[serde(default)]
    pub video: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub photo: bool,
    #[serde(default)]
    pub document: bool,
    #[serde(default)]
    pub text: bool,
}

impl ChatDownloadTypes {
    pub fn any(self) -> bool {
        self.video || self.audio || self.photo || self.document || self.text
    }

    pub fn any_media(self) -> bool {
        self.video || self.audio || self.photo || self.document
    }

    pub fn media_kind_names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.photo {
            names.push("photo");
        }
        if self.video {
            names.push("video");
        }
        if self.audio {
            names.push("audio");
        }
        if self.document {
            names.push("document");
        }
        names
    }

    pub fn allows_media(self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Video => self.video,
            MediaKind::Audio => self.audio,
            MediaKind::Photo => self.photo,
            MediaKind::Document => self.document,
        }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            video: self.video || other.video,
            audio: self.audio || other.audio,
            photo: self.photo || other.photo,
            document: self.document || other.document,
            text: self.text || other.text,
        }
    }

    /// `self` 里开了、`applied` 里还没扫过的类型。
    pub fn newly_enabled(self, applied: Self) -> Self {
        Self {
            video: self.video && !applied.video,
            audio: self.audio && !applied.audio,
            photo: self.photo && !applied.photo,
            document: self.document && !applied.document,
            text: self.text && !applied.text,
        }
    }
}

/// 设置页展示 / 保存的 SOCKS5 代理。密码只在提交时出现，读回用 `hasPassword`。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    pub enabled: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    #[serde(default)]
    pub has_password: bool,
}

fn encode_userinfo(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn decode_userinfo(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &value[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_socks5_url(
    raw: &str,
) -> Result<(String, u16, Option<String>, Option<String>), AppError> {
    let rest = raw
        .strip_prefix("socks5://")
        .ok_or_else(|| AppError::Config("代理只支持 socks5://".into()))?;
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, rest),
    };
    let (host, port_raw) = hostport
        .rsplit_once(':')
        .ok_or_else(|| AppError::Config("代理地址格式应为 host:port".into()))?;
    if host.is_empty() {
        return Err(AppError::Config("代理主机不能为空".into()));
    }
    let port: u16 = port_raw
        .parse()
        .map_err(|_| AppError::Config("代理端口无效".into()))?;
    if port == 0 {
        return Err(AppError::Config("代理端口无效".into()));
    }
    let (username, password) = match userinfo {
        None | Some("") => (None, None),
        Some(userinfo) => match userinfo.split_once(':') {
            Some((user, pass)) => (
                Some(decode_userinfo(user)).filter(|s| !s.is_empty()),
                Some(decode_userinfo(pass)).filter(|s| !s.is_empty()),
            ),
            None => (
                Some(decode_userinfo(userinfo)).filter(|s| !s.is_empty()),
                None,
            ),
        },
    };
    Ok((host.to_string(), port, username, password))
}

pub fn build_socks5_url(
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<String, AppError> {
    let host = host.trim();
    if host.is_empty() {
        return Err(AppError::Config("代理主机不能为空".into()));
    }
    if host.contains('/') || host.contains('@') {
        return Err(AppError::Config("代理主机无效".into()));
    }
    if port == 0 {
        return Err(AppError::Config("代理端口无效".into()));
    }
    let mut url = String::from("socks5://");
    if let Some(user) = username.map(str::trim).filter(|s| !s.is_empty()) {
        url.push_str(&encode_userinfo(user));
        if let Some(pass) = password.map(str::trim).filter(|s| !s.is_empty()) {
            url.push(':');
            url.push_str(&encode_userinfo(pass));
        }
        url.push('@');
    }
    url.push_str(host);
    url.push(':');
    url.push_str(&port.to_string());
    Ok(url)
}

/// 应用本地设置。落在 `app_data_dir()/settings.json`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    /// 加入监听下载名单的群组 / 频道 id。后续媒体下载只认这份名单。
    #[serde(default)]
    pub watched_chat_ids: Vec<String>,
    /// 全局回爬天数。0 = 只收新消息；`< 0` = 全量回爬。
    #[serde(default)]
    pub backfill_days: i32,
    /// 每群要下载的类型。未写入的群视为全关。
    #[serde(default)]
    pub chat_download_types: HashMap<String, ChatDownloadTypes>,
    /// 每群回爬天数覆盖。没有这个 key 就用全局 `backfill_days`。
    #[serde(default)]
    pub chat_backfill_days: HashMap<String, i32>,
    /// 消息列表是否显示已下载媒体。缺省关。
    #[serde(default)]
    pub show_media: bool,
    /// 群组 / 频道显示别名。没有这个 key 就用 Telegram 原名。
    #[serde(default)]
    pub chat_aliases: HashMap<String, String>,
    /// 回爬时同时下载的媒体数。缺省 2，写入时夹到 1–8。
    #[serde(default = "default_download_concurrency")]
    pub download_concurrency: u32,
    /// 跳过小于该体积（MB）的媒体。缺省 0 = 不过滤。
    #[serde(default)]
    pub min_media_mb: f64,
    /// 开机自启。缺省关。启动时按这项同步系统登录项。
    #[serde(default)]
    pub autostart: bool,
    /// 暂停全部下载。缺省关。重启后按这项恢复。
    #[serde(default)]
    pub download_paused: bool,
    /// SOCKS5 代理 URL，例如 `socks5://127.0.0.1:7891`。空 = 直连。
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// 频道 → 关联讨论组。空字符串表示已查过、没有评论组。
    #[serde(default)]
    pub channel_discussion: HashMap<String, String>,
    /// 因监听频道而自动勾上的讨论组。关掉频道时一起关；用户手动关则记入 skipped，不再自动勾。
    #[serde(default)]
    pub auto_comment_chats: Vec<String>,
    /// 用户在评论组上关掉「监听下载」。频道仍监听时不要再自动勾上。
    #[serde(default)]
    pub auto_comment_skipped: Vec<String>,
    /// 未加入的公开群/频道预览。与已加入的多选监听并存，全局最多一个。
    #[serde(default)]
    pub guest_watch_enabled: bool,
    /// 用户填写的 @用户名或 t.me 链接。
    #[serde(default)]
    pub guest_watch_query: String,
    #[serde(default)]
    pub guest_watch_chat_id: String,
    #[serde(default)]
    pub guest_watch_title: String,
    #[serde(default)]
    pub guest_watch_username: String,
    /// `group` 或 `channel`。
    #[serde(default)]
    pub guest_watch_kind: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            download_dir: None,
            watched_chat_ids: Vec::new(),
            backfill_days: 0,
            chat_download_types: HashMap::new(),
            chat_backfill_days: HashMap::new(),
            show_media: false,
            chat_aliases: HashMap::new(),
            download_concurrency: DOWNLOAD_CONCURRENCY_DEFAULT,
            min_media_mb: 0.0,
            autostart: false,
            download_paused: false,
            proxy_url: None,
            channel_discussion: HashMap::new(),
            auto_comment_chats: Vec::new(),
            auto_comment_skipped: Vec::new(),
            guest_watch_enabled: false,
            guest_watch_query: String::new(),
            guest_watch_chat_id: String::new(),
            guest_watch_title: String::new(),
            guest_watch_username: String::new(),
            guest_watch_kind: String::new(),
        }
    }
}

impl AppSettings {
    pub fn file_path(root: &Path) -> PathBuf {
        root.join("settings.json")
    }

    pub fn load(root: &Path) -> Self {
        let path = Self::file_path(root);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&raw) {
            Ok(settings) => settings,
            Err(err) => {
                log::warn!("failed to parse settings.json: {err}");
                Self::default()
            }
        }
    }

    pub fn save(&self, root: &Path) -> Result<(), AppError> {
        std::fs::create_dir_all(root).map_err(|err| AppError::Io(err.to_string()))?;
        let raw =
            serde_json::to_string_pretty(self).map_err(|err| AppError::Io(err.to_string()))?;
        std::fs::write(Self::file_path(root), raw).map_err(|err| AppError::Io(err.to_string()))
    }

    pub fn is_watched(&self, chat_id: &str) -> bool {
        self.watched_chat_ids.iter().any(|id| id == chat_id)
    }

    pub fn guest_watch_status(&self) -> GuestWatchStatus {
        GuestWatchStatus {
            enabled: self.guest_watch_enabled,
            query: self.guest_watch_query.clone(),
            chat_id: nonempty_opt(&self.guest_watch_chat_id),
            title: nonempty_opt(&self.guest_watch_title),
            username: nonempty_opt(&self.guest_watch_username),
        }
    }

    pub fn guest_chat_id(&self) -> Option<&str> {
        let id = self.guest_watch_chat_id.trim();
        if id.is_empty() {
            None
        } else {
            Some(id)
        }
    }

    pub fn active_guest_chat_id(&self) -> Option<&str> {
        if self.guest_watch_enabled {
            self.guest_chat_id()
        } else {
            None
        }
    }

    pub fn is_guest_slot(&self, chat_id: &str) -> bool {
        self.guest_chat_id() == Some(chat_id)
    }

    pub fn guest_chat_kind(&self) -> super::telegram::ChatKind {
        if self.guest_watch_kind == "group" {
            super::telegram::ChatKind::Group
        } else {
            super::telegram::ChatKind::Channel
        }
    }

    /// 关掉未加入预览，移出监听。保留已解析的目标方便再打开。
    pub fn disable_guest_watch(&mut self) -> Option<String> {
        self.guest_watch_enabled = false;
        let id = self.guest_chat_id()?.to_string();
        let _ = self.set_chat_watched(id.clone(), false);
        Some(id)
    }

    /// 设为唯一未加入目标。若替换了旧会话，返回旧 id。
    pub fn enable_guest_watch(
        &mut self,
        query: String,
        chat_id: String,
        title: String,
        username: Option<String>,
        kind: &str,
    ) -> Option<String> {
        let chat_id = chat_id.trim().to_string();
        let old = self
            .guest_chat_id()
            .filter(|id| *id != chat_id.as_str())
            .map(str::to_string);
        if let Some(old) = &old {
            let _ = self.set_chat_watched(old.clone(), false);
        }
        self.guest_watch_enabled = true;
        self.guest_watch_query = query.trim().to_string();
        self.guest_watch_chat_id = chat_id.clone();
        self.guest_watch_title = title;
        self.guest_watch_username = username.unwrap_or_default();
        self.guest_watch_kind = kind.to_string();
        let _ = self.set_chat_watched(chat_id, true);
        old
    }

    /// 该频道的关联讨论组 id。`None` 表示未缓存或没有评论组。
    pub fn discussion_id(&self, channel_id: &str) -> Option<String> {
        self.channel_discussion
            .get(channel_id)
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    }

    pub fn comment_channel_of(&self, group_id: &str) -> Option<String> {
        self.channel_discussion
            .iter()
            .find_map(|(channel, disc)| (disc == group_id).then_some(channel.clone()))
    }

    pub fn set_channel_discussion(&mut self, channel_id: String, discussion_id: Option<String>) {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            return;
        }
        let value = discussion_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_default();
        self.channel_discussion
            .insert(channel_id.to_string(), value);
    }

    pub fn is_auto_comment(&self, chat_id: &str) -> bool {
        self.auto_comment_chats.iter().any(|id| id == chat_id)
    }

    pub fn auto_discussion_of(&self, channel_id: &str) -> Option<String> {
        let disc = self.discussion_id(channel_id)?;
        self.is_auto_comment(&disc).then_some(disc)
    }

    /// 清除频道时连带关联评论组。清群组/机器人只清自己。
    pub fn chats_to_wipe(&self, chat_id: &str) -> Vec<String> {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return Vec::new();
        }
        let mut ids = vec![chat_id.to_string()];
        if let Some(disc) = self.discussion_id(chat_id) {
            if disc != chat_id && !ids.iter().any(|id| id == &disc) {
                ids.push(disc);
            }
        }
        ids
    }

    /// 检查文件要扫的会话：自己，频道再加仍在同步的评论组。
    pub fn media_check_ids(&self, chat_id: &str) -> Result<Vec<String>, AppError> {
        let chat_id = require_chat_id(chat_id)?;
        if !self.should_sync_chat(&chat_id) {
            return Err(AppError::Config("未监听该会话".into()));
        }
        if self.effective_backfill_days(&chat_id) == 0 {
            return Err(AppError::Config("回爬天数是 0，只收新消息，不检查".into()));
        }
        if !self.chat_types(&chat_id).any_media() {
            return Err(AppError::Config("没有勾选要检查的媒体类型".into()));
        }
        Ok(self
            .chats_to_wipe(&chat_id)
            .into_iter()
            .filter(|id| {
                self.should_sync_chat(id)
                    && self.effective_backfill_days(id) != 0
                    && self.chat_types(id).any_media()
            })
            .collect())
    }

    pub fn mark_auto_comment(&mut self, chat_id: String) {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() || self.is_auto_comment(chat_id) {
            return;
        }
        self.auto_comment_chats.push(chat_id.to_string());
    }

    pub fn unmark_auto_comment(&mut self, chat_id: &str) {
        self.auto_comment_chats.retain(|id| id != chat_id);
    }

    pub fn is_auto_comment_skipped(&self, chat_id: &str) -> bool {
        self.auto_comment_skipped.iter().any(|id| id == chat_id)
    }

    pub fn skip_auto_comment(&mut self, chat_id: &str) {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() || self.is_auto_comment_skipped(chat_id) {
            return;
        }
        self.auto_comment_skipped.push(chat_id.to_string());
    }

    pub fn unskip_auto_comment(&mut self, chat_id: &str) {
        self.auto_comment_skipped.retain(|id| id != chat_id);
    }

    /// 关掉频道时：若讨论组是自动勾上的，移出自动名单并返回其 id。
    pub fn take_auto_discussion(&mut self, channel_id: &str) -> Option<String> {
        let disc = self.auto_discussion_of(channel_id)?;
        self.unmark_auto_comment(&disc);
        Some(disc)
    }

    pub fn set_chat_watched(&mut self, chat_id: String, watched: bool) -> Result<(), AppError> {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return Err(AppError::Io("会话 id 不能为空".into()));
        }

        if watched {
            if !self.is_watched(chat_id) {
                self.watched_chat_ids.push(chat_id.to_string());
            }
        } else {
            self.watched_chat_ids.retain(|id| id != chat_id);
        }
        Ok(())
    }

    pub fn chat_types(&self, chat_id: &str) -> ChatDownloadTypes {
        self.chat_download_types
            .get(chat_id)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_chat_types(
        &mut self,
        chat_id: String,
        types: ChatDownloadTypes,
    ) -> Result<ChatDownloadTypes, AppError> {
        let chat_id = require_chat_id(&chat_id)?;
        self.chat_download_types.insert(chat_id, types);
        Ok(types)
    }

    pub fn set_backfill_days(&mut self, days: i32) {
        self.backfill_days = days;
    }

    pub fn effective_download_concurrency(&self) -> u32 {
        clamp_download_concurrency(self.download_concurrency)
    }

    pub fn set_download_concurrency(&mut self, n: u32) -> u32 {
        self.download_concurrency = clamp_download_concurrency(n);
        self.download_concurrency
    }

    pub fn effective_min_media_mb(&self) -> f64 {
        clamp_min_media_mb(self.min_media_mb)
    }

    pub fn set_min_media_mb(&mut self, n: f64) -> f64 {
        self.min_media_mb = clamp_min_media_mb(n);
        self.min_media_mb
    }

    pub fn effective_proxy_url(&self) -> Option<String> {
        self.proxy_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    pub fn proxy_config(&self) -> ProxyConfig {
        let Some(raw) = self.effective_proxy_url() else {
            return ProxyConfig::default();
        };
        match parse_socks5_url(&raw) {
            Ok((host, port, username, password)) => ProxyConfig {
                enabled: true,
                host,
                port,
                username,
                password: None,
                has_password: password.is_some(),
            },
            Err(_) => ProxyConfig::default(),
        }
    }

    pub fn set_proxy(&mut self, input: ProxyConfig) -> Result<ProxyConfig, AppError> {
        if !input.enabled {
            self.proxy_url = None;
            return Ok(ProxyConfig::default());
        }
        let existing = self
            .effective_proxy_url()
            .and_then(|raw| parse_socks5_url(&raw).ok());
        let username = input
            .username
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let password = match input.password.as_deref().map(str::trim) {
            Some("") => None,
            Some(pass) => Some(pass.to_string()),
            None => existing.as_ref().and_then(|(_, _, _, pass)| pass.clone()),
        };
        let url = build_socks5_url(input.host.trim(), input.port, username, password.as_deref())?;
        self.proxy_url = Some(url);
        Ok(self.proxy_config())
    }

    pub fn chat_backfill_override(&self, chat_id: &str) -> Option<i32> {
        self.chat_backfill_days.get(chat_id).copied()
    }

    /// 该群有效回爬天数：有覆盖用覆盖，否则用全局。
    /// `0` 只收新，`< 0` 全量。
    pub fn effective_backfill_days(&self, chat_id: &str) -> i32 {
        self.chat_backfill_override(chat_id)
            .unwrap_or(self.backfill_days)
    }

    pub fn set_chat_backfill_days(
        &mut self,
        chat_id: String,
        days: Option<i32>,
    ) -> Result<Option<i32>, AppError> {
        let chat_id = require_chat_id(&chat_id)?;
        match days {
            Some(days) => {
                self.chat_backfill_days.insert(chat_id, days);
                Ok(Some(days))
            }
            None => {
                self.chat_backfill_days.remove(&chat_id);
                Ok(None)
            }
        }
    }

    pub fn should_sync_chat(&self, chat_id: &str) -> bool {
        self.is_watched(chat_id) && self.chat_types(chat_id).any()
    }

    pub fn chat_alias(&self, chat_id: &str) -> Option<String> {
        self.chat_aliases
            .get(chat_id)
            .map(|alias| alias.trim())
            .filter(|alias| !alias.is_empty())
            .map(str::to_string)
    }

    /// 显示名：有别名用别名，否则用传入的 Telegram 原名。
    pub fn display_title(&self, chat_id: &str, title: &str) -> String {
        self.chat_alias(chat_id)
            .unwrap_or_else(|| title.to_string())
    }

    pub fn set_chat_alias(
        &mut self,
        chat_id: String,
        alias: Option<String>,
    ) -> Result<Option<String>, AppError> {
        let chat_id = require_chat_id(&chat_id)?;
        match alias {
            Some(alias) if !alias.trim().is_empty() => {
                let alias = alias.trim().to_string();
                self.chat_aliases.insert(chat_id, alias.clone());
                Ok(Some(alias))
            }
            _ => {
                self.chat_aliases.remove(&chat_id);
                Ok(None)
            }
        }
    }
}

fn nonempty_opt(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 未加入公开群/频道预览。全局最多一个，与已加入监听并存。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct GuestWatchStatus {
    pub enabled: bool,
    pub query: String,
    pub chat_id: Option<String>,
    pub title: Option<String>,
    pub username: Option<String>,
}

fn require_chat_id(chat_id: &str) -> Result<String, AppError> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return Err(AppError::Io("会话 id 不能为空".into()));
    }
    Ok(chat_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tgd-settings-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn save_and_load_download_dir() {
        let root = temp_root("save");
        std::fs::create_dir_all(&root).unwrap();

        let mut types = HashMap::new();
        types.insert(
            "1".into(),
            ChatDownloadTypes {
                video: true,
                audio: true,
                ..ChatDownloadTypes::default()
            },
        );
        let mut overrides = HashMap::new();
        overrides.insert("1".into(), 30);

        let settings = AppSettings {
            download_dir: Some(root.join("media")),
            watched_chat_ids: vec!["1".into()],
            backfill_days: 7,
            chat_download_types: types,
            chat_backfill_days: overrides,
            show_media: true,
            chat_aliases: HashMap::new(),
            download_concurrency: 3,
            min_media_mb: 2.5,
            channel_discussion: HashMap::new(),
            auto_comment_chats: Vec::new(),
            auto_comment_skipped: Vec::new(),
            autostart: true,
            download_paused: true,
            proxy_url: Some("socks5://127.0.0.1:7891".into()),
            ..Default::default()
        };
        settings.save(&root).unwrap();

        let loaded = AppSettings::load(&root);
        assert_eq!(loaded.download_dir, settings.download_dir);
        assert_eq!(loaded.watched_chat_ids, settings.watched_chat_ids);
        assert_eq!(loaded.backfill_days, 7);
        assert!(loaded.chat_types("1").video);
        assert!(loaded.chat_types("1").audio);
        assert!(!loaded.chat_types("1").photo);
        assert_eq!(loaded.effective_backfill_days("1"), 30);
        assert_eq!(loaded.effective_backfill_days("2"), 7);
        assert!(loaded.show_media);
        assert_eq!(loaded.download_concurrency, 3);
        assert_eq!(loaded.min_media_mb, 2.5);
        assert!(loaded.autostart);
        assert!(loaded.download_paused);
        assert_eq!(loaded.proxy_url.as_deref(), Some("socks5://127.0.0.1:7891"));
        assert!(loaded.proxy_config().enabled);
        assert_eq!(loaded.proxy_config().host, "127.0.0.1");
        assert_eq!(loaded.proxy_config().port, 7891);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_file_is_default() {
        let root = temp_root("missing");
        let loaded = AppSettings::load(&root);
        assert!(loaded.download_dir.is_none());
        assert!(loaded.watched_chat_ids.is_empty());
        assert_eq!(loaded.backfill_days, 0);
        assert!(!loaded.chat_types("1").any());
        assert_eq!(loaded.effective_backfill_days("1"), 0);
        assert!(!loaded.show_media);
        assert!(loaded.chat_aliases.is_empty());
        assert_eq!(
            loaded.effective_download_concurrency(),
            DOWNLOAD_CONCURRENCY_DEFAULT
        );
        assert_eq!(loaded.min_media_mb, 0.0);
        assert!(!loaded.autostart);
        assert!(!loaded.download_paused);
        assert!(loaded.proxy_url.is_none());
        assert!(!loaded.proxy_config().enabled);
        assert!(!loaded.guest_watch_enabled);
        assert!(loaded.guest_chat_id().is_none());
    }

    #[test]
    fn guest_watch_replaces_previous_slot() {
        let mut settings = AppSettings::default();
        assert!(settings
            .enable_guest_watch(
                "https://t.me/one".into(),
                "11".into(),
                "One".into(),
                Some("one".into()),
                "channel",
            )
            .is_none());
        assert!(settings.guest_watch_enabled);
        assert_eq!(settings.active_guest_chat_id(), Some("11"));
        assert!(settings.is_watched("11"));

        let old = settings.enable_guest_watch(
            "@two".into(),
            "22".into(),
            "Two".into(),
            Some("two".into()),
            "group",
        );
        assert_eq!(old.as_deref(), Some("11"));
        assert!(!settings.is_watched("11"));
        assert!(settings.is_watched("22"));
        assert_eq!(settings.active_guest_chat_id(), Some("22"));
        assert!(settings.is_guest_slot("22"));
        assert!(!settings.is_guest_slot("11"));

        let dropped = settings.disable_guest_watch();
        assert_eq!(dropped.as_deref(), Some("22"));
        assert!(!settings.guest_watch_enabled);
        assert!(settings.active_guest_chat_id().is_none());
        assert!(!settings.is_watched("22"));
        assert_eq!(settings.guest_chat_id(), Some("22"));
    }

    #[test]
    fn socks5_url_roundtrip_and_keep_password() {
        let url = build_socks5_url("192.168.5.2", 7891, Some("u"), Some("p@ss")).unwrap();
        assert_eq!(url, "socks5://u:p%40ss@192.168.5.2:7891");
        let (host, port, user, pass) = parse_socks5_url(&url).unwrap();
        assert_eq!(host, "192.168.5.2");
        assert_eq!(port, 7891);
        assert_eq!(user.as_deref(), Some("u"));
        assert_eq!(pass.as_deref(), Some("p@ss"));

        let mut settings = AppSettings::default();
        settings
            .set_proxy(ProxyConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port: 1080,
                username: Some("u".into()),
                password: Some("secret".into()),
                has_password: false,
            })
            .unwrap();
        let shown = settings.proxy_config();
        assert!(shown.enabled);
        assert_eq!(shown.host, "127.0.0.1");
        assert_eq!(shown.port, 1080);
        assert_eq!(shown.username.as_deref(), Some("u"));
        assert!(shown.has_password);
        assert!(shown.password.is_none());

        settings
            .set_proxy(ProxyConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port: 1080,
                username: Some("u".into()),
                password: None,
                has_password: true,
            })
            .unwrap();
        assert!(settings.effective_proxy_url().unwrap().contains("secret"));

        settings
            .set_proxy(ProxyConfig {
                enabled: false,
                host: "127.0.0.1".into(),
                port: 1080,
                username: None,
                password: None,
                has_password: false,
            })
            .unwrap();
        assert!(settings.proxy_url.is_none());
    }

    #[test]
    fn download_concurrency_default_and_clamp() {
        let mut settings = AppSettings::default();
        assert_eq!(
            settings.effective_download_concurrency(),
            DOWNLOAD_CONCURRENCY_DEFAULT
        );
        assert_eq!(settings.set_download_concurrency(0), 1);
        assert_eq!(settings.set_download_concurrency(99), 8);
        assert_eq!(settings.set_download_concurrency(4), 4);
        settings.download_concurrency = 0;
        assert_eq!(settings.effective_download_concurrency(), 1);
    }

    #[test]
    fn min_media_mb_default_and_clamp() {
        let mut settings = AppSettings::default();
        assert_eq!(settings.effective_min_media_mb(), 0.0);
        assert_eq!(settings.set_min_media_mb(-1.0), 0.0);
        assert_eq!(settings.set_min_media_mb(f64::NAN), 0.0);
        assert_eq!(settings.set_min_media_mb(0.5), 0.5);
        assert_eq!(settings.set_min_media_mb(9000.0), 4096.0);
    }

    #[test]
    fn chat_alias_set_and_clear() {
        let mut settings = AppSettings::default();
        assert!(settings.chat_alias("1").is_none());
        assert_eq!(settings.display_title("1", "原名"), "原名");

        assert_eq!(
            settings
                .set_chat_alias("1".into(), Some(" 工作群 ".into()))
                .unwrap(),
            Some("工作群".into())
        );
        assert_eq!(settings.chat_alias("1").as_deref(), Some("工作群"));
        assert_eq!(settings.display_title("1", "原名"), "工作群");

        assert!(settings
            .set_chat_alias("1".into(), Some("   ".into()))
            .unwrap()
            .is_none());
        assert!(settings.chat_alias("1").is_none());
        assert!(settings
            .set_chat_alias("".into(), Some("x".into()))
            .is_err());
    }

    #[test]
    fn set_chat_watched_add_and_remove() {
        let mut settings = AppSettings::default();
        settings.set_chat_watched(" 42 ".into(), true).unwrap();
        settings.set_chat_watched("42".into(), true).unwrap();
        assert_eq!(settings.watched_chat_ids, vec!["42"]);
        assert!(settings.is_watched("42"));

        settings.set_chat_watched("42".into(), false).unwrap();
        assert!(settings.watched_chat_ids.is_empty());
        assert!(settings.set_chat_watched("".into(), true).is_err());
    }

    #[test]
    fn channel_discussion_cache_and_auto_comment() {
        let mut settings = AppSettings::default();
        settings.set_channel_discussion("ch".into(), Some("  disc  ".into()));
        assert_eq!(settings.discussion_id("ch").as_deref(), Some("disc"));
        assert_eq!(settings.comment_channel_of("disc").as_deref(), Some("ch"));
        assert!(settings.auto_discussion_of("ch").is_none());
        assert_eq!(
            settings.chats_to_wipe("ch"),
            vec!["ch".to_string(), "disc".to_string()]
        );
        assert_eq!(settings.chats_to_wipe("disc"), vec!["disc".to_string()]);
        assert!(settings.chats_to_wipe("").is_empty());

        settings.set_chat_watched("disc".into(), true).unwrap();
        settings.mark_auto_comment("disc".into());
        assert_eq!(settings.auto_discussion_of("ch").as_deref(), Some("disc"));

        settings.skip_auto_comment("disc");
        assert!(settings.is_auto_comment_skipped("disc"));
        settings.unskip_auto_comment("disc");
        assert!(!settings.is_auto_comment_skipped("disc"));

        let taken = settings.take_auto_discussion("ch");
        assert_eq!(taken.as_deref(), Some("disc"));
        assert!(!settings.is_auto_comment("disc"));
        assert_eq!(settings.discussion_id("ch").as_deref(), Some("disc"));

        settings.set_channel_discussion("ch".into(), None);
        assert!(settings.discussion_id("ch").is_none());
        assert_eq!(
            settings.channel_discussion.get("ch").map(String::as_str),
            Some("")
        );
    }

    #[test]
    fn media_check_ids_requires_watch_window_and_media() {
        let mut settings = AppSettings::default();
        assert!(settings.media_check_ids("ch").is_err());

        settings.set_chat_watched("ch".into(), true).unwrap();
        settings.set_backfill_days(-1);
        assert!(settings.media_check_ids("ch").is_err());

        let video = ChatDownloadTypes {
            video: true,
            ..ChatDownloadTypes::default()
        };
        settings.set_chat_types("ch".into(), video).unwrap();
        assert_eq!(
            settings.media_check_ids("ch").unwrap(),
            vec!["ch".to_string()]
        );

        settings.set_channel_discussion("ch".into(), Some("disc".into()));
        settings.set_chat_watched("disc".into(), true).unwrap();
        settings.set_chat_types("disc".into(), video).unwrap();
        assert_eq!(
            settings.media_check_ids("ch").unwrap(),
            vec!["ch".to_string(), "disc".to_string()]
        );
        assert_eq!(
            settings.media_check_ids("disc").unwrap(),
            vec!["disc".to_string()]
        );

        settings.set_backfill_days(0);
        assert!(settings.media_check_ids("ch").is_err());
    }

    #[test]
    fn types_default_off_and_filter() {
        let types = ChatDownloadTypes::default();
        assert!(!types.any());
        assert!(!types.allows_media(MediaKind::Video));

        let enabled = ChatDownloadTypes {
            video: true,
            audio: true,
            ..ChatDownloadTypes::default()
        };
        assert!(enabled.any());
        assert!(enabled.allows_media(MediaKind::Video));
        assert!(enabled.allows_media(MediaKind::Audio));
        assert!(!enabled.allows_media(MediaKind::Photo));
        assert!(!enabled.allows_media(MediaKind::Document));
    }

    #[test]
    fn backfill_days_global_and_override() {
        let mut settings = AppSettings::default();
        assert_eq!(settings.effective_backfill_days("a"), 0);

        settings.set_backfill_days(7);
        assert_eq!(settings.effective_backfill_days("a"), 7);

        settings
            .set_chat_backfill_days("a".into(), Some(30))
            .unwrap();
        assert_eq!(settings.effective_backfill_days("a"), 30);
        assert_eq!(settings.effective_backfill_days("b"), 7);
        assert_eq!(settings.chat_backfill_override("a"), Some(30));

        settings
            .set_chat_backfill_days("a".into(), Some(0))
            .unwrap();
        assert_eq!(settings.effective_backfill_days("a"), 0);

        settings
            .set_chat_backfill_days("a".into(), Some(-1))
            .unwrap();
        assert_eq!(settings.effective_backfill_days("a"), -1);

        settings.set_chat_backfill_days("a".into(), None).unwrap();
        assert_eq!(settings.effective_backfill_days("a"), 7);
        assert_eq!(settings.chat_backfill_override("a"), None);
        assert!(settings.set_chat_backfill_days("".into(), Some(1)).is_err());
    }

    #[test]
    fn should_sync_requires_watch_and_types() {
        let mut settings = AppSettings::default();
        settings.set_chat_watched("1".into(), true).unwrap();
        assert!(!settings.should_sync_chat("1"));

        settings
            .set_chat_types(
                "1".into(),
                ChatDownloadTypes {
                    text: true,
                    ..ChatDownloadTypes::default()
                },
            )
            .unwrap();
        assert!(settings.should_sync_chat("1"));

        settings.set_chat_watched("1".into(), false).unwrap();
        assert!(!settings.should_sync_chat("1"));
        assert!(settings.chat_types("1").text);
    }

    #[test]
    fn types_union_and_newly_enabled() {
        let text = ChatDownloadTypes {
            text: true,
            ..ChatDownloadTypes::default()
        };
        let video = ChatDownloadTypes {
            video: true,
            ..ChatDownloadTypes::default()
        };
        let both = text.union(video);
        assert!(both.text && both.video);
        assert!(!both.photo);

        let newly = both.newly_enabled(text);
        assert!(newly.video);
        assert!(!newly.text);
        assert!(newly.any_media());
        assert_eq!(newly.media_kind_names(), vec!["video"]);
        assert!(!both.newly_enabled(both).any());
    }
}
