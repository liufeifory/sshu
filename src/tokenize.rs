/// 复刻 Python `shlex.split(line, posix=False)` 后对每个 token
/// 执行 `strip("\"'")` 的结果（见设计文档 §7.5）。
pub fn split_command_line(line: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_token = false;

    for ch in line.chars() {
        if let Some(q) = quote {
            cur.push(ch);
            if ch == q {
                quote = None;
                tokens.push(std::mem::take(&mut cur));
                in_token = false;
            }
            continue;
        }

        // 引号只在 token 起始处成组；token 中间的引号是普通字符。
        if (ch == '\'' || ch == '"') && !in_token {
            quote = Some(ch);
            cur.push(ch);
            in_token = true;
            continue;
        }

        if ch.is_whitespace() {
            if in_token {
                tokens.push(std::mem::take(&mut cur));
                in_token = false;
            }
            continue;
        }

        cur.push(ch);
        in_token = true;
    }

    if quote.is_some() {
        return Err("参数解析失败: No closing quotation".to_string());
    }
    if in_token {
        tokens.push(cur);
    }

    Ok(tokens
        .into_iter()
        .map(|t| t.trim_matches(|c| c == '\'' || c == '"').to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_shlex_parity() {
        let c = |s: &str| split_command_line(s).unwrap();
        assert_eq!(c(r"upload C:\a.txt"), ["upload", r"C:\a.txt"]);
        assert_eq!(
            c(r#"upload "C:\a b\c.txt" /tmp"#),
            ["upload", r"C:\a b\c.txt", "/tmp"]
        );
        assert_eq!(c(r"upload C:\a b\c.txt"), ["upload", r"C:\a", r"b\c.txt"]);
        assert_eq!(c("download '/tmp/a b'"), ["download", "/tmp/a b"]);
        assert_eq!(c(r#"upload """#), ["upload", ""]);
        assert_eq!(c("upload a\tb"), ["upload", "a", "b"]);
        assert_eq!(c(r#"upload "a'b""#), ["upload", "a'b"]);
        assert_eq!(c("upload 'a'b'"), ["upload", "a", "b"]);
        assert_eq!(c("upload a'b'c"), ["upload", "a'b'c"]);
        assert_eq!(c("upload ''"), ["upload", ""]);
        assert_eq!(c("upload"), ["upload"]);
        assert_eq!(c(""), Vec::<String>::new());
        assert_eq!(c(r#"upload "a b"c"#), ["upload", "a b", "c"]);
        assert_eq!(c(r#"upload "a""b""#), ["upload", "a", "b"]);
        assert_eq!(c(r#"upload a"b c""#), ["upload", r#"a"b"#, "c"]);
        assert_eq!(c(r#"upload ""x"#), ["upload", "", "x"]);
        assert!(split_command_line("upload \"unterminated").is_err());
        assert!(split_command_line("upload 'unterminated").is_err());
    }
}
