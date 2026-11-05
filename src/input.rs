use crate::debuglog;
use crate::tokenize::split_command_line;

/// magic 词：行首输入这些词（+空格或回车）时本地拦截。
/// put/get 是正式名，upload/download 保留为兼容别名；lcd/lll 是本地目录操作。
const MAGIC: [&[u8]; 6] = [b"put ", b"get ", b"lcd ", b"lll ", b"upload ", b"download "];
const MAGIC_WORDS: [&[u8]; 6] = [b"put", b"get", b"lcd", b"lll", b"upload", b"download"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Upload(Vec<String>),
    Download(Vec<String>),
    /// 切换本地工作目录；None = 显示当前目录。
    Lcd(Option<String>),
    /// 本地目录长格式列表（等效 ls -l）。
    Lll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    ToRemote(Vec<u8>),
    Local(Vec<u8>),
    Run(Command),
    /// Cmd 模式下按 Tab：请求对当前行做补全（本地路径 / SFTP 远端路径）。
    Complete(String),
}

/// Tab 补全结果（由外部计算——远端路径需要异步 SFTP 列目录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// 唯一匹配或候选公共前缀：整行替换为该值，新增部分本地回显。
    Line(String),
    /// 有歧义：先列出候选，再回到当前行继续输入。
    List { line: String, names: Vec<String> },
    /// 无匹配，忽略。
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pass,
    Cmd,
}

/// 交互输入状态机：把控制台字节流翻译成「发给远端 / 本地显示 / 执行命令」。
pub struct InputState {
    prefix: u8,
    magic: bool,
    mode: Mode,
    /// 远端 shell 当前行里、由本端透传出去的可见字符数。
    /// `Some(0)` = 行首（提示符刚刷新 / 刚回车 / 已删光）；
    /// `None` = 行内容不可知（光标键、Tab 补全等），此时不做 magic 拦截。
    line_len: Option<usize>,
    pre: Option<Vec<u8>>,
    /// 是否对当前 pre 做了本地回显（仅在确认处于 bash 提示符时回显，
    /// 避免把密码首字符打到屏幕上）。
    pre_echo: bool,
    /// 已本地回显的 pre 字节数，冲刷时需要先擦除。
    pre_echoed: usize,
    /// 本行内是否见过 OSC7 提示符（回显门控）。
    prompt_seen: bool,
    alt_screen: bool,
    line: String,
    auto_return: bool,
    utf8: Vec<u8>,
    esc: Vec<u8>,
}

impl InputState {
    pub fn new(prefix: u8, magic: bool) -> Self {
        Self {
            prefix,
            magic,
            mode: Mode::Pass,
            line_len: Some(0),
            pre: None,
            pre_echo: false,
            pre_echoed: 0,
            prompt_seen: false,
            alt_screen: false,
            line: String::new(),
            auto_return: false,
            utf8: Vec::new(),
            esc: Vec::new(),
        }
    }

    pub fn set_alt_screen(&mut self, on: bool) {
        debuglog::log("state", &format!("alt_screen={on}"));
        self.alt_screen = on;
    }

    pub fn note_prompt(&mut self) {
        debuglog::log("state", "osc7 prompt -> line_len=Some(0)");
        self.line_len = Some(0);
        self.prompt_seen = true;
    }

    pub fn in_cmd_mode(&self) -> bool {
        self.mode == Mode::Cmd
    }

    /// 行首判断：远端当前行没有未处理的可见输入。
    fn at_line_start(&self) -> bool {
        self.line_len == Some(0)
    }

