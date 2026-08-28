use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::protocol::DEFAULT_BASE_URL;
use crate::error::AppError;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IlinkSession {
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub baseurl: String,
    #[serde(default)]
    pub bot_id: String,
    /// 扫码授权的微信机主，绑定只接受该 id 的消息
    #[serde(default)]
    pub owner_user_id: Option<String>,
    #[serde(default)]
    pub target_user_id: Option<String>,
    #[serde(default)]
    pub context_token: Option<String>,
    #[serde(default)]
    pub get_updates_buf: String,
}

impl IlinkSession {
    pub fn file_path(root: &Path) -> PathBuf {
        root.join("ilink.json")
    }

    pub fn load(root: &Path) -> Self {
        let path = Self::file_path(root);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&raw) {
            Ok(session) => session,
            Err(err) => {
                log::warn!("failed to parse ilink.json: {err}");
                Self::default()
            }
        }
    }

    pub fn save(&self, root: &Path) -> Result<(), AppError> {
        std::fs::create_dir_all(root).map_err(|err| AppError::Io(err.to_string()))?;
        let path = Self::file_path(root);
        let raw =
            serde_json::to_string_pretty(self).map_err(|err| AppError::Io(err.to_string()))?;
        std::fs::write(&path, raw).map_err(|err| AppError::Io(err.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn delete(root: &Path) {
        let _ = std::fs::remove_file(Self::file_path(root));
    }

    pub fn logged_in(&self) -> bool {
        !self.bot_token.trim().is_empty()
    }

    pub fn bound(&self) -> bool {
        self.logged_in()
            && self
                .target_user_id
                .as_deref()
                .map(str::trim)
                .is_some_and(|id| !id.is_empty())
            && self
                .context_token
                .as_deref()
                .map(str::trim)
                .is_some_and(|id| !id.is_empty())
    }

    pub fn base_url(&self) -> &str {
        let url = self.baseurl.trim();
        if url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            url
        }
    }

    pub fn bind_inbound(&mut self, from_user_id: &str, context_token: &str) -> bool {
        let from = from_user_id.trim();
        let token = context_token.trim();
        if from.is_empty() || token.is_empty() {
            return false;
        }
        // 旧会话没记机主时不设限，保持原有行为
        if let Some(owner) = self
            .owner_user_id
            .as_deref()
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
        {
            if from != owner {
                return false;
            }
        }
        match self
            .target_user_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            None => {
                self.target_user_id = Some(from.to_string());
                self.context_token = Some(token.to_string());
                true
            }
            Some(existing) if existing == from => {
                let changed = self.context_token.as_deref() != Some(token);
                self.context_token = Some(token.to_string());
                changed
            }
            Some(_) => false,
        }
    }

    pub fn user_hint(&self) -> Option<String> {
        let raw = self.target_user_id.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let name = raw.split('@').next().unwrap_or(raw);
        if name.chars().count() <= 8 {
            Some(name.to_string())
        } else {
            let prefix: String = name.chars().take(8).collect();
            Some(format!("{prefix}…"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_first_then_refresh_same_user() {
        let mut session = IlinkSession {
            bot_token: "token".into(),
            ..Default::default()
        };
        assert!(session.bind_inbound("alice@im.wechat", "tok-1"));
        assert!(!session.bind_inbound("bob@im.wechat", "tok-2"));
        assert_eq!(session.target_user_id.as_deref(), Some("alice@im.wechat"));
        assert!(session.bind_inbound("alice@im.wechat", "tok-3"));
        assert_eq!(session.context_token.as_deref(), Some("tok-3"));
        assert!(session.bound());
    }

    #[test]
    fn bind_restricted_to_owner() {
        let mut session = IlinkSession {
            bot_token: "token".into(),
            owner_user_id: Some("alice@im.wechat".into()),
            ..Default::default()
        };
        assert!(!session.bind_inbound("bob@im.wechat", "tok-1"));
        assert!(session.bind_inbound("alice@im.wechat", "tok-1"));
        assert_eq!(session.target_user_id.as_deref(), Some("alice@im.wechat"));
        assert!(session.bind_inbound("alice@im.wechat", "tok-2"));
        assert_eq!(session.context_token.as_deref(), Some("tok-2"));
    }

    #[test]
    fn hint_truncates() {
        let session = IlinkSession {
            target_user_id: Some("abcdefghijk@im.wechat".into()),
            ..Default::default()
        };
        assert_eq!(session.user_hint().as_deref(), Some("abcdefgh…"));
    }
}
