use percent_encoding::percent_decode_str;

const MARKER: &[u8] = b"\x1b]7;";
const MAX_SEQ: usize = 4096;
/// 提示符猜测用的输出尾部窗口（字节）。
const TAIL_KEEP: usize = 256;

/// 增量解析 OSC 7（`ESC ] 7 ; file://host/path BEL|ST`）里的远端 cwd。
/// 行为与 Python 版 `Osc7Tracker` 一致。
///
/// 另有提示符回退：`su` / `su -` 切换用户后的子 shell 没有钩子、不再上报
/// OSC 7，此时从 bash 默认提示符（`[user@host ~]$` 或 `user@host:~$`）
/// 解析当前目录作为回退来源——`~` 依据 /etc/passwd（SFTP 读一次）或
/// /home/<user> 惯例解析成绝对路径。远端零注入。
#[derive(Debug, Default)]
pub struct Osc7Tracker {
    buf: Vec<u8>,
    cwd: Option<String>,
    /// OSC 7 生效时的提示符事件序号。
    cwd_seq: u64,
    /// 远端输出尾部窗口（原始字节），供提示符解析。
    tail: Vec<u8>,
    /// 提示符解析出的 cwd（`\w` 完整路径时可定位，含 `~`）；`\W` 短名时为 None。
    guess: Option<String>,
    /// 提示符猜测生效时的提示符事件序号。
    guess_seq: u64,
    /// 最近一次提示符里的用户名（su 切用户后的 /proc 探测目标）。
    guess_user: Option<String>,
    /// exec 通道探测到的精确 cwd 及其探测时的提示符事件序号。
    exec_cwd: Option<(String, u64)>,
    /// /etc/passwd 的 user -> home 缓存。
    homes: Vec<(String, String)>,
    /// 仅 OSC 7 递增（note_prompt 触发用）。
    version: u64,
    /// 提示符事件时钟：OSC 7 与提示符 sighting 都递增，新者胜。
    seq: u64,
}

impl Osc7Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<String> {
        let mut buf = std::mem::take(&mut self.buf);
        buf.extend_from_slice(data);

        let mut found = Vec::new();
        let mut i = 0usize;
        let keep: usize;

        loop {
            let start = match find(&buf, MARKER, i) {
                Some(p) => p,
                None => {
                    let max = (MARKER.len() - 1).min(buf.len());
                    let mut k = 0usize;
                    for ln in (1..=max).rev() {
                        if buf.ends_with(&MARKER[..ln]) {
                            k = ln;
                            break;
                        }
                    }
                    keep = buf.len().saturating_sub(k).max(i);
                    break;
                }
            };

            let mut term: Option<(usize, usize)> = None;
            if let Some(p) = find(&buf, b"\x07", start) {
                term = Some((p, 1));
            }
            if let Some(p) = find(&buf, b"\x1b\\", start) {
                if term.is_none_or(|(q, _)| p < q) {
                    term = Some((p, 2));
                }
            }

            match term {
                None => {
                    if buf.len() - start > MAX_SEQ {
                        i = start + MARKER.len();
                        continue;
                    }
                    keep = start;
                    break;
                }
                Some((end, tlen)) => {
                    if let Some(path) = parse(&buf[start + MARKER.len()..end]) {
                        found.push(path);
                    }
                    i = end + tlen;
                }
            }
        }

        self.buf.extend_from_slice(&buf[keep..]);
        if let Some(last) = found.last() {
            self.cwd = Some(last.clone());
            self.seq += 1;
            self.cwd_seq = self.seq;
            self.version += 1;
        }

        // 提示符回退：本批没有 OSC 7 时，尝试从输出尾部解析 bash 提示符。
        // su 切用户后的子 shell 每次提示符都比 root 最后一次 OSC 7 更新，
        // 因此以「提示符事件序号」最新者为准，而非 OSC 7 永远优先。
        if found.is_empty() {
            self.tail.extend_from_slice(data);
            if self.tail.len() > TAIL_KEEP {
                let cut = self.tail.len() - TAIL_KEEP;
                self.tail.drain(..cut);
            }
            let stripped = strip_ansi(&self.tail);
            if let Some((path, user)) = prompt_hint(&stripped) {
                self.seq += 1;
                self.guess_user = Some(user);
                // \w 完整路径（/abs 或 ~ 开头）可直接定位；\W 短名只提供用户名，
                // 交给 /proc 探测拿精确 cwd
                if path.starts_with('/') || path.starts_with('~') {
                    self.guess = Some(path);
                } else {
                    self.guess = None;
                }
                self.guess_seq = self.seq;
                self.exec_cwd = None; // 新提示符事件使探测缓存失效
            }
        } else {
            self.tail.clear();
        }
        found
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// 远端当前目录（绝对路径）：OSC 7 与提示符猜测按最新提示符事件取优先，
    /// 猜测中的 `~` 按提示符用户解析成家目录。
    pub fn remote_cwd(&self) -> Option<String> {
        if self.guess_seq > self.cwd_seq {
            let (path, user) = (self.guess.as_deref()?, self.guess_user.as_deref()?);
            return self.resolve_tilde(path, user);
        }
        self.cwd.clone()
    }

