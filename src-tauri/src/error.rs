use serde::Serialize;
use specta::Type;

#[derive(Debug, thiserror::Error, Serialize, Type)]
#[serde(tag = "kind", content = "message")]
pub enum AppError {
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Telegram(String),
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Internal(String),
}

impl From<tauri::Error> for AppError {
    fn from(value: tauri::Error) -> Self {
        Self::Internal(value.to_string())
    }
}

impl From<grammers_client::InvocationError> for AppError {
    fn from(value: grammers_client::InvocationError) -> Self {
        Self::Telegram(value.to_string())
    }
}
