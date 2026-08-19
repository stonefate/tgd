use std::path::PathBuf;

use tauri::{AppHandle, Manager};

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

    pub fn resolve(app: &AppHandle) -> Result<Self, AppError> {
        let root = app
            .path()
            .app_data_dir()
            .map_err(|err| AppError::Io(err.to_string()))?;
        let settings = AppSettings::load(&root);
        let download_dir = settings
            .download_dir
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Self::default_download_dir(&root));

        Ok(Self {
            session_file: root.join("telegram.session"),
            download_dir,
            root,
        })
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
    pub fn allow_asset_access(&self, app: &AppHandle) {
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
