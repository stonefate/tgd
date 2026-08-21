use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::settings::AppSettings;

/// 本地 Telegram 会话与下载目录。
///
/// 会话落在系统 app data 下（macOS: `~/Library/Application Support/com.tgd.app`）。
/// 下载目录默认是同路径下的 `downloads`，用户改过则读 `settings.json`。
/// grammers 0.10 用 `SqliteSession` 把授权密钥写进 `session_file`，
/// 掉线重连时复用，避免反复登录。
pub struct SessionPaths {
    pub root: PathBuf,
    pub session_file: PathBuf,
    pub download_dir: PathBuf,
}

impl SessionPaths {
    pub fn default_download_dir(root: &std::path::Path) -> PathBuf {
        root.join("downloads")
    }

    pub fn from_root(root: impl Into<PathBuf>, download_dir_override: Option<&Path>) -> Self {
        let root = root.into();
        let settings = AppSettings::load(&root);
        // 设置页改过的路径优先；否则用 TGD_DOWNLOAD_DIR，再否则默认。
        let download_dir = settings
            .download_dir
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| {
                download_dir_override
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(Path::to_path_buf)
            })
            .unwrap_or_else(|| Self::default_download_dir(&root));

        Self {
            session_file: root.join("telegram.session"),
            download_dir,
            root,
        }
    }

    #[cfg(feature = "desktop")]
    pub fn resolve(app: &tauri::AppHandle) -> Result<Self, AppError> {
        use tauri::Manager;
        let root = app
            .path()
            .app_data_dir()
            .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(Self::from_root(root, None))
    }

    pub fn parse_download_dir(raw: &str) -> Result<PathBuf, AppError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(AppError::Io("下载目录不能为空".into()));
        }
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(AppError::Io("下载目录必须是绝对路径".into()));
        }
        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(AppError::Io("下载目录不能包含 ..".into()));
        }
        Ok(path)
    }

    pub fn set_download_dir(&mut self, dir: PathBuf) -> Result<(), AppError> {
        if dir.as_os_str().is_empty() {
            return Err(AppError::Io("下载目录不能为空".into()));
        }

        std::fs::create_dir_all(&dir).map_err(|err| AppError::Io(err.to_string()))?;
        let dir = dir.canonicalize().unwrap_or(dir);

        let mut settings = AppSettings::load(&self.root);
        settings.download_dir = Some(dir.clone());
        settings.save(&self.root)?;
        self.download_dir = dir;
        Ok(())
    }

    pub fn ensure_dirs(&self) -> Result<(), AppError> {
        std::fs::create_dir_all(&self.root).map_err(|err| AppError::Io(err.to_string()))?;
        std::fs::create_dir_all(&self.download_dir).map_err(|err| AppError::Io(err.to_string()))?;
        Ok(())
    }

    /// 让 webview 能用 asset 协议读下载目录里的媒体。
    #[cfg(feature = "desktop")]
    pub fn allow_asset_access(&self, app: &tauri::AppHandle) {
        use tauri::Manager;
        if let Err(err) = app
            .asset_protocol_scope()
            .allow_directory(&self.download_dir, true)
        {
            log::warn!(
                "asset protocol: cannot allow {}: {err}",
                self.download_dir.display()
            );
        }
    }

    pub fn session_exists(&self) -> bool {
        self.session_file.exists()
    }

    pub fn remove_session(&self) -> Result<(), AppError> {
        let path = &self.session_file;
        let extras = [
            path.with_file_name(format!(
                "{}-wal",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
            path.with_file_name(format!(
                "{}-shm",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
        ];

        if path.exists() {
            std::fs::remove_file(path).map_err(|err| AppError::Io(err.to_string()))?;
        }
        for extra in extras {
            if extra.exists() {
                let _ = std::fs::remove_file(extra);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_download_dir_rejects_relative_and_dotdot() {
        assert!(SessionPaths::parse_download_dir("").is_err());
        assert!(SessionPaths::parse_download_dir("downloads").is_err());
        assert!(SessionPaths::parse_download_dir("/tmp/../etc").is_err());
        assert_eq!(
            SessionPaths::parse_download_dir("  /downloads  ").unwrap(),
            PathBuf::from("/downloads")
        );
    }

    #[test]
    fn settings_download_dir_beats_env_override() {
        let root = std::env::temp_dir().join(format!(
            "tgd-session-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let chosen = root.join("media");
        std::fs::create_dir_all(&chosen).unwrap();
        let mut paths = SessionPaths::from_root(&root, Some(Path::new("/downloads")));
        paths.set_download_dir(chosen.clone()).unwrap();
        let loaded = SessionPaths::from_root(&root, Some(Path::new("/downloads")));
        assert_eq!(loaded.download_dir, chosen.canonicalize().unwrap_or(chosen));
        let _ = std::fs::remove_dir_all(&root);
    }
}
