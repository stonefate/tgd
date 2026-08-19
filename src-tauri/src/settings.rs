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
    /// 开机自启。缺省关。启动时按这项同步系统登录项。
    #[serde(default)]
    pub autostart: bool,
    /// 暂停全部下载。缺省关。重启后按这项恢复。
    #[serde(default)]
    pub download_paused: bool,
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
            autostart: false,
            download_paused: false,
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
            autostart: true,
            download_paused: true,
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
        assert!(loaded.autostart);
        assert!(loaded.download_paused);

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
        assert!(!loaded.autostart);
        assert!(!loaded.download_paused);
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
