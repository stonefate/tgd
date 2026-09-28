use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::telegram::{DownloadPhase, DownloadProgress};

pub const DEBOUNCE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifyKind {
    Paused,
    FailedPaused,
    FloodWait,
    Reconnect,
    BackfillDone,
}

pub fn classify(prev: &DownloadProgress, next: &DownloadProgress) -> Option<(NotifyKind, String)> {
    if !prev.paused && next.paused {
        if next
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("下载失败"))
        {
            return Some((
                NotifyKind::FailedPaused,
                format!(
                    "[纸飞机下载器] 下载失败，已暂停{}",
                    next.detail
                        .as_deref()
                        .filter(|detail| !detail.is_empty())
                        .map(|detail| format!("\n{detail}"))
                        .unwrap_or_default()
                ),
            ));
        }
        return Some((NotifyKind::Paused, "[纸飞机下载器] 下载已暂停".into()));
    }

    if prev.phase != DownloadPhase::FloodWait && next.phase == DownloadPhase::FloodWait {
        let secs = next.flood_wait_secs.unwrap_or(0);
        return Some((
            NotifyKind::FloodWait,
            format!("[纸飞机下载器] Telegram 限流，等待 {secs} 秒"),
        ));
    }

    if prev.phase != DownloadPhase::Reconnect && next.phase == DownloadPhase::Reconnect {
        return Some((
            NotifyKind::Reconnect,
            "[纸飞机下载器] 连接断开，正在重连".into(),
        ));
    }

    if prev.phase == DownloadPhase::Backfill
        && next.phase == DownloadPhase::Idle
        && next.detail.as_deref() == Some("回爬已完成")
    {
        return Some((NotifyKind::BackfillDone, "[纸飞机下载器] 回爬已完成".into()));
    }

    None
}

pub struct NotifyTracker {
    prev: Option<DownloadProgress>,
    last_sent: HashMap<NotifyKind, Instant>,
}

impl NotifyTracker {
    pub fn new() -> Self {
        Self {
            prev: None,
            last_sent: HashMap::new(),
        }
    }

    pub fn observe(
        &mut self,
        next: DownloadProgress,
        now: Instant,
    ) -> Option<(NotifyKind, String)> {
        let Some(prev) = self.prev.replace(next.clone()) else {
            return None;
        };
        let (kind, text) = classify(&prev, &next)?;
        if let Some(last) = self.last_sent.get(&kind) {
            if now.saturating_duration_since(*last) < DEBOUNCE {
                return None;
            }
        }
        self.last_sent.insert(kind, now);
        Some((kind, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(phase: DownloadPhase, paused: bool, detail: Option<&str>) -> DownloadProgress {
        DownloadProgress {
            phase,
            paused,
            detail: detail.map(str::to_string),
            flood_wait_secs: None,
            ..DownloadProgress::default()
        }
    }

    #[test]
    fn first_snapshot_is_seed_only() {
        let mut tracker = NotifyTracker::new();
        let now = Instant::now();
        assert!(tracker
            .observe(progress(DownloadPhase::Idle, true, Some("已暂停")), now)
            .is_none());
    }

    #[test]
    fn paused_and_failed_pause() {
        let live = progress(DownloadPhase::Live, false, None);
        let paused = progress(DownloadPhase::Live, true, Some("已暂停"));
        let failed = progress(DownloadPhase::Backfill, true, Some("下载失败，已暂停"));
        assert_eq!(
            classify(&live, &paused).map(|item| item.0),
            Some(NotifyKind::Paused)
        );
        assert_eq!(
            classify(&live, &failed).map(|item| item.0),
            Some(NotifyKind::FailedPaused)
        );
    }

    #[test]
    fn flood_and_reconnect_edges() {
        let live = progress(DownloadPhase::Live, false, None);
        let mut flood = progress(
            DownloadPhase::FloodWait,
            false,
            Some("Telegram 限流，等待 20 秒"),
        );
        flood.flood_wait_secs = Some(20);
        let reconnect = progress(DownloadPhase::Reconnect, false, None);
        assert!(classify(&live, &flood)
            .is_some_and(|(kind, text)| kind == NotifyKind::FloodWait && text.contains("20")));
        assert_eq!(
            classify(&live, &reconnect).map(|item| item.0),
            Some(NotifyKind::Reconnect)
        );
        assert!(classify(&flood, &flood).is_none());
    }

    #[test]
    fn backfill_complete_only_from_backfill() {
        let backfill = progress(DownloadPhase::Backfill, false, None);
        let done = progress(DownloadPhase::Idle, false, Some("回爬已完成"));
        let idle = progress(DownloadPhase::Idle, false, None);
        assert_eq!(
            classify(&backfill, &done).map(|item| item.0),
            Some(NotifyKind::BackfillDone)
        );
        assert!(classify(&idle, &done).is_none());
        assert!(classify(&done, &done).is_none());
    }

    #[test]
    fn progress_ticks_ignored() {
        let mut a = progress(DownloadPhase::Backfill, false, None);
        a.downloaded = 1;
        let mut b = a.clone();
        b.downloaded = 2;
        assert!(classify(&a, &b).is_none());
    }

    #[test]
    fn debounce_same_kind() {
        let mut tracker = NotifyTracker::new();
        let t0 = Instant::now();
        tracker.observe(progress(DownloadPhase::Live, false, None), t0);
        let first = tracker.observe(
            progress(DownloadPhase::Live, true, Some("已暂停")),
            t0 + Duration::from_secs(1),
        );
        assert_eq!(first.map(|item| item.0), Some(NotifyKind::Paused));
        tracker.observe(
            progress(DownloadPhase::Live, false, None),
            t0 + Duration::from_secs(2),
        );
        let second = tracker.observe(
            progress(DownloadPhase::Live, true, Some("已暂停")),
            t0 + Duration::from_secs(10),
        );
        assert!(second.is_none());
        let third = tracker.observe(
            progress(DownloadPhase::Live, false, None),
            t0 + Duration::from_secs(70),
        );
        assert!(third.is_none());
        let fourth = tracker.observe(
            progress(DownloadPhase::Live, true, Some("已暂停")),
            t0 + Duration::from_secs(71),
        );
        assert_eq!(fourth.map(|item| item.0), Some(NotifyKind::Paused));
    }
}
