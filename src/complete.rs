//! Cmd 模式下的 Tab 补全：put/lcd 补本地路径，get 补远端路径（SFTP 列目录），
//! 行内还没空格时补命令词本身。

use std::path::PathBuf;

use crate::input::Completion;
use crate::sftp::Sftp;

/// 可补全的命令词（含 upload/download 兼容别名）。
const COMMANDS: [&str; 6] = ["get", "lcd", "lll", "put", "download", "upload"];

pub async fn compute(line: &str, sftp: &Option<Sftp>, remote_cwd: Option<&str>) -> Completion {
    match line.rfind(' ') {
        // 行内还没有空格：补命令词。
        None => complete_word(line),
        Some(i) => {
            let (head, token) = (&line[..=i], &line[i + 1..]);
            let word = head.trim_end();
            match word {
                "put" | "upload" => local_complete(head, token, false),
                "lcd" => local_complete(head, token, true),
                "get" | "download" => {
                    let Some(s) = sftp else {
                        return Completion::None;
                    };
                    remote_complete(s, head, token, remote_cwd).await
                }
                _ => Completion::None,
            }
        }
    }
}

/// 命令词补全（大小写敏感，与 bash 行为一致）。
fn complete_word(line: &str) -> Completion {
    if line.is_empty() {
        return Completion::None;
    }
    let names: Vec<String> = COMMANDS
        .iter()
        .filter(|w| w.starts_with(line))
        .map(|w| format!("{w} "))
        .collect();
    build("", line, names)
}

/// 本地路径补全。dirs_only = lcd 只补目录。
fn local_complete(head: &str, token: &str, dirs_only: bool) -> Completion {
    let (base, prefix) = split_token(token);
    // Windows 不接受非根路径的尾随分隔符（C:\foo\ 非法），去掉尾部分隔符；
    // 根路径（/ 或 C:\）保持原样。
    let dir = if base.is_empty() {
        PathBuf::from(".")
    } else {
        let t = base.trim_end_matches(['/', '\\']);
        if t.is_empty() || t.ends_with(':') {
            PathBuf::from(base)
        } else {
            PathBuf::from(t)
        }
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Completion::None;
    };
    let mut names: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(prefix) {
            continue;
        }
        // 默认不补隐藏文件（与 bash 一致；显式输入 . 前缀时才补）。
        if prefix.is_empty() && name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if dirs_only && !is_dir {
            continue;
        }
        names.push(if is_dir { format!("{name}/") } else { name });
    }
    names.sort();
    build(&format!("{head}{base}"), prefix, names)
}

/// 远端路径补全。相对路径基于远端 shell 的实时 cwd（OSC7 跟踪）。
async fn remote_complete(sftp: &Sftp, head: &str, token: &str, cwd: Option<&str>) -> Completion {
    let (base, prefix) = match token.rfind('/') {
        Some(i) => (&token[..=i], &token[i + 1..]),
        None => ("", token),
    };
    let dir = if base.starts_with('/') {
        base.trim_end_matches('/').to_string()
    } else {
        match cwd.map(|c| c.trim_end_matches('/').to_string()) {
            Some(r) if !base.is_empty() => format!("{r}/{}", base.trim_end_matches('/')),
            Some(r) if r.is_empty() => "/".to_string(),
            Some(r) => r,
            None if base.is_empty() => ".".to_string(),
            None => base.trim_end_matches('/').to_string(),
        }
    };
    let dir = if dir.is_empty() { "/".to_string() } else { dir };
    let Ok(list) = sftp.list_dir(&dir).await else {
        return Completion::None;
    };
    let mut names: Vec<String> = list
        .into_iter()
        .filter(|(n, _)| n.starts_with(prefix))
        // 默认不补隐藏文件。
        .filter(|(n, _)| !(prefix.is_empty() && n.starts_with('.')))
        .map(|(n, is_dir)| if is_dir { format!("{n}/") } else { n })
        .collect();
    names.sort();
    build(&format!("{head}{base}"), prefix, names)
}

/// 按 `/` 或 `\` 把路径 token 拆成「目录前缀（含分隔符）+ 文件名前缀」。
fn split_token(token: &str) -> (&str, &str) {
    match token.rfind(|c| c == '/' || c == '\\') {
        Some(i) => (&token[..=i], &token[i + 1..]),
        None => ("", token),
    }
}

/// 由候选集生成补全结果：唯一 → 整行；有公共前缀 → 补到公共前缀；否则列候选。
fn build(head_base: &str, prefix: &str, mut names: Vec<String>) -> Completion {
    names.retain(|n| n.starts_with(prefix));
    if names.is_empty() {
        return Completion::None;
    }
    if names.len() == 1 {
        return Completion::Line(format!("{head_base}{}", names[0]));
    }
    let lcp = longest_common_prefix(&names);
    if lcp.len() > prefix.len() {
        return Completion::Line(format!("{head_base}{lcp}"));
    }
    Completion::List {
        line: format!("{head_base}{prefix}"),
        names,
    }
}

fn longest_common_prefix(names: &[String]) -> String {
    let mut lcp = names[0].clone();
    for n in &names[1..] {
        while !n.starts_with(&lcp) {
            lcp.pop();
        }
    }
    lcp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sshu_comp_test_{tag}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn complete_command_word() {
        assert_eq!(complete_word("pu"), Completion::Line("put ".to_string()));
        assert_eq!(complete_word("ge"), Completion::Line("get ".to_string()));
        assert_eq!(
            complete_word("up"),
            Completion::Line("upload ".to_string())
        );
        // l 开头有歧义：列候选
        match complete_word("l") {
            Completion::List { names, .. } => {
                assert_eq!(names, vec!["lcd ".to_string(), "lll ".to_string()])
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(complete_word("xyz"), Completion::None);
    }

    #[test]
    fn local_path_completion() {
        let d = temp_dir("local");
        std::fs::write(d.join("alpha.txt"), b"x").unwrap();
        std::fs::write(d.join("beta.txt"), b"x").unwrap();
        std::fs::create_dir_all(d.join("adir")).unwrap();

        // base 保留用户输入的分隔符（Windows 为 \）
        let dir_s = format!("{}\\", d.display());
        // 唯一匹配 → 补全到文件名
        let c = local_complete("put ", &format!("{dir_s}al"), false);
        assert_eq!(c, Completion::Line(format!("put {dir_s}alpha.txt")));
        // 目录 → 补全并追加 /
        let c = local_complete("put ", &format!("{dir_s}ad"), false);
        assert_eq!(c, Completion::Line(format!("put {dir_s}adir/")));
        // lcd 只补目录
        let c = local_complete("lcd ", &format!("{dir_s}b"), true);
        assert_eq!(c, Completion::None);
        // 有歧义 → 列候选（adir 与 alpha 公共前缀不长于已输入的 "a"）
        let c = local_complete("put ", &format!("{dir_s}a"), false);
        match c {
            Completion::List { names, .. } => {
                assert_eq!(names, vec!["adir/".to_string(), "alpha.txt".to_string()])
            }
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