    /// 提示符事件时钟。
    pub fn prompt_seq(&self) -> u64 {
        self.seq
    }

    /// 当前生效来源是提示符（而非 OSC 7）→ 需要 exec 探测精确 cwd。
    pub fn needs_probe(&self) -> bool {
        self.guess_seq > self.cwd_seq
    }

    /// 最近提示符里的用户名。
    pub fn guess_user(&self) -> Option<&str> {
        self.guess_user.as_deref()
    }

    /// 探测缓存是否对当前提示符事件仍有效。
    pub fn exec_fresh(&self) -> Option<String> {
        self.exec_cwd
            .as_ref()
            .filter(|(_, s)| *s == self.seq)
            .map(|(c, _)| c.clone())
    }

    /// 缓存一次探测结果（seq 为探测开始时的提示符事件序号）。
    pub fn store_exec(&mut self, cwd: String, seq: u64) {
        self.exec_cwd = Some((cwd, seq));
    }

    fn resolve_tilde(&self, path: &str, user: &str) -> Option<String> {
        let Some(rest) = path.strip_prefix('~') else {
            return Some(path.to_string());
        };
        let (target, sub) = if let Some(s) = rest.strip_prefix('/') {
            (user, s)
        } else if rest.is_empty() {
            (user, "")
        } else {
            // ~user 或 ~user/sub：显式指定用户
            match rest.split_once('/') {
                Some((u, s)) => (u, s),
                None => (rest, ""),
            }
        };
        if target.is_empty() {
            return None;
        }
        let home = self.home_of(target)?;
        Some(if sub.is_empty() {
            home
        } else {
            format!("{}/{}", home.trim_end_matches('/'), sub)
        })
    }

    fn home_of(&self, user: &str) -> Option<String> {
        if let Some((_, h)) = self.homes.iter().find(|(u, _)| u == user) {
            return Some(h.clone());
        }
        if user == "root" {
            Some("/root".to_string())
        } else {
            Some(format!("/home/{user}"))
        }
    }

    /// 注入 /etc/passwd 解析出的 user -> home 映射（`~` 解析用）。
    pub fn set_homes(&mut self, homes: Vec<(String, String)>) {
        self.homes = homes;
    }

    pub fn version(&self) -> u64 {
        self.version
    }
}

