use std::path::{Path, PathBuf};

use crate::error::SshuError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    Missing,
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dest {
    pub path: String,
    pub create_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDest {
    pub path: PathBuf,
    pub create_dir: bool,
}

/// 把用户给的远端路径解析成绝对路径：空 = 当前 cwd，相对 = 相对 cwd，~ = 家目录。
pub fn abs_remote(raw: &str, cwd: Option<&str>, home: &str) -> Result<String, SshuError> {
    if raw.is_empty() {
        return cwd.map(str::to_string).ok_or_else(|| {
            SshuError::new("未知远端当前目录（shell 未上报 cwd），请给出绝对路径")
        });
    }

    let t = raw.trim().replace('\\', "/");
    let joined = if let Some(rest) = t.strip_prefix('~') {
        if rest.is_empty() {
            home.to_string()
        } else {
            format!("{}{}", home.trim_end_matches('/'), rest)
        }
    } else if t.starts_with('/') {
        t
    } else {
        let c = cwd.ok_or_else(|| {
            SshuError::new(format!("未知远端当前目录，无法解析相对路径 {raw:?}"))
        })?;
        format!("{}/{}", c.trim_end_matches('/'), t)
    };

    Ok(normalize_posix(&joined))
}

/// scp 风格：目标已存在为目录，或以 `/`、`\` 结尾 -> 放进该目录（可能需创建）；
/// 否则目标就是最终文件名。
pub fn remote_dest(
    target: Option<&str>,
    source_name: &str,
    cwd: Option<&str>,
    home: &str,
    existing: Existing,
) -> Result<Dest, SshuError> {
    let Some(raw) = target else {
        let c = cwd.ok_or_else(|| {
            SshuError::new("未知远端当前目录（shell 未上报 cwd），请显式指定：put <本地路径> <远端目录>")
        })?;
        return Ok(Dest {
            path: format!("{}/{}", c.trim_end_matches('/'), source_name),
            create_dir: false,
        });
    };

    let abs = abs_remote(raw, cwd, home)?;
    let trailing = raw.trim_end().ends_with('/') || raw.trim_end().ends_with('\\');
    let (is_dir, create) = match existing {
        Existing::Dir => (true, false),
        Existing::File => (false, false),
        Existing::Missing => (trailing, trailing),
    };

    Ok(if is_dir {
        Dest {
            path: format!("{}/{}", abs.trim_end_matches('/'), source_name),
            create_dir: create,
        }
    } else {
        Dest { path: abs, create_dir: false }
    })
}

/// 本地目标：规则与 remote_dest 相同；相对路径相对启动目录解析。
pub fn local_dest(
    raw: Option<&str>,
    remote_name: &str,
    startup_dir: &Path,
    existing: Existing,
) -> LocalDest {
    let Some(raw) = raw else {
        return LocalDest { path: startup_dir.join(remote_name), create_dir: false };
    };

    let raw = expand_home(raw);
    let mut p = PathBuf::from(&raw);
    if !p.is_absolute() {
        p = startup_dir.join(p);
    }

    let trailing = raw.trim_end().ends_with('/') || raw.trim_end().ends_with('\\');
    match existing {
        Existing::Dir => LocalDest { path: p.join(remote_name), create_dir: false },
        Existing::File => LocalDest { path: p, create_dir: false },
        Existing::Missing => {
            if trailing {
                LocalDest { path: p.join(remote_name), create_dir: true }
            } else {
                LocalDest { path: p, create_dir: false }
            }
        }
    }
}

/// Windows 本地文件名安全校验：拒绝路径分隔符、盘符冒号和 . / ..
pub fn safe_local_name(name: &str) -> Option<&str> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    if name.contains(['/', '\\', ':', '\0']) {
        return None;
    }
    Some(name)
}

pub fn remote_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "download".to_string(),
    }
}

pub fn expand_home(raw: &str) -> String {
    if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(|h| h.to_string_lossy().into_owned());
        if let Some(h) = home {
            return format!("{}{}", h.trim_end_matches(['\\', '/']), &raw[1..]);
        }
    }
    raw.to_string()
}

fn normalize_posix(path: &str) -> String {
    let abs = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(last) = parts.last() {
                    if *last == ".." {
                        if !abs {
                            parts.push("..");
                        }
                    } else {
                        parts.pop();
                    }
                } else if !abs {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    if abs {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn remote_rules() {
        let d = |t: Option<&str>, e| remote_dest(t, "a.txt", Some("/tmp"), "/root", e).unwrap();
        assert_eq!(d(None, Existing::Missing).path, "/tmp/a.txt");
        assert_eq!(d(Some("/data"), Existing::Dir).path, "/data/a.txt");
        assert_eq!(d(Some("/new/"), Existing::Missing).path, "/new/a.txt");
        assert!(d(Some("/new/"), Existing::Missing).create_dir);
        assert_eq!(d(Some("/tmp/renamed"), Existing::Missing).path, "/tmp/renamed");
        assert_eq!(d(Some("/tmp/f"), Existing::File).path, "/tmp/f");
        assert_eq!(abs_remote("~/x", None, "/root").unwrap(), "/root/x");
        assert_eq!(abs_remote("a\\b", Some("/tmp"), "/root").unwrap(), "/tmp/a/b");
        assert!(abs_remote("a", None, "/root").is_err());
    }

    #[test]
    fn safe_local_names() {
        assert_eq!(safe_local_name("a.txt"), Some("a.txt"));
        assert_eq!(safe_local_name("..\\..\\evil.exe"), None);
        assert_eq!(safe_local_name("C:\\Windows\\x.exe"), None);
        assert_eq!(safe_local_name(".."), None);
        assert_eq!(safe_local_name("a/b"), None);
        assert_eq!(safe_local_name(""), None);
    }

    #[test]
    fn remote_basename() {
        assert_eq!(remote_name("/"), "download");
        assert_eq!(remote_name("/tmp/a.txt"), "a.txt");
        assert_eq!(remote_name("//"), "download");
    }

    #[test]
    fn expands_home() {
        let home = std::env::var("USERPROFILE").unwrap();
        let e = expand_home("~/x");
        assert!(e.starts_with(&home), "{e}");
        assert!(e.ends_with("x"), "{e}");
        assert!(!e.contains('~'), "{e}");
    }

    #[test]
    fn local_rules() {
        let d = |raw: Option<&str>, e| local_dest(raw, "a.txt", Path::new(r"C:\start"), e);
        assert_eq!(d(None, Existing::Missing).path, Path::new(r"C:\start\a.txt"));
        assert_eq!(d(Some(r"C:\dir"), Existing::Dir).path, Path::new(r"C:\dir\a.txt"));
        assert_eq!(d(Some(r"C:\new\"), Existing::Missing).path, Path::new(r"C:\new\a.txt"));
        assert!(d(Some(r"C:\new\"), Existing::Missing).create_dir);
        assert_eq!(d(Some(r"C:\renamed.txt"), Existing::Missing).path, Path::new(r"C:\renamed.txt"));
    }
}
