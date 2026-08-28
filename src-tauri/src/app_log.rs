use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use specta::Type;

const CAP: usize = 50;
const MESSAGE_MAX: usize = 500;

/// 设置页展示的最近 warn / error。
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
}

struct LogRing {
    entries: Mutex<VecDeque<LogEntry>>,
}

impl LogRing {
    fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(CAP)),
        }
    }

    fn record(&self, level: log::Level, message: impl Into<String>) {
        if level > log::Level::Warn {
            return;
        }
        let entry = LogEntry {
            time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            level: if level == log::Level::Error {
                "error".into()
            } else {
                "warn".into()
            },
            message: clip_message(message.into()),
        };
        let mut guard = self.entries.lock().unwrap_or_else(|err| err.into_inner());
        if guard.len() >= CAP {
            guard.pop_front();
        }
        guard.push_back(entry);
    }

    fn snapshot(&self) -> Vec<LogEntry> {
        let guard = self.entries.lock().unwrap_or_else(|err| err.into_inner());
        guard.iter().rev().cloned().collect()
    }
}

fn clip_message(raw: String) -> String {
    let flat: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = flat.chars().count();
    if count <= MESSAGE_MAX {
        return flat;
    }
    flat.chars().take(MESSAGE_MAX).collect()
}

static RING: OnceLock<LogRing> = OnceLock::new();

fn ring() -> &'static LogRing {
    RING.get_or_init(LogRing::new)
}

pub fn snapshot() -> Vec<LogEntry> {
    ring().snapshot()
}

struct CaptureLogger {
    inner: Box<dyn log::Log>,
}

impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if record.level() <= log::Level::Warn {
            ring().record(record.level(), record.args().to_string());
        }
        self.inner.log(record);
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(feature = "desktop")]
struct StderrLogger;

#[cfg(feature = "desktop")]
impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        eprintln!(
            "{} {:<5} {}: {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

fn install(inner: Box<dyn log::Log>, max_level: log::LevelFilter) {
    if log::set_boxed_logger(Box::new(CaptureLogger { inner })).is_ok() {
        log::set_max_level(max_level);
    }
}

/// 桌面端：stderr + 环形缓冲。`tauri_plugin_log` 需 `skip_logger`。
#[cfg(feature = "desktop")]
pub fn init() {
    install(Box::new(StderrLogger), log::LevelFilter::Info);
}

/// 飞牛：沿用 env_logger 格式，并写入环形缓冲。
#[cfg(feature = "server")]
pub fn init_env() {
    let env =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let filter = env.filter();
    install(Box::new(env), filter);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_last_fifty_newest_first() {
        let ring = LogRing::new();
        for i in 0..60 {
            ring.record(log::Level::Warn, format!("n{i}"));
        }
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 50);
        assert_eq!(snap[0].message, "n59");
        assert_eq!(snap[0].level, "warn");
        assert_eq!(snap[49].message, "n10");
    }

    #[test]
    fn ignores_info_and_clips_message() {
        let ring = LogRing::new();
        ring.record(log::Level::Info, "skip");
        ring.record(log::Level::Error, format!("{}\n  extra", "x".repeat(600)));
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].level, "error");
        assert_eq!(snap[0].message.chars().count(), MESSAGE_MAX);
        assert!(!snap[0].message.contains('\n'));
    }
}
