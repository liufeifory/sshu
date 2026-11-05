use std::time::{Duration, Instant};

use crate::sftp::TransferSummary;

#[derive(Debug, Clone, PartialEq)]
pub enum TransferStatus {
    Running { name: String, done: u64, total: u64 },
    Done(TransferSummary),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatusLine {
    Show(String),
    Clear,
    Nothing,
}

/// 传输状态 + 「完成结果保留 3 秒」计时。
/// `begin()` 必须在新传输开始时调用，否则上一条结果的计时会吞掉新结果。
pub struct StatusSlot {
    status: Option<TransferStatus>,
    clear_at: Option<Instant>,
    ttl: Duration,
}

impl StatusSlot {
    pub fn new() -> Self {
        Self {
            status: None,
            clear_at: None,
            ttl: Duration::from_secs(3),
        }
    }

    pub fn begin(&mut self) {
        self.clear_at = None;
    }

    pub fn set(&mut self, status: TransferStatus, now: Instant) {
        let running = matches!(status, TransferStatus::Running { .. });
        self.status = Some(status);
        if running {
            self.clear_at = None;
        } else {
            self.clear_at = Some(now + self.ttl);
        }
    }

    pub fn poll(&mut self, now: Instant) -> StatusLine {
        let Some(status) = &self.status else {
            return StatusLine::Nothing;
        };
        if let Some(deadline) = self.clear_at {
            if now >= deadline {
                self.status = None;
                self.clear_at = None;
                return StatusLine::Clear;
            }
        }
        StatusLine::Show(render(status))
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, Some(TransferStatus::Running { .. }))
    }

    pub fn has_status(&self) -> bool {
        self.status.is_some()
    }
}

pub fn render(status: &TransferStatus) -> String {
    match status {
        TransferStatus::Running { name, done, total } => {
            let base = name.rsplit('/').next().unwrap_or(name);
            if *total > 0 {
                let pct = 100.0 * *done as f64 / *total as f64;
                format!(
                    "  {:5.1}%  {}  {} / {}",
                    pct,
                    base,
                    human(*done),
                    human(*total)
                )
            } else {
                format!("  {base} ...")
            }
        }
        TransferStatus::Done(r) => {
            let extra = if r.skipped > 0 {
                format!(", 跳过 {} 个软链", r.skipped)
            } else {
                String::new()
            };
            format!(
                "  \u{2713} {}  ({}, {} 个文件{}, {:.2}s)",
                r.dest,
                human(r.size),
                r.files,
                extra,
                r.seconds
            )
        }
        TransferStatus::Failed(e) => format!("  \u{2717} {e}"),
    }
}

pub fn human(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    for (i, unit) in units.iter().enumerate() {
        if value < 1024.0 || i == units.len() - 1 {
            return if i == 0 {
                format!("{n} {unit}")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn summary() -> TransferSummary {
        TransferSummary { dest: "/tmp/a".into(), size: 1, files: 1, skipped: 0, seconds: 0.0 }
    }

    #[test]
    fn done_shows_then_clears() {
        let t0 = Instant::now();
        let mut slot = StatusSlot::new();
        slot.begin();
        slot.set(TransferStatus::Done(summary()), t0);
        match slot.poll(t0 + Duration::from_secs(1)) {
            StatusLine::Show(s) => assert!(s.contains('\u{2713}'), "{s}"),
            other => panic!("expected show, got {other:?}"),
        }
        assert_eq!(slot.poll(t0 + Duration::from_secs(4)), StatusLine::Clear);
        assert_eq!(slot.poll(t0 + Duration::from_secs(5)), StatusLine::Nothing);
    }

    #[test]
    fn back_to_back_failure_not_swallowed() {
        let t0 = Instant::now();
        let mut slot = StatusSlot::new();
        slot.begin();
        slot.set(TransferStatus::Done(summary()), t0);
        slot.begin();
        slot.set(
            TransferStatus::Failed("远端路径不存在: /nope".into()),
            t0 + Duration::from_millis(500),
        );
        match slot.poll(t0 + Duration::from_millis(600)) {
            StatusLine::Show(s) => assert!(s.contains("不存在"), "{s}"),
            other => panic!("expected failure shown, got {other:?}"),
        }
    }

    #[test]
    fn running_shows_percent() {
        let mut slot = StatusSlot::new();
        slot.begin();
        slot.set(
            TransferStatus::Running { name: "/tmp/a".into(), done: 50, total: 100 },
            Instant::now(),
        );
        match slot.poll(Instant::now()) {
            StatusLine::Show(s) => assert!(s.contains("50.0%"), "{s}"),
            other => panic!("expected show, got {other:?}"),
        }
    }
}