    /// 字节被透传给远端后，同步维护行字符数。
    fn forwarded(&mut self, bytes: &[u8]) {
        let before = self.line_len;
        for &b in bytes {
            match b {
                0x0d | 0x0a => {
                    self.line_len = Some(0);
                    // 回车后到下一个提示符之间（sudo 密码等）不回显。
                    self.prompt_seen = false;
                }
                // 退格 / Ctrl-H：删一个字符；删光后恢复行首。
                0x7f | 0x08 => self.line_len = self.line_len.map(|n| n.saturating_sub(1)),
                // Ctrl-U：readline 清空整行。
                0x15 => self.line_len = Some(0),
                // 其余控制字节（光标键、Tab、Ctrl-C 等）会让行内容不可知。
                b if b < 0x20 => self.line_len = None,
                // UTF-8 续字节不增加字符数（与退格按字符删除配对）。
                b if b & 0xC0 == 0x80 => {}
                _ => self.line_len = self.line_len.map(|n| n + 1),
            }
        }
        if before != self.line_len {
            debuglog::log("state", &format!("forwarded {} bytes, line_len {before:?} -> {:?}", bytes.len(), self.line_len));
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Action> {
        let filtered = filter_events(self, chunk);
        if filtered.is_empty() {
            return Vec::new();
        }
        let chunk = filtered.as_slice();
        let mut out = Vec::new();

        if self.mode == Mode::Cmd {
            for &b in chunk {
                self.cmd_key(b, &mut out);
            }
            return out;
        }

        let mut i = 0usize;
        while i < chunk.len() {
            let b = chunk[i];
            let magic_start = self.magic
                && self.at_line_start()
                && !self.alt_screen
                && MAGIC.iter().any(|w| w.starts_with(&chunk[i..i + 1]));
            if b == self.prefix || b < 0x20 || b == 0x7f || self.pre.is_some() || magic_start {
                break;
            }
            i += 1;
        }

        if i > 0 {
            out.push(Action::ToRemote(chunk[..i].to_vec()));
            self.forwarded(&chunk[..i]);
        }

        for &b in &chunk[i..] {
            if self.mode == Mode::Cmd {
                self.cmd_key(b, &mut out);
            } else {
                self.pass_key(b, &mut out);
            }
        }
        out
    }

    fn pass_key(&mut self, b: u8, out: &mut Vec<Action>) {
        if b == self.prefix {
            self.flush_pre(out);
            self.enter_cmd(out);
            return;
        }

        if let Some(pre) = self.pre.take() {
            let mut cand = pre;
            cand.push(b);
            if MAGIC.iter().any(|w| w.starts_with(&cand)) {
                if MAGIC.iter().any(|w| *w == cand.as_slice()) {
                    debuglog::log("magic", "complete magic word + space -> cmd mode");
                    self.line = String::from_utf8_lossy(&cand).into_owned();
                    self.mode = Mode::Cmd;
                    self.auto_return = true;
                    self.pre_echoed = 0;
                    // 补上触发词的那个空格的回显：前几个字符已逐字回显，
                    // 空格是判定字符，不补的话屏幕上是 "put/root/x" 这种缺空格的样子。
                    if self.pre_echo {
                        out.push(Action::Local(vec![b]));
                    }
                    // 不再打印 "(sshu) <词>"：提示符行上已本地回显该词，
                    // 参数就地续输，观感与 bash 一致；
                    // "(sshu) " 提示符仅在前缀键（Ctrl-]）进入时出现。
                    return;
                } else {
                    debuglog::log(
                        "magic",
                        &format!("accumulate pre={:?}", String::from_utf8_lossy(&cand)),
                    );
                    if self.echo_pre_byte(&cand[cand.len() - 1..], out) {
                        self.pre_echoed += 1;
                    }
                    self.pre = Some(cand);
                }
                return;
            }
            // 不带尾随空格直接回车（如 "put\r"）也按 magic 处理，
            // 避免整词漏给远端 bash 报 command not found。
            if matches!(b, 0x0d | 0x0a) && MAGIC_WORDS.contains(&&cand[..cand.len() - 1]) {
                debuglog::log("magic", "magic word + enter (no space) -> handle locally");
                self.line = String::from_utf8_lossy(&cand[..cand.len() - 1]).into_owned();
                self.mode = Mode::Cmd;
                self.auto_return = true;
                self.pre_echoed = 0;
                // 不再回显 "(sshu) <词>"：结果直接跟在提示符行下面。
                self.cmd_key(b, out);
                return;
            }
            self.flush_echo(out);
            debuglog::log_bytes("flush-pre", &cand);
            self.forwarded(&cand);
            out.push(Action::ToRemote(cand));
            return;
        }

        let can_start = self.magic
            && self.at_line_start()
            && !self.alt_screen
            && MAGIC.iter().any(|w| w.starts_with(&[b]));
        if can_start {
            debuglog::log("magic", "capture first byte (pending)");
            self.pre_echo = self.prompt_seen;
            self.pre_echoed = 0;
            self.pre = Some(vec![b]);
            if self.echo_pre_byte(&[b], out) {
                self.pre_echoed += 1;
            }
            return;
        }

        out.push(Action::ToRemote(vec![b]));
        self.forwarded(&[b]);
    }

    /// 本地回显 pre 里的字节（仅在确认过提示符时，防止密码首字符上屏）。
    /// 返回是否真的回显了（未回显时不得计入 pre_echoed，否则冲刷会多发擦除序列）。
    fn echo_pre_byte(&mut self, bytes: &[u8], out: &mut Vec<Action>) -> bool {
        if self.pre_echo {
            out.push(Action::Local(bytes.to_vec()));
            true
        } else {
            false
        }
    }

    /// 冲刷 pre 前擦除已回显的字符，远端回显会接上，视觉无缝。
    fn flush_echo(&mut self, out: &mut Vec<Action>) {
        if self.pre_echoed > 0 {
            let mut erase = Vec::with_capacity(self.pre_echoed * 3);
            for _ in 0..self.pre_echoed {
                erase.extend_from_slice(b"\x08 \x08");
            }
            out.push(Action::Local(erase));
        }
        self.pre_echoed = 0;
    }

    fn flush_pre(&mut self, out: &mut Vec<Action>) {
        if let Some(pre) = self.pre.take() {
            self.flush_echo(out);
            debuglog::log_bytes("flush-pre(prefix)", &pre);
            out.push(Action::ToRemote(pre.clone()));
            self.forwarded(&pre);
        }
    }

    fn enter_cmd(&mut self, out: &mut Vec<Action>) {
        debuglog::log("state", "prefix -> enter cmd mode");
        self.mode = Mode::Cmd;
        self.auto_return = false;
        self.line.clear();
        out.push(Action::Local(b"\r\n(sshu) ".to_vec()));
    }

    fn leave_cmd(&mut self, out: &mut Vec<Action>) {
        debuglog::log(
            "state",
            &format!("leave cmd mode, line_len={:?}", self.line_len),
        );
        self.mode = Mode::Pass;
        self.line.clear();
        self.auto_return = false;
        self.utf8.clear();
        // 只有远端当前行确认为空时才补发回车刷新提示符；
        // 行内容不可知时补发 \r 可能把远端未回车的输入误执行掉。
        if self.line_len == Some(0) && !self.alt_screen {
            out.push(Action::ToRemote(b"\r".to_vec()));
        }
    }

    fn after_cmd(&mut self, out: &mut Vec<Action>) {
        if self.auto_return {
            self.leave_cmd(out);
            return;
        }
        self.mode = Mode::Cmd;
        self.line.clear();
        out.push(Action::Local(b"(sshu) ".to_vec()));
    }

    fn cmd_key(&mut self, b: u8, out: &mut Vec<Action>) {
        let Some(ch) = self.decode(b) else {
            return;
        };
        match ch {
            '\r' | '\n' => {
                out.push(Action::Local(b"\r\n".to_vec()));
                let line = std::mem::take(&mut self.line);
                self.utf8.clear();
                debuglog::log("cmd", &format!("submit line={line:?}"));
                if line.trim().is_empty() {
                    self.leave_cmd(out);
                    return;
                }
                match split_command_line(line.trim()) {
                    Ok(argv) => {
                        let cmd = argv.first().map(|s| s.to_ascii_lowercase());
                        let args: Vec<String> = argv.get(1..).unwrap_or(&[]).to_vec();
                        match cmd.as_deref() {
                            Some("put") | Some("upload") if !args.is_empty() => {
                                out.push(Action::Run(Command::Upload(args)))
                            }
                            Some("get") | Some("download") if !args.is_empty() => {
                                out.push(Action::Run(Command::Download(args)))
                            }
                            Some("put") | Some("upload") => out.push(msg(
                                "失败: 用法: put <本地路径> [远端路径]（以 / 结尾或已存在为目录；省略 = 远端 cwd）",
                            )),
                            Some("get") | Some("download") => out.push(msg(
                                "失败: 用法: get <远端路径> [本地路径]（以分隔符结尾或已存在为目录；省略 = 本地当前目录）",
                            )),
                            Some("lcd") => {
                                out.push(Action::Run(Command::Lcd(args.first().cloned())))
                            }
                            Some("lll") => out.push(Action::Run(Command::Lll)),
                            Some(other) => out.push(msg(&format!(
                                "未知命令: {other}（本客户端指令: put / get / lcd / lll，空行回车回到远端会话）"
                            ))),
                            None => {}
                        }
                    }
                    Err(e) => out.push(msg(&format!("失败: {e}"))),
                }
                self.after_cmd(out);
            }
            '\x03' => {
                out.push(Action::Local(b"^C\r\n(sshu) ".to_vec()));
                self.line.clear();
            }
            '\x7f' | '\x08' => {
                if self.line.pop().is_some() {
                    out.push(Action::Local(b"\x08 \x08".to_vec()));
                }
            }
            '\x15' => {
                while self.line.pop().is_some() {
                    out.push(Action::Local(b"\x08 \x08".to_vec()));
                }
            }
            '\x04' => {
                if self.line.is_empty() {
                    out.push(Action::Local(b"\r\n".to_vec()));
                    self.leave_cmd(out);
                }
            }
            '\t' => {
                debuglog::log("cmd", "tab -> request completion");
                let line = self.line.clone();
                out.push(Action::Complete(line));
            }
            c if (c as u32) < 32 => {}
            c => {
                self.line.push(c);
                let mut buf = [0u8; 4];
                out.push(Action::Local(c.encode_utf8(&mut buf).as_bytes().to_vec()));
            }
        }
    }

    /// 应用补全结果：追加部分本地回显；有歧义时先列候选再回到当前行。
    pub fn apply_completion(&mut self, c: Completion, out: &mut Vec<Action>) {
        let (new_line, names) = match c {
            Completion::None => return,
            Completion::Line(l) => (l, Vec::new()),
            Completion::List { line, names } => (line, names),
        };
        debuglog::log(
            "cmd",
            &format!("completion -> {:?}, {} candidates", new_line, names.len()),
        );
        if !names.is_empty() {
            let mut text = b"\r\n".to_vec();
            text.extend_from_slice(names.join("  ").as_bytes());
            text.extend_from_slice(b"\r\n");
            if !self.auto_return {
                text.extend_from_slice(b"(sshu) ");
            }
            text.extend_from_slice(new_line.as_bytes());
            out.push(Action::Local(text));
        }
        if !new_line.starts_with(self.line.as_str()) {
            // 整行替换（如大小写不同）：先擦掉已回显内容再重打。
            let count = self.line.chars().count();
            for _ in 0..count {
                out.push(Action::Local(b"\x08 \x08".to_vec()));
            }
            out.push(Action::Local(new_line.clone().into_bytes()));
        } else if new_line.len() > self.line.len() {
            let suffix = new_line[self.line.len()..].to_string();
            out.push(Action::Local(suffix.into_bytes()));
        }
        self.line = new_line;
    }

    fn decode(&mut self, b: u8) -> Option<char> {        self.utf8.push(b);
        match std::str::from_utf8(&self.utf8) {
            Ok(s) => {
                let c = s.chars().next();
                self.utf8.clear();
                c
            }
            Err(e) if e.error_len().is_none() => None,
            Err(_) => {
                self.utf8.clear();
                Some('\u{fffd}')
            }
        }
    }
}

    /// 丢弃终端上报的焦点/鼠标事件，避免它们被当成键盘输入转发给远端，
    /// 也避免它们把 magic 所需的行首状态清掉。
    fn filter_events(state: &mut InputState, chunk: &[u8]) -> Vec<u8> {
        let mut data = std::mem::take(&mut state.esc);
        data.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0usize;
        while i < data.len() {
            if data[i] != 0x1b {
                out.push(data[i]);
                i += 1;
                continue;
            }
            match parse_term_event(&data[i..]) {
                EventParse::Complete(len) => i += len,
                EventParse::Incomplete => {
                    state.esc = data[i..].to_vec();
                    return out;
                }
                EventParse::NotEvent => {
                    out.push(0x1b);
                    i += 1;
                }
            }
        }
        strip_literal(&mut out, b"[200~");
        strip_literal(&mut out, b"[201~");
        out
    }

const PASTE_START: &[u8] = &[0x1b, b'[', b'2', b'0', b'0', b'~'];
const PASTE_END: &[u8] = &[0x1b, b'[', b'2', b'0', b'1', b'~'];

enum EventParse {
    Complete(usize),
    Incomplete,
    NotEvent,
}

fn strip_literal(out: &mut Vec<u8>, pat: &[u8]) {
    let mut i = 0;
    while i + pat.len() <= out.len() {
        if &out[i..i + pat.len()] == pat {
            out.drain(i..i + pat.len());
        } else {
            i += 1;
        }
    }
}

fn parse_term_event(b: &[u8]) -> EventParse {
    if b.len() < 2 {
        return EventParse::Incomplete;
    }
    if b[1] != b'[' {
        return EventParse::NotEvent;
    }
    if b.len() < 3 {
        return EventParse::Incomplete;
    }
    if b.len() < 6 && (PASTE_START.starts_with(b) || PASTE_END.starts_with(b)) {
        return EventParse::Incomplete;
    }
    if b.len() >= 6 && (&b[..6] == PASTE_START || &b[..6] == PASTE_END) {
        return EventParse::Complete(6);
    }
    match b[2] {
        b'I' | b'O' => EventParse::Complete(3),
        b'<' => {
            for (j, &c) in b.iter().enumerate().skip(3) {
                if c == b'M' || c == b'm' {
                    return EventParse::Complete(j + 1);
                }
                if j > 64 {
                    return EventParse::NotEvent;
                }
            }
            EventParse::Incomplete
        }
        b'M' => {
            if b.len() >= 6 {
                EventParse::Complete(6)
            } else {
                EventParse::Incomplete
            }
        }
        _ => EventParse::NotEvent,
    }
}


fn msg(text: &str) -> Action {
    // 调用点（回车提交）已输出过换行，这里不再前置空行。
    Action::Local(text.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_run(actions: &[Action], pred: impl Fn(&Command) -> bool) -> bool {
        actions.iter().any(|a| matches!(a, Action::Run(c) if pred(c)))
    }

    #[test]
    fn normal_passthrough() {
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"ls -la\r");
        let sent: Vec<u8> = a
            .iter()
            .filter_map(|x| match x {
                Action::ToRemote(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(sent, b"ls -la\r");
        assert!(a.iter().all(|x| matches!(x, Action::ToRemote(_))));
    }

    #[test]
    fn magic_put_no_leak() {
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"put ");
        assert!(s.in_cmd_mode());
        assert!(a.iter().all(|x| !matches!(x, Action::ToRemote(_))));

        let b = s.feed(b"C:\\tmp\\a.txt\r");
        assert!(has_run(&b, |c| matches!(c, Command::Upload(v) if v == &["C:\\tmp\\a.txt"])));
        assert!(b
            .iter()
            .all(|x| !matches!(x, Action::ToRemote(v) if v.windows(3).any(|w| w == b"put"))));
        assert!(!s.in_cmd_mode());
        assert!(b.iter().any(|x| matches!(x, Action::ToRemote(v) if v == b"\r")));
    }

    #[test]
    fn prefix_mode_not_leaked() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        assert!(s.in_cmd_mode());
        let a = s.feed(b"get /x\r");
        assert!(has_run(&a, |c| matches!(c, Command::Download(v) if v == &["/x"])));
        assert!(!a.iter().any(|x| matches!(x, Action::ToRemote(_))));
        assert!(s.in_cmd_mode());
    }

    #[test]
    fn empty_enter_leaves() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        let a = s.feed(b"\r");
        assert!(!s.in_cmd_mode());
        assert!(a.iter().any(|x| matches!(x, Action::ToRemote(v) if v == b"\r")));
    }

    #[test]
    fn backspace_edits_line() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        let a = s.feed(b"putx\x7f u\r");
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["u"])));
    }

    #[test]
    fn utf8_command_line() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        let line = "put /tmp/中文 名称.txt\r".as_bytes().to_vec();
        let a = s.feed(&line);
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["/tmp/中文", "名称.txt"])));
    }

    #[test]
    fn alt_screen_disables_magic() {
        let mut s = InputState::new(0x1d, true);
        s.set_alt_screen(true);
        assert_eq!(
            s.feed(b"put "),
            vec![Action::ToRemote(b"put ".to_vec())]
        );
    }

    #[test]
    fn note_prompt_resets_fresh() {
        let mut s = InputState::new(0x1d, true);
        s.feed(b"x");
        s.note_prompt();
        let a = s.feed(b"put ");
        assert!(s.in_cmd_mode());
        assert!(a.iter().all(|x| !matches!(x, Action::ToRemote(_))));
    }

    #[test]
    fn backspace_to_empty_restores_magic() {
        // 手打一半后删光再输入 upload：必须重新拦截，不能透传给 bash。
        let mut s = InputState::new(0x1d, true);
        s.feed(b"ab");
        s.feed(b"\x7f\x7f");
        let a = s.feed(b"put C:\\x.txt\r");
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["C:\\x.txt"])));
        let leaked: Vec<u8> = a
            .iter()
            .filter_map(|x| match x {
                Action::ToRemote(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(!leaked.windows(3).any(|w| w == b"put"), "{leaked:?}");
    }

    #[test]
    fn partial_backspace_still_blocks_magic() {
        // 行上还留着字符时不拦截（避免吞掉用户已输入的内容）。
        let mut s = InputState::new(0x1d, true);
        let mut all = s.feed(b"abc");
        all.extend(s.feed(b"\x7f"));
        all.extend(s.feed(b"put x\r"));
        assert!(!s.in_cmd_mode());
        assert_eq!(sent(&all), b"abc\x7fput x\r");
    }

    #[test]
    fn ctrl_u_clears_line_restores_magic() {
        let mut s = InputState::new(0x1d, true);
        s.feed(b"abc");
        s.feed(&[0x15]);
        let a = s.feed(b"put C:\\y.txt\r");
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["C:\\y.txt"])));
    }

    #[test]
    fn enter_restores_magic() {
        let mut s = InputState::new(0x1d, true);
        s.feed(b"ls\r");
        let a = s.feed(b"put x\r");
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["x"])));
    }

    #[test]
    fn arrow_keys_keep_line_unknown() {
        // ↑ 之后行内容不可知（可能是历史命令），必须继续透传不能误吞。
        let mut s = InputState::new(0x1d, true);
        let mut all = s.feed(b"\x1b[A");
        all.extend(s.feed(b"put x\r"));
        assert!(!s.in_cmd_mode());
        assert_eq!(sent(&all), b"\x1b[Aput x\r");
    }

    #[test]
    fn tab_marks_line_unknown() {
        let mut s = InputState::new(0x1d, true);
        let mut all = s.feed(b"upl\t");
        all.extend(s.feed(b"oad x\r"));
        assert!(!s.in_cmd_mode());
        assert_eq!(sent(&all), b"upl\toad x\r");
    }

    #[test]
    fn magic_word_enter_without_space_intercepted() {
        // "upload" + 回车（不带空格）：本地提示用法，不再漏给 bash。
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"put\r");
        assert!(!s.in_cmd_mode());
        let leaked: Vec<u8> = a
            .iter()
            .filter_map(|x| match x {
                Action::ToRemote(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(leaked, b"\r"); // 只补发刷新提示符的回车
        let echoed: Vec<u8> = a
            .iter()
            .filter_map(|x| match x {
                Action::Local(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        let echoed = String::from_utf8_lossy(&echoed).to_string();
        assert!(!echoed.contains("(sshu)"), "{echoed:?}");
        assert!(echoed.contains("用法"), "{echoed:?}");
    }

    #[test]
    fn magic_word_get_enter_without_space_intercepted() {
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"get\r");
        assert!(!s.in_cmd_mode());
        let leaked: Vec<u8> = a
            .iter()
            .filter_map(|x| match x {
                Action::ToRemote(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(leaked, b"\r");
    }

    #[test]
    fn incomplete_word_enter_still_flushed() {
        // "uploa" + 回车：不完整的词必须透传（bash 会报 uploa: 未找到命令）。
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"uploa\r");
        assert!(!s.in_cmd_mode());
        assert_eq!(sent(&a), b"uploa\r");
    }

    #[test]
    fn magic_word_enter_then_args_works() {
        // "upload" + 回车（用法提示）之后，重新输入完整命令仍正常。
        let mut s = InputState::new(0x1d, true);
        s.feed(b"put\r");
        let a = s.feed(b"put /tmp/a.txt\r");
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["/tmp/a.txt"])));
    }

    #[test]
    fn legacy_upload_download_aliases_still_intercepted() {
        // 旧名 upload/download 仍作为兼容别名被 magic 拦截并分发。
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"upload /tmp/a.txt\r");
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["/tmp/a.txt"])));

        let mut s2 = InputState::new(0x1d, true);
        let b = s2.feed(b"download /r.txt\r");
        assert!(has_run(&b, |c| matches!(c, Command::Download(v) if v == &["/r.txt"])));
    }

    #[test]
    fn lcd_and_lll_commands() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        let a = s.feed(b"lcd D:\\tmp\r");
        assert!(has_run(&a, |c| matches!(c, Command::Lcd(Some(d)) if d == "D:\\tmp")));

        let b = s.feed(b"lcd\r");
        assert!(has_run(&b, |c| matches!(c, Command::Lcd(None))));

        let c = s.feed(b"lll\r");
        assert!(has_run(&c, |c| matches!(c, Command::Lll)));
    }

    #[test]
    fn lcd_lll_magic_at_prompt() {
        // 提示符下直接敲 lcd / lll 也走 magic 拦截。
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"lcd /tmp\r");
        assert!(has_run(&a, |c| matches!(c, Command::Lcd(Some(d)) if d == "/tmp")));
        // 只允许补发刷新提示符的回车
        assert_eq!(sent(&a), b"\r");
        assert!(a.iter().any(|x| matches!(x, Action::Local(_))));

        let mut s2 = InputState::new(0x1d, true);
        let b = s2.feed(b"lll\r");
        assert!(has_run(&b, |c| matches!(c, Command::Lll)));
        assert_eq!(sent(&b), b"\r");
    }

    #[test]
    fn no_echo_no_erase() {
        // 未见过提示符时 pre 不回显，冲刷时也不得多发擦除序列。
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"ls -la\r");
        assert_eq!(sent(&a), b"ls -la\r");
        assert!(a.iter().all(|x| matches!(x, Action::ToRemote(_))));
    }

    #[test]
    fn tab_in_cmd_mode_requests_completion() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        let a = s.feed(b"get /ro\t");
        assert!(a
            .iter()
            .any(|x| matches!(x, Action::Complete(l) if l == "get /ro")));
        // Tab 不发给远端
        assert!(a.iter().all(|x| !matches!(x, Action::ToRemote(_))));
    }

    #[test]
    fn apply_completion_appends_suffix() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        let _ = s.feed(b"get /ro");
        let mut out = Vec::new();
        s.apply_completion(Completion::Line("get /root/".to_string()), &mut out);
        // 本地回显补上的 "ot/"
        assert_eq!(locals(&out), b"ot/");
        // 行缓冲已更新
        let b = s.feed(b"\r");
        assert!(has_run(&b, |c| matches!(c, Command::Download(v) if v == &["/root/"])));
    }

    #[test]
    fn apply_completion_list_reprompts() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        let _ = s.feed(b"get /r");
        let mut out = Vec::new();
        s.apply_completion(
            Completion::List {
                line: "get /r".to_string(),
                names: vec!["root/".to_string(), "run/".to_string()],
            },
            &mut out,
        );
        let text = String::from_utf8_lossy(&locals(&out)).to_string();
        assert!(text.contains("root/  run/"), "{text:?}");
        // 前缀模式（Ctrl-] 进入）列完候选后重打 (sshu) 提示符和当前行
        assert!(text.contains("(sshu) get /r"), "{text:?}");
    }

    #[test]
    fn tab_in_pass_mode_still_forwarded() {
        // 普通透传模式的 Tab 仍归远端 bash 补全。
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"ls \t");
        assert_eq!(sent(&a), b"ls \t");
    }

    fn locals(actions: &[Action]) -> Vec<u8> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Local(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn pre_first_byte_echoed_after_prompt() {
        // 提示符下敲 u：立即本地回显，不再"隐形"。
        let mut s = InputState::new(0x1d, true);
        s.note_prompt();
        let a = s.feed(b"u");
        assert_eq!(locals(&a), b"u");
        assert!(sent(&a).is_empty());
    }

    #[test]
    fn flush_erases_local_echo_first() {
        // "u" 后跟非 magic 字符：先擦掉回显的 u，再透传，远端回显接上。
        let mut s = InputState::new(0x1d, true);
        s.note_prompt();
        let _ = s.feed(b"u");
        let a = s.feed(b"n");
        assert_eq!(locals(&a), b"\x08 \x08");
        assert_eq!(sent(&a), b"un");
    }

    #[test]
    fn no_echo_before_first_prompt() {
        // 未见过 OSC7 提示符（如远端密码输入）不回显，防密码首字符上屏。
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"u");
        assert!(locals(&a).is_empty());
        assert!(sent(&a).is_empty());
    }

    #[test]
    fn no_echo_after_enter_without_new_prompt() {
        // 回车后未见新提示符（sudo 密码等）同样不回显。
        let mut s = InputState::new(0x1d, true);
        s.note_prompt();
        let _ = s.feed(b"sudo x\r");
        let a = s.feed(b"u");
        assert!(locals(&a).is_empty());
    }

    #[test]
    fn dangling_pre_persists_until_next_key() {
        // 不再有空闲冲刷：敲 u 后无论停多久都不透传，
        // 慢速打字也能凑齐 magic 词；下个键到来时才冲刷。
        let mut s = InputState::new(0x1d, true);
        s.note_prompt();
        let _ = s.feed(b"u");
        assert_eq!(s.pre.as_deref(), Some(b"u".as_slice()));
        // 继续慢速敲完整词（键间隔任意长）→ 拦截
        let mut all = Vec::new();
        for ch in b"pload " {
            all.extend(s.feed(&[*ch]));
        }
        assert!(s.in_cmd_mode());
        assert!(all.iter().all(|x| !matches!(x, Action::ToRemote(_))));
    }

    #[test]
    fn dangling_pre_flushed_by_unrelated_key() {
        // 悬空的 u 不会被丢：下一个不相关键把它带出去，行为与 bash 一致。
        let mut s = InputState::new(0x1d, true);
        s.note_prompt();
        let _ = s.feed(b"u");
        let a = s.feed(b"n");
        assert_eq!(locals(&a), b"\x08 \x08");
        assert_eq!(sent(&a), b"un");
    }

    fn sent(actions: &[Action]) -> Vec<u8> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::ToRemote(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn focus_event_does_not_break_magic() {
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"\x1b[I");
        assert!(a.is_empty(), "{a:?}");
        let b = s.feed(b"put ");
        assert!(s.in_cmd_mode());
        assert!(b.iter().all(|x| !matches!(x, Action::ToRemote(_))));
    }

    #[test]
    fn split_focus_event() {
        let mut s = InputState::new(0x1d, true);
        assert!(s.feed(b"\x1b[").is_empty());
        assert!(s.feed(b"I").is_empty());
        s.feed(b"put ");
        assert!(s.in_cmd_mode());
    }

    #[test]
    fn mouse_event_ignored() {
        let mut s = InputState::new(0x1d, true);
        assert!(s.feed(b"\x1b[<35;72;17M").is_empty());
        s.feed(b"put ");
        assert!(s.in_cmd_mode());
    }

    #[test]
    fn arrow_keys_still_forwarded() {
        let mut s = InputState::new(0x1d, true);
        let a = s.feed(b"\x1b[A");
        assert_eq!(sent(&a), b"\x1b[A");
    }

    #[test]
    fn bracketed_paste_in_cmd_mode() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        s.feed(b"put ");
        let a = s.feed(b"\x1b[200~C:\\x\\a.txt\x1b[201~\r");
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["C:\\x\\a.txt"])));
        let echoed: Vec<u8> = a
            .iter()
            .filter_map(|x| match x {
                Action::Local(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(!echoed.windows(5).any(|w| w == b"[200~"), "{echoed:?}");
        assert!(!echoed.windows(5).any(|w| w == b"[201~"), "{echoed:?}");
    }

    #[test]
    fn esc_less_paste_markers() {
        let mut s = InputState::new(0x01, true);
        s.feed(&[0x01]);
        s.feed(b"put ");
        let a = s.feed(b"[200~C:\\x\\a.txt[201~\r");
        assert!(has_run(&a, |c| matches!(c, Command::Upload(v) if v == &["C:\\x\\a.txt"])));
        let echoed: Vec<u8> = a
            .iter()
            .filter_map(|x| match x {
                Action::Local(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(!echoed.windows(5).any(|w| w == b"[200~"), "{echoed:?}");
        assert!(!echoed.windows(5).any(|w| w == b"[201~"), "{echoed:?}");
    }

    #[test]
    fn bracketed_paste_magic() {
        let mut s = InputState::new(0x1d, true);
        assert!(s.feed(b"\x1b[200~").is_empty());
        let a = s.feed(b"put ");
        assert!(s.in_cmd_mode());
        assert!(a.iter().all(|x| !matches!(x, Action::ToRemote(_))));
        let b = s.feed(b"/tmp/a.txt\x1b[201~\r");
        assert!(has_run(&b, |c| matches!(c, Command::Upload(v) if v == &["/tmp/a.txt"])));
    }
}
