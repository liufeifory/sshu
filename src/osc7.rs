use percent_encoding::percent_decode_str;

const MARKER: &[u8] = b"\x1b]7;";
const MAX_SEQ: usize = 4096;

/// 增量解析 OSC 7（`ESC ] 7 ; file://host/path BEL|ST`）里的远端 cwd。
/// 行为与 Python 版 `Osc7Tracker` 一致。
#[derive(Debug, Default)]
pub struct Osc7Tracker {
    buf: Vec<u8>,
    cwd: Option<String>,
    version: u64,
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
            self.version += 1;
        }
        found
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    pub fn version(&self) -> u64 {
        self.version
    }
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
}