/// /etc/passwd 内容 → (user, home) 列表。
pub fn parse_passwd(data: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(data)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() >= 6 && !f[0].is_empty() && f[5].starts_with('/') {
                Some((f[0].to_string(), f[5].to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// 剥掉 ANSI 转义序列（CSI / OSC），便于提示符匹配。
fn strip_ansi(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            0x1b if i + 1 < data.len() && data[i + 1] == b'[' => {
                i += 2;
                while i < data.len() && !(0x40..=0x7e).contains(&data[i]) {
                    i += 1;
                }
                i += 1; // 跳过终止字节；序列不完整则丢弃尾部
            }
            0x1b if i + 1 < data.len() && data[i + 1] == b']' => {
                let mut j = i + 2;
                while j < data.len() {
                    if data[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if data[j] == 0x1b && j + 1 < data.len() && data[j + 1] == b'\\' {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
            }
            0x1b => i += 1,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// 从（已去转义的）输出尾部解析 bash 提示符，返回 (路径, 用户)。
/// 仅接受 `\w` 形态（`/abs`、`~`、`~/sub`、`~user`）；`\W` 短名无法定位，忽略。
fn prompt_hint(s: &[u8]) -> Option<(String, String)> {
    let mut end = s.len();
    while end > 0 && matches!(s[end - 1], b' ' | b'\r') {
        end -= 1;
    }
    if end < 4 {
        return None;
    }
    let last = s[end - 1];
    if last != b'$' && last != b'#' {
        return None;
    }
    let line_start = s[..end - 1]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |p| p + 1);
    let line = &s[line_start..end - 1];
    if line.len() < 3 || line.len() > 160 {
        return None;
    }

    let (who, path): (&str, &str) = if line.last() == Some(&b']') {
        // [user@host path]
        let open = line.iter().rposition(|&b| b == b'[')?;
        let content = std::str::from_utf8(&line[open + 1..line.len() - 1]).ok()?;
        content.split_once(' ')?
    } else if let Some(colon) = line.iter().rposition(|&b| b == b':') {
        // user@host:path
        let who = std::str::from_utf8(&line[..colon]).ok()?;
        let path = std::str::from_utf8(&line[colon + 1..]).ok()?;
        (who, path)
    } else {
        return None;
    };
    if who.is_empty() || path.is_empty() || who.contains(' ') || !who.contains('@') {
        return None;
    }
    let user = who.split('@').next()?;
    if user.is_empty() {
        return None;
    }
    let path = path.trim();
    // 路径形态（/abs、~、\W 短名）由调用方区分强弱，这里只要求非空
    Some((path.to_string(), user.to_string()))
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn parse(payload: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(payload);
    let rest = text.strip_prefix("file://")?;
    let path = match rest.find('/') {
        Some(i) => &rest[i..],
        None => return Some("/".to_string()),
    };
    if !path.starts_with('/') {
        return None;
    }
    Some(percent_decode_str(path).decode_utf8_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_and_chunked() {
        let mut t = Osc7Tracker::new();
        assert_eq!(t.feed(b"\x1b]7;file://host/root\x07"), ["/root"]);
        assert_eq!(t.cwd(), Some("/root"));
        let mut t2 = Osc7Tracker::new();
        t2.feed(b"x\x1b]7;file://h/tm");
        t2.feed(b"p/a%20b\x1b\\");
        assert_eq!(t2.cwd(), Some("/tmp/a b"));
        assert_eq!(t2.version(), 1);
    }

    #[test]
    fn last_wins_and_ignores_noise() {
        let mut t = Osc7Tracker::new();
        t.feed(b"\x1b]7;file://h/a\x07noise\x1b]7;file://h/b\x07");
        assert_eq!(t.cwd(), Some("/b"));
        assert_eq!(t.feed(b"nothing here"), Vec::<String>::new());
        assert_eq!(t.feed(b"\x1b]7;http://h/x\x07"), Vec::<String>::new());
    }

    #[test]
    fn split_marker_across_feeds() {
        let mut t = Osc7Tracker::new();
        assert!(t.feed(b"abc\x1b]").is_empty());
        assert!(t.feed(b"7;file://h/a b").is_empty());
        assert_eq!(t.feed(b"\x07"), ["/a b"]);
    }

    #[test]
    fn prompt_guess_bracket_form() {
        let mut t = Osc7Tracker::new();
        t.feed(b"some output\r\n[omm@gauss1 ~]$ ");
        assert_eq!(t.remote_cwd(), Some("/home/omm".to_string()));
        // cd 后的绝对路径
        t.feed(b"\r\n[omm@gauss1 /data/gaussdb_tmp]$ ");
        assert_eq!(t.remote_cwd(), Some("/data/gaussdb_tmp".to_string()));
        // \W 短名无法定位：清掉旧猜测，转由 /proc 探测兜底
        t.feed(b"\r\n[omm@gauss1 tmp]$ ");
        assert_eq!(t.remote_cwd(), None);
        assert!(t.needs_probe());
        assert_eq!(t.guess_user(), Some("omm"));
    }

    #[test]
    fn prompt_guess_colon_form_and_tilde_user() {
        let mut t = Osc7Tracker::new();
        t.feed(b"\r\nroot@gauss1:~# ");
        assert_eq!(t.remote_cwd(), Some("/root".to_string()));
        t.feed(b"\r\nomm@gauss1:~ommx$ ");
        assert_eq!(t.remote_cwd(), Some("/home/ommx".to_string()));
    }

    #[test]
    fn prompt_guess_chunked_and_colored() {
        let mut t = Osc7Tracker::new();
        t.feed(b"\r\n\x1b[01;32m[omm@gauss1 ~]\x1b[0m$ ");
        assert_eq!(t.remote_cwd(), Some("/home/omm".to_string()));
        // 提示符跨批到达
        let mut t2 = Osc7Tracker::new();
        t2.feed(b"\r\n[omm@gauss1 ~");
        // 还没收完，猜测已可产生（路径段已完整）或等待；收完 \$ 后必须成功
        t2.feed(b"]$ ");
        assert_eq!(t2.remote_cwd(), Some("/home/omm".to_string()));
    }

    #[test]
    fn osc7_overrides_prompt_guess() {
        let mut t = Osc7Tracker::new();
        t.feed(b"[omm@gauss1 ~]$ ");
        assert_eq!(t.remote_cwd(), Some("/home/omm".to_string()));
        t.feed(b"\x1b]7;file://h/real\x07\r\n[root@gauss1 ~]# ");
        assert_eq!(t.remote_cwd(), Some("/real".to_string()));
        // su 回 omm：新的无 OSC7 提示符重新生效
        t.feed(b"\r\n[omm@gauss1 /var]$ ");
        assert_eq!(t.remote_cwd(), Some("/var".to_string()));
    }

    #[test]
    fn homes_from_passwd() {
        let mut t = Osc7Tracker::new();
        t.set_homes(parse_passwd(b"root:x:0:0:root:/root:/bin/bash\nomm:x:1000:1000::/home/omm:/bin/bash\n"));
        t.feed(b"[omm@gauss1 ~]$ ");
        assert_eq!(t.remote_cwd(), Some("/home/omm".to_string()));
        t.feed(b"\r\n[root@gauss1 ~]# ");
        assert_eq!(t.remote_cwd(), Some("/root".to_string()));
    }

    #[test]
    fn command_output_tail_not_prompt() {
        let mut t = Osc7Tracker::new();
        // 输出行以 [ ]$ 结尾但不是 user@host path 形态 → 不产生猜测
        t.feed(b"\r\n[2026-11-06 12:00:01 some log line]$ ");
        assert_eq!(t.remote_cwd(), None);
        // 时间戳里的冒号也不该被当成 host:path 形态
        t.feed(b"\r\n2026-11-06 12:00:01 user action$ ");
        assert_eq!(t.remote_cwd(), None);
    }
}
