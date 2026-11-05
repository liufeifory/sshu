//! 轻量调试日志：`--debug` 开启后按击键级别记录输入与状态机转移，
//! 输出追加到 `%TEMP%\sshu-debug.log`，用于事后定位交互问题。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static LOGGER: OnceLock<Mutex<Option<File>>> = OnceLock::new();

pub fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("sshu-debug.log")
}

/// 打开日志文件（追加）。只在进程启动时调用一次。
pub fn init() {
    let logger = LOGGER.get_or_init(|| Mutex::new(None));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .ok();
    if let Some(f) = file.as_mut() {
        let _ = writeln!(f, "\n===== session start {} =====", timestamp());
    }
    *logger.lock().unwrap() = file;
}

/// 记录一条事件；未开启日志时为空操作。
pub fn log(kind: &str, detail: &str) {
    let Some(mutex) = LOGGER.get() else {
        return;
    };
    let Ok(mut guard) = mutex.lock() else {
        return;
    };
    let Some(file) = guard.as_mut() else {
        return;
    };
    let _ = writeln!(file, "{} [{}] {}", timestamp(), kind, detail);
}

/// 记录字节序列：十六进制 + 可打印 ASCII 对照。
pub fn log_bytes(kind: &str, bytes: &[u8]) {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let text: String = bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                (b as char).to_string()
            } else {
                ".".to_string()
            }
        })
        .collect();
    log(kind, &format!("len={} hex={ascii_hex} text={text}", bytes.len(), ascii_hex = hex));
}

fn timestamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    // UTC，足够定位问题；避免引入时区依赖。
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default() {
        // 未 init 时 log 不应 panic
        log("test", "no-op");
        log_bytes("test", b"ab");
    }
}
