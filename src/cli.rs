use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(name = "sshu", version, about = "SSH 客户端：在交互会话里额外提供 put / get / lcd / lll")]
pub struct Args {
    #[arg(value_name = "[user@]host[:port]")]
    pub target: String,

    #[arg(short = 'p', long)]
    pub port: Option<u16>,

    #[arg(short = 'l', long)]
    pub user: Option<String>,

    #[arg(long)]
    pub password: Option<String>,

    #[arg(long, default_value = "c-]")]
    pub prefix: String,

    #[arg(long = "no-magic", action = clap::ArgAction::SetFalse)]
    pub magic: bool,

    #[arg(long = "no-hook", action = clap::ArgAction::SetFalse)]
    pub hook: bool,

    #[arg(long = "strict-host-key")]
    pub strict_host_key: bool,

    /// 记录击键级调试日志到 %TEMP%\sshu-debug.log
    #[arg(long)]
    pub debug: bool,

    #[arg(long, default_value_t = 20)]
    pub timeout: u64,
}

pub fn parse() -> Args {
    let args = Args::parse();
    if let Err(e) = parse_prefix(&args.prefix) {
        eprintln!("{e}");
        std::process::exit(2);
    }
    args
}

/// Python 版 `split_target` 的等价实现：`[user@]host[:port]`，支持 `[::1]:port`。
pub fn split_target(spec: &str, default_port: Option<u16>) -> (Option<String>, String, u16) {
    let (user, hostport) = match spec.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h),
        None => (None, spec),
    };
    let mut port = default_port.unwrap_or(22);

    if let Some(rest) = hostport.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            if let Some(p) = tail.strip_prefix(':') {
                if let Ok(v) = p.parse::<u16>() {
                    port = v;
                }
            }
            return (user, host.to_string(), port);
        }
    }

    if hostport.matches(':').count() == 1 {
        let (host, p) = hostport.split_once(':').unwrap();
        if let Ok(v) = p.parse::<u16>() {
            return (user, host.to_string(), v);
        }
    }

    (user, hostport.to_string(), port)
}

/// Python 版 `parse_prefix`：`c-]` / `ctrl-a` / `c-space` / 单字符。
pub fn parse_prefix(s: &str) -> Result<u8, String> {
    let text = s.trim().to_ascii_lowercase();
    for pre in ["c-", "ctrl-"] {
        if let Some(k) = text.strip_prefix(pre) {
            if k == "space" {
                return Ok(0x00);
            }
            let bytes = k.as_bytes();
            if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
                return Ok(bytes[0] - b'a' + 1);
            }
            let code = match k {
                "[" => 0x1b,
                "\\" => 0x1c,
                "]" => 0x1d,
                "^" => 0x1e,
                "_" => 0x1f,
                _ => return Err(format!("无法识别的前缀键: {s}")),
            };
            return Ok(code);
        }
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() == 1 && (chars[0] as u32) < 128 {
        return Ok(chars[0] as u8);
    }
    Err(format!("无法识别的前缀键: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            split_target("alice@example.com:2222", None),
            (Some("alice".into()), "example.com".into(), 2222)
        );
        assert_eq!(split_target("[::1]:2200", None), (None, "::1".into(), 2200));
        assert_eq!(split_target("host", Some(2222)), (None, "host".into(), 2222));
        assert_eq!(split_target("host", None), (None, "host".into(), 22));
    }

    #[test]
    fn prefixes() {
        assert_eq!(parse_prefix("c-]").unwrap(), 0x1d);
        assert_eq!(parse_prefix("ctrl-a").unwrap(), 0x01);
        assert_eq!(parse_prefix("c-space").unwrap(), 0x00);
        assert_eq!(parse_prefix("c-[").unwrap(), 0x1b);
        assert_eq!(parse_prefix("x").unwrap(), b'x');
        assert!(parse_prefix("c-1").is_err());
    }
}
