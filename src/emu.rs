//! 终端模拟层：远端字节喂给 alacritty_terminal 的 VT 状态机，由本模块
//! 负责把网格重新渲染到控制台（"拥有屏幕"），状态栏单独占最后一行。
//!
//! 关键设计：
//! - 远端输出**不再原样透传**，而是先进入 Term 网格，再按行 diff 渲染成
//!   ANSI 序列写给本地终端。全屏应用（top/vim/less）的滚动区域、光标定位
//!   全部由网格状态决定，不再需要向字节流里注入控制序列。
//! - 打字回显（`Screen::echo`）是临时覆盖层：这些字符远端还不知道，不能进
//!   网格；远端重绘后由 `InputState::pending_echo` 重新画出来。
//! - 本地消息（put/get 结果、lll 列表等）走 `Screen::print`，喂进网格，
//!   因此会正常滚动、并被后续渲染保留。

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor};

/// 回滚历史行数（供 PgUp/PgDn 翻页查看）。
const HISTORY_LINES: usize = 10000;

/// 终端尺寸（屏幕行 + 回滚历史）。
#[derive(Clone, Copy)]
struct Dims {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        // 屏幕行 + 回滚历史：本机终端的历史由我们自己维护
        // （渲染接管屏幕后，conhost 的 scrollback 不再被填充）。
        self.screen_lines + HISTORY_LINES
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

pub struct Emu {
    term: Term<VoidListener>,
    processor: Processor,
    cols: usize,
    rows: usize,
}

impl Emu {
    pub fn new(cols: u16, rows: u16) -> Self {
        let dims = Dims {
            columns: cols.max(1) as usize,
            screen_lines: rows.max(1) as usize,
        };
        Self {
            term: Term::new(Config::default(), &dims, VoidListener),
            processor: Processor::new(),
            cols: dims.columns,
            rows: dims.screen_lines,
        }
    }

    /// 喂入字节，更新模拟器状态。
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// 是否处于全屏/接管模式（1049 alt screen 或 smkx 应用光标键）。
    pub fn fullscreen(&self) -> bool {
        let mode = self.term.mode();
        mode.contains(TermMode::ALT_SCREEN) || mode.contains(TermMode::APP_CURSOR)
    }

    /// 光标位置（行, 列），0 基。
    pub fn cursor(&self) -> (usize, usize) {
        let p = self.term.grid().cursor.point;
        (p.line.0.max(0) as usize, p.column.0)
    }

    pub fn cols(&self) -> u16 {
        self.cols as u16
    }

    pub fn rows(&self) -> u16 {
        self.rows as u16
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1) as usize;
        self.rows = rows.max(1) as usize;
        self.term.resize(Dims {
            columns: self.cols,
            screen_lines: self.rows,
        });
    }

    /// 当前可视偏移（>0 表示正在查看历史）。
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn scroll_display(&mut self, scroll: Scroll) {
        self.term.scroll_display(scroll);
    }

    /// 测试辅助：读出整行文本。
    pub fn cell_text(&self, row: usize) -> String {
        (0..self.cols).map(|c| self.cell(row, c).c).collect::<String>().trim_end().to_string()
    }

    fn cell(&self, row: usize, col: usize) -> Snap {
        // 渲染第 row 行时，若正在查看历史，实际取 Line(row - offset)。
        let line = row as i32 - self.term.grid().display_offset() as i32;
        let cell = &self.term.grid()[Line(line)][Column(col)];
        Snap {
            c: cell.c,
            fg: col_of(cell.fg),
            bg: col_of(cell.bg),
            flags: cell.flags.bits(),
        }
    }
}

/// 颜色（渲染用的紧凑表示；Default = 交给终端默认色）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Col {
    #[default]
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

