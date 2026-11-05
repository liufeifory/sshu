//! 本地目录操作：lcd（切换/显示本地工作目录）与 lll（本地长格式列表）。
//!
//! 两者都只操作客户端进程自身的文件系统；put 的本地源路径与 get 的默认
//! 下载目录都基于 `std::env::current_dir()`，因此 lcd 直接生效。

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::term::Console;

/// lcd [目录]：切换本地工作目录；省略参数 = 显示当前目录。
pub fn lcd(dir: Option<String>, console: &Console) {
    let res = match &dir {
        Some(d) => std::env::set_current_dir(d),
        None => Ok(()),
    };
    let text = match res {
        Ok(()) => {
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".to_string());
            format!("本地目录: {cwd}\r\n")
        }
        Err(e) => format!("失败: 无法切换到 {}: {e}\r\n", dir.unwrap_or_default()),
    };
    let _ = console.write_all(text.as_bytes());
}

/// lll：列出本地当前目录（目录优先，之后按名称排序），格式对齐 `ls -l`。
pub fn lll(console: &Console) {
    let mut text = String::new();
    match list_cwd() {
        Ok(lines) => {
            text.push_str(&format!("总计 {} 项\r\n", lines.len()));
            for line in lines {
                text.push_str(&line);
                text.push_str("\r\n");
            }
        }
        Err(e) => text.push_str(&format!("失败: 无法读取本地目录: {e}\r\n")),
    }
    let _ = console.write_all(text.as_bytes());
}

fn list_cwd() -> std::io::Result<Vec<String>> {
    let mut rows: Vec<(bool, String, String)> = Vec::new();
    for entry in fs::read_dir(".")? {
        let entry = entry?;
        let md = entry.metadata()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = md.is_dir();
        let size = if is_dir { "-".to_string() } else { human(md.len()) };
        let mtime = fmt_time(md.modified().unwrap_or(UNIX_EPOCH));
        let perms = perms_string(&md, is_dir);
        let suffix = if is_dir { "/" } else { "" };
        rows.push((
            is_dir,
            name.to_lowercase(),
            format!("{perms} {size:>9} {mtime} {name}{suffix}"),
        ));
    }
    rows.sort_by(|a, b| (!a.0).cmp(&(!b.0)).then(a.1.cmp(&b.1)));
    Ok(rows.into_iter().map(|(_, _, line)| line).collect())
}

/// 权限位：Unix 用真实 mode；Windows 用简化近似（目录/只读/可写）。
#[cfg(unix)]
fn perms_string(md: &fs::Metadata, is_dir: bool) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = md.permissions().mode();
    let bits = mode & 0o777;
    let mut s = String::with_capacity(10);
    s.push(if is_dir { 'd' } else if mode & 0o170_000 == 0o120_000 { 'l' } else { '-' });
    for shift in [6, 3, 0] {
        let tri = (bits >> shift) & 0o7;
        s.push(if tri & 4 != 0 { 'r' } else { '-' });
        s.push(if tri & 2 != 0 { 'w' } else { '-' });
        s.push(if tri & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[cfg(windows)]
fn perms_string(md: &fs::Metadata, is_dir: bool) -> String {
    if is_dir {
        "drwxr-xr-x".to_string()
    } else if md.permissions().readonly() {
        "-r--r--r--".to_string()
    } else {
        "-rw-r--r--".to_string()
    }
}

fn human(n: u64) -> String {
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

/// SystemTime → "YYYY-MM-DD HH:MM"（本地UTC偏移不引入依赖，按 UTC 显示）。
fn fmt_time(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", sod / 3600, (sod % 3600) / 60)
}

/// Howard Hinnant 的 civil_from_days：epoch 天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
        assert_eq!(civil_from_days(20_680), (2026, 8, 15));
    }

    #[test]
    fn fmt_time_formats_minutes() {
        let t = UNIX_EPOCH + Duration::from_secs(1_783_286_400); // 2026-07-05 21:20:00 UTC
        assert_eq!(fmt_time(t), "2026-07-05 21:20");
    }

    #[test]
    fn list_cwd_sorts_dirs_first() {
        let tmp = std::env::temp_dir().join(format!("sshu_lll_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("a.txt"), b"hello").unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        let lines = list_cwd().unwrap();
        std::env::set_current_dir(prev).unwrap();
        let _ = fs::remove_dir_all(&tmp);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("sub/"), "{lines:?}");
        assert!(lines[1].contains("a.txt"), "{lines:?}");
        assert!(lines[1].contains("5 B"), "{lines:?}");
    }
}