fn col_of(c: Color) -> Col {
    match c {
        // 0..=15 是标准/亮色，用基础 SGR；其余（前景/背景/光标/暗色）走默认。
        Color::Named(n) => match n as u16 {
            0..=15 => Col::Idx(n as u8),
            _ => Col::Default,
        },
        Color::Indexed(i) => Col::Idx(i),
        Color::Spec(rgb) => Col::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// 单个单元格的渲染快照，用于行内 diff。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Snap {
    c: char,
    fg: Col,
    bg: Col,
    flags: u16,
}

impl Snap {
    fn is_blank(&self) -> bool {
        (self.c == ' ' || self.c == '\0')
            && self.fg == Col::Default
            && self.bg == Col::Default
            && self.flags & !(Flags::WRAPLINE.bits()) == 0
    }

    fn is_spacer(&self) -> bool {
        self.flags & (Flags::WIDE_CHAR_SPACER.bits() | Flags::LEADING_WIDE_CHAR_SPACER.bits()) != 0
    }
}

/// 生成 SGR 序列：0 复位 + 属性 + 前景 + 背景。
fn sgr(fg: Col, bg: Col, flags: u16) -> String {
    let mut codes: Vec<String> = vec!["0".to_string()];
    if flags & Flags::BOLD.bits() != 0 {
        codes.push("1".into());
    }
    if flags & Flags::DIM.bits() != 0 {
        codes.push("2".into());
    }
    if flags & Flags::ITALIC.bits() != 0 {
        codes.push("3".into());
    }
    if flags & Flags::ALL_UNDERLINES.bits() != 0 {
        codes.push("4".into());
    }
    if flags & Flags::INVERSE.bits() != 0 {
        codes.push("7".into());
    }
    if flags & Flags::HIDDEN.bits() != 0 {
        codes.push("8".into());
    }
    if flags & Flags::STRIKEOUT.bits() != 0 {
        codes.push("9".into());
    }
    push_color(&mut codes, fg, false);
    push_color(&mut codes, bg, true);
    format!("\x1b[{}m", codes.join(";"))
}

fn push_color(codes: &mut Vec<String>, c: Col, bg: bool) {
    match c {
        Col::Default => {}
        Col::Idx(i) if i < 8 => codes.push(format!("{}", if bg { 40 } else { 30 } + i as u16)),
        Col::Idx(i) if i < 16 => {
            codes.push(format!("{}", if bg { 100 } else { 90 } + (i as u16 - 8)))
        }
        Col::Idx(i) => codes.push(format!("{};5;{}", if bg { 48 } else { 38 }, i)),
        Col::Rgb(r, g, b) => codes.push(format!("{};2;{};{};{}", if bg { 48 } else { 38 }, r, g, b)),
    }
}

/// 渲染一行（从 start 列开始），返回 ANSI 片段。
fn render_row(snaps: &[Snap]) -> String {
    // 尾部空白用 \x1b[K 清掉，避免逐列写空格。
    let mut end = snaps.len();
    while end > 0 && snaps[end - 1].is_blank() {
        end -= 1;
    }
    if end == 0 {
        return "\x1b[K".to_string();
    }
    let mut out = String::new();
    let mut cur: Option<(Col, Col, u16)> = None;
    for s in &snaps[..end] {
        if s.is_spacer() {
            continue;
        }
        let sig = (s.fg, s.bg, s.flags);
        if cur != Some(sig) {
            out.push_str(&sgr(s.fg, s.bg, s.flags));
            cur = Some(sig);
        }
        if s.c != '\0' {
            out.push(s.c);
        }
    }
    out.push_str("\x1b[0m");
    if end < snaps.len() {
        out.push_str("\x1b[K");
    }
    out
}

/// 屏幕渲染器：持有模拟器，按行 diff 输出 ANSI。
pub struct Screen {
    emu: Emu,
    /// 上一次渲染的每一行内容。
    prev: Vec<Vec<Snap>>,
    /// 上次定位过的光标（1 基），None = 需要重新定位。
    cursor: Option<(u16, u16)>,
}

impl Screen {
    /// rows 为内容行数（不含状态栏行）。
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            emu: Emu::new(cols, rows),
            prev: Vec::new(),
            cursor: None,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.emu.resize(cols, rows);
        self.prev.clear();
        self.cursor = None;
    }

    pub fn fullscreen(&self) -> bool {
        self.emu.fullscreen()
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.emu.cursor()
    }

    pub fn cols(&self) -> u16 {
        self.emu.cols()
    }

    pub fn rows(&self) -> u16 {
        self.emu.rows()
    }

    /// 远端输出：进网格后渲染差异。
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        self.emu.feed(bytes);
        self.render()
    }

    /// 本地打印（命令结果、列表等）：作为合成输出进网格，参与滚动。
    pub fn print(&mut self, bytes: &[u8]) -> String {
        self.emu.feed(bytes);
        self.render()
    }

    /// 打字回显是直接写控制台的临时覆盖层，写完后光标位置未知，需重新定位。
    pub fn note_echo(&mut self) {
        self.cursor = None;
    }

    /// 计算需要写给控制台的增量 ANSI。
    pub fn render(&mut self) -> String {
        let cols = self.emu.cols() as usize;
        let rows = self.emu.rows() as usize;
        if self.prev.len() != rows || self.prev.first().map_or(0, |r| r.len()) != cols {
            self.prev = vec![vec![Snap::default(); cols]; rows];
            self.cursor = None;
        }

        let mut out = String::new();
        for row in 0..rows {
            let mut snaps = Vec::with_capacity(cols);
            let mut from: Option<usize> = None;
            for col in 0..cols {
                let s = self.emu.cell(row, col);
                if from.is_none() && s != self.prev[row][col] {
                    from = Some(col);
                }
                snaps.push(s);
            }
            if let Some(f) = from {
                out.push_str(&format!("\x1b[{};{}H", row + 1, f + 1));
                out.push_str(&render_row(&snaps[f..]));
            }
            self.prev[row] = snaps;
        }

        // 查看历史时隐藏光标，不做定位（光标属于最新屏底）。
        if self.emu.display_offset() > 0 {
            if out.is_empty() {
                return out;
            }
            return format!("\x1b[?25l{out}");
        }

        let (cr, cc) = self.emu.cursor();
        let pos = (cr as u16 + 1, cc as u16 + 1);
        if self.cursor != Some(pos) {
            out.push_str(&format!("\x1b[{};{}H", pos.0, pos.1));
            self.cursor = Some(pos);
        }

        if out.is_empty() {
            return out;
        }
        format!("\x1b[?25l{out}\x1b[?25h")
    }

    /// 翻页查看历史（与真实终端一致：仅普通屏幕生效）。
    pub fn scroll_page(&mut self, up: bool) -> String {
        if self.emu.fullscreen() {
            return String::new();
        }
        if !up && self.emu.display_offset() == 0 {
            return String::new();
        }
        self.emu
            .scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
        self.cursor = None;
        self.render()
    }

    /// 回到最新输出（有新键入时调用，和真实终端行为一致）。
    pub fn reset_scroll(&mut self) -> String {
        if self.emu.display_offset() == 0 {
            return String::new();
        }
        self.emu.scroll_display(Scroll::Bottom);
        self.cursor = None;
        self.render()
    }

    /// 清屏并丢弃渲染缓存。
    pub fn clear(&mut self) -> String {
        self.prev.clear();
        self.cursor = None;
        "\x1b[2J\x1b[H".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_screen_enter_exit() {
        let mut s = Screen::new(80, 24);
        assert!(!s.fullscreen());
        s.feed(b"\x1b[?1049h");
        assert!(s.fullscreen());
        s.feed(b"\x1b[?1049l");
        assert!(!s.fullscreen());
    }

    #[test]
    fn smkx_app_cursor_tracks_enter_exit() {
        let mut s = Screen::new(80, 24);
        s.feed(b"\x1b[?1h");
        assert!(s.fullscreen());
        s.feed(b"\x1b[?1l");
        assert!(!s.fullscreen());
    }

    #[test]
    fn similar_sequences_do_not_misfire() {
        let mut s = Screen::new(80, 24);
        s.feed(b"\x1b[?12l");
        s.feed(b"\x1b[?1034h");
        s.feed(b"\x1b[?1000l");
        s.feed(b"\x1b[?1006h");
        assert!(!s.fullscreen());
    }

    #[test]
    fn mixed_1049_and_smkx_in_one_chunk() {
        let mut s = Screen::new(80, 24);
        s.feed(b"\x1b[?1049h\x1b[?1h");
        assert!(s.fullscreen());
        s.feed(b"\x1b[?1l\x1b[?1049l");
        assert!(!s.fullscreen());
    }

    #[test]
    fn render_outputs_text_and_cursor() {
        let mut s = Screen::new(80, 24);
        let out = s.feed(b"hello");
        assert!(out.contains("hello"), "out={out:?}");
        // 光标定位到第 1 行第 6 列。
        assert!(out.contains("\x1b[1;6H"), "out={out:?}");
    }

    #[test]
    fn unchanged_screen_renders_nothing() {
        let mut s = Screen::new(80, 24);
        let _ = s.feed(b"stable");
        let out = s.feed(b"");
        assert!(out.is_empty(), "out={out:?}");
    }

    #[test]
    fn only_changed_row_is_redrawn() {
        let mut s = Screen::new(80, 24);
        let _ = s.feed(b"first\r\nsecond");
        // 回车后光标在第二行，再输出会覆盖第二行内容。
        let out = s.feed(b"\rX");
        assert!(!out.contains("first"), "out={out:?}");
        assert!(out.contains('X'), "out={out:?}");
    }

    #[test]
    fn cursor_position_after_cup_clamped() {
        let mut s = Screen::new(80, 24);
        s.feed(b"\x1b[34;1H");
        assert_eq!(s.cursor().0, 23);
    }

    #[test]
    fn render_sgr_for_colored_text() {
        let mut s = Screen::new(80, 24);
        let out = s.feed(b"\x1b[31mred\x1b[0m");
        assert!(out.contains("\x1b[0;31m") || out.contains(";31"), "out={out:?}");
    }

    #[test]
    fn scroll_region_does_not_confuse_fullscreen() {
        let mut s = Screen::new(80, 24);
        s.feed(b"\x1b[1;29r");
        assert!(!s.fullscreen());
    }

    #[test]
    fn resize_rebuilds_prev_buffer() {
        let mut s = Screen::new(80, 24);
        let _ = s.feed(b"hello");
        s.resize(40, 10);
        let out = s.feed(b"");
        // 尺寸变化后必须全量重绘一次（不能因为 prev 还在而跳过）。
        assert!(out.contains('h'), "out={out:?}");
    }

    #[test]
    fn print_local_text_enters_grid_and_scrolls() {
        let mut s = Screen::new(20, 3);
        let out = s.feed(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        assert!(!out.is_empty());
        // 4 行内容在 3 行屏幕上滚动，最后一行应是 four 的上一行位置。
        assert_eq!(s.cursor().0, 2);
    }
}

#[cfg(test)]
mod scroll_probe {
    use super::*;

    #[test]
    fn scroll_page_reveals_history() {
        let mut s = Screen::new(20, 3);
        // 写 10 行，屏 3 行 → 历史 7 行
        let _ = s.feed(b"01\r\n02\r\n03\r\n04\r\n05\r\n06\r\n07\r\n08\r\n09\r\n10\r\n");
        let bottom: Vec<char> = (0..3)
            .map(|r| s.emu.cell_text(r))
            .collect::<Vec<_>>()
            .iter()
            .map(|t| t.chars().next().unwrap_or(' '))
            .collect();
        let out = s.scroll_page(true);
        let after: Vec<char> = (0..3)
            .map(|r| s.emu.cell_text(r))
            .collect::<Vec<_>>()
            .iter()
            .map(|t| t.chars().next().unwrap_or(' '))
            .collect();
        assert_ne!(bottom, after, "offset={} out_empty={}", s.emu.display_offset(), out.is_empty());
    }
}
