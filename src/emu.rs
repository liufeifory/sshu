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
use unicode_width::UnicodeWidthChar;

/// 默认回滚历史行数（供滚轮 / PgUp / PgDn 查看）。
/// 每行约占 3 KB 内存（100 列 × 32 B/单元格），2 万行 ≈ 60 MB。
pub const DEFAULT_HISTORY: usize = 20_000;

/// 回滚上限：避免 --history 被写成天文数字把内存吃光。
const MAX_HISTORY: usize = 200_000;

/// 终端尺寸（屏幕行 + 回滚历史）。
#[derive(Clone, Copy)]
struct Dims {
    columns: usize,
    screen_lines: usize,
    history: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        // 屏幕行 + 回滚历史：本机终端的历史由我们自己维护
        // （渲染接管屏幕后，conhost 的 scrollback 不再被填充）。
        self.screen_lines + self.history
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
    history: usize,
}

impl Emu {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_history(cols, rows, DEFAULT_HISTORY)
    }

    pub fn with_history(cols: u16, rows: u16, history: usize) -> Self {
        let history = history.clamp(0, MAX_HISTORY);
        let dims = Dims {
            columns: cols.max(1) as usize,
            screen_lines: rows.max(1) as usize,
            history,
        };
        // 历史容量由 Config.scrolling_history 决定（Dimensions 里的
        // total_lines 只是给外部描述用）。
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &dims, VoidListener),
            processor: Processor::new(),
            cols: dims.columns,
            rows: dims.screen_lines,
            history,
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
            history: self.history,
        });
    }

    /// 当前可视偏移（>0 表示正在查看历史）。
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn scroll_display(&mut self, scroll: Scroll) {
        self.term.scroll_display(scroll);
    }

    /// 是否处于 1049 备用屏。
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// 网格当前的历史行数。
    ///
    /// `Grid::history_size` 是私有的，用「滚到顶读偏移再还原」探测。
    /// 只在两次渲染之间调用，瞬时的偏移变化不会写屏。
    pub fn history_len(&mut self) -> usize {
        let off = self.term.grid().display_offset();
        self.term.scroll_display(Scroll::Top);
        let len = self.term.grid().display_offset();
        if len != off {
            self.term.scroll_display(Scroll::Delta(off as i32 - len as i32));
        }
        len
    }

    /// 历史写满后「新增行数」无法再从 history_len 差值算出，
    /// 把最老的一半丢掉（这些行已进入本机终端的 scrollback，不丢内容）。
    pub fn compact_history(&mut self, max: usize) {
        if max < 2 || self.history_len() < max {
            return;
        }
        self.term.grid_mut().update_history(max / 2);
        self.term.grid_mut().update_history(max);
    }

    /// 远端是否自己开了鼠标上报（vim/htop 之类）。
    pub fn mouse_reporting(&self) -> bool {
        self.mouse_level() > 0
    }

    /// 远端鼠标上报级别：0 关，1 点击，2 拖拽，3 移动。
    pub fn mouse_level(&self) -> u8 {
        let mode = self.term.mode();
        if mode.contains(TermMode::MOUSE_MOTION) {
            3
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            2
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            1
        } else {
            0
        }
    }

    /// 应用光标键模式（smkx）：方向键要发 SS3 序列。
    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
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
///
/// 宿主终端的 scrollback 是「真」历史：网格每滚过一行，就向宿主终端补发
/// 一个换行，让这一行真正滚出宿主屏幕、进入它的 scrollback —— 这样滚轮、
/// 滚动条、拖选复制都是本机终端的原生行为。渲染缓存（prev）随之整体上移，
/// 保证绝对定位始终对得齐。
pub struct Screen {
    emu: Emu,
    /// 上一次渲染的每一行内容（含最后一行状态栏）。
    prev: Vec<Vec<Snap>>,
    /// 上次定位过的光标（1 基），None = 需要重新定位。
    cursor: Option<(u16, u16)>,
    /// 网格历史容量（用于写满后收缩，保证滚动行数可统计）。
    max_history: usize,
    /// 用户查看历史期间网格又滚过的行数，等回到最新屏时一并补发给宿主。
    debt: usize,
    /// 底部状态栏文本（本地内容，不属于远端网格）。
    bar: String,
    /// 打字回显覆盖层的光标偏移（行偏移, 列偏移），相对网格光标。
    /// 覆盖层字符不进网格，网格不知道它们存在；渲染定位光标时必须加上
    /// 这个偏移，否则光标会跳回已回显字符左边（magic 词前缀逐字回显时
    /// 尤其明显）。行偏移 >0 时列偏移是绝对列（覆盖层从行首重新计）。
    overlay: (i32, i32),
}

impl Screen {
    /// rows 为内容行数（不含状态栏行）。
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_history(cols, rows, DEFAULT_HISTORY)
    }

    pub fn with_history(cols: u16, rows: u16, history: usize) -> Self {
        Self {
            emu: Emu::with_history(cols, rows, history),
            prev: Vec::new(),
            cursor: None,
            max_history: history.clamp(0, MAX_HISTORY),
            debt: 0,
            bar: String::new(),
            overlay: (0, 0),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.emu.resize(cols, rows);
        self.prev.clear();
        self.debt = 0;
        self.cursor = None;
        self.overlay = (0, 0);
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

    /// 更新底部状态栏文本（下一帧 render 时按 diff 重画）。
    pub fn set_bar(&mut self, text: &str) {
        self.bar = text.to_string();
    }

    /// 远端输出：进网格后渲染差异。
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        let mut out = String::new();
        self.push_remote(bytes, &mut out);
        out.push_str(&self.render());
        out
    }

    /// 本地打印（命令结果、列表等）：作为合成输出进网格，参与滚动。
    pub fn print(&mut self, bytes: &[u8]) -> String {
        self.feed(bytes)
    }

    /// 喂远端字节，并把网格新增的历史行数同步给宿主终端。
    fn push_remote(&mut self, bytes: &[u8], out: &mut String) {
        if bytes.is_empty() {
            return;
        }
        // 远端输出进入网格后，覆盖层光标重新以网格光标为锚点；
        // 被覆盖的回显由调用方按 pending_echo 重画并重新累计偏移。
        self.overlay = (0, 0);
        if self.emu.alt_screen() {
            // 备用屏（top/vim/less）不产生真正的历史，宿主也不该滚动。
            self.emu.feed(bytes);
            return;
        }
        self.emu.compact_history(self.max_history);
        let h0 = self.emu.history_len();
        self.emu.feed(bytes);
        let delta = self.emu.history_len().saturating_sub(h0);
        if delta == 0 {
            return;
        }
        if self.emu.display_offset() == 0 {
            self.emit_scroll(out, delta);
        } else {
            // 正在看历史：宿主画面是历史视图，不能现在滚，记账等回来再补。
            self.debt += delta;
        }
    }

    /// 让宿主终端滚过 n 行：先把光标挪到最后一行再补换行，
    /// 换行把旧行推进宿主 scrollback，同时渲染缓存整体上移保持对齐。
    fn emit_scroll(&mut self, out: &mut String, n: usize) {
        let rows = self.emu.rows() as usize; // 内容行数；物理最后一行是状态栏
        out.push_str(&format!("\x1b[{};1H", rows + 1));
        for _ in 0..n {
            out.push('\n');
        }
        let cols = self.prev.first().map_or(0, |r| r.len());
        if self.prev.len() == rows + 1 && cols > 0 {
            for _ in 0..n {
                self.prev.remove(0);
                self.prev.push(vec![Snap::default(); cols]);
            }
        }
        self.cursor = None;
    }

    /// 打字回显是直接写控制台的覆盖层：解释这批字节，跟踪覆盖层光标
    /// 相对网格光标的偏移，渲染时据此定位，光标不会跳回回显内容左边。
    pub fn note_echo(&mut self, bytes: &[u8]) {
        self.cursor = None;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                0x08 => {
                    // 退格（冲刷前的擦除）
                    self.overlay.1 = (self.overlay.1 - 1).max(0);
                    i += 1;
                }
                b'\r' => {
                    self.overlay.1 = 0;
                    i += 1;
                }
                b'\n' => {
                    self.overlay.0 += 1;
                    self.overlay.1 = 0;
                    i += 1;
                }
                0x1b => {
                    // 覆盖层正常不含转义序列；出现时跳过完整 CSI 以免污染计数。
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'[' {
                        i += 1;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                _ => {
                    let rest = match std::str::from_utf8(&bytes[i..]) {
                        Ok(s) => s,
                        Err(_) => {
                            i += 1;
                            continue;
                        }
                    };
                    let Some(ch) = rest.chars().next() else {
                        i += 1;
                        continue;
                    };
                    i += ch.len_utf8();
                    self.overlay.1 += ch.width().unwrap_or(0).max(0) as i32;
                }
            }
        }
    }

    /// 计算需要写给控制台的增量 ANSI。
    pub fn render(&mut self) -> String {
        let cols = self.emu.cols() as usize;
        let rows = self.emu.rows() as usize;
        let total = rows + 1; // 最后一行是本地状态栏
        if self.prev.len() != total || self.prev.first().map_or(0, |r| r.len()) != cols {
            self.prev = vec![vec![Snap::default(); cols]; total];
            self.cursor = None;
        }

        let bar = bar_row(&self.bar, cols);
        let mut out = String::new();
        for row in 0..total {
            let mut snaps = Vec::with_capacity(cols);
            let mut from: Option<usize> = None;
            for col in 0..cols {
                let s = if row < rows {
                    self.emu.cell(row, col)
                } else {
                    bar[col]
                };
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
        // 覆盖层偏移：行偏移 >0 说明覆盖层换过行（如 "(sshu) " 提示符），
        // 此时列是绝对列；否则在网格光标所在行上累加。
        let (r, c) = if self.overlay.0 > 0 {
            (cr as i32 + self.overlay.0, self.overlay.1)
        } else {
            (cr as i32, cc as i32 + self.overlay.1)
        };
        let r = r.clamp(0, rows as i32) as u16; // 最后一行是状态栏
        let c = c.clamp(0, cols.saturating_sub(1) as i32) as u16;
        let pos = (r + 1, c + 1);
        if self.cursor != Some(pos) {
            out.push_str(&format!("\x1b[{};{}H", pos.0, pos.1));
            self.cursor = Some(pos);
        }

        if out.is_empty() {
            return out;
        }
        format!("\x1b[?25l{out}\x1b[?25h")
    }

    /// 翻页查看历史（与真实终端一致：全屏应用里交给应用自己处理）。
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

    /// 滚轮按行滚动（正数向上）。全屏应用里没有本地历史可看，直接忽略。
    pub fn scroll_lines(&mut self, lines: i32) -> String {
        if self.emu.fullscreen() {
            return String::new();
        }
        if lines < 0 && self.emu.display_offset() == 0 {
            return String::new();
        }
        self.emu.scroll_display(Scroll::Delta(lines));
        self.cursor = None;
        self.render()
    }

    /// 当前回滚偏移（0 = 最新输出）。
    pub fn scroll_offset(&self) -> usize {
        self.emu.display_offset()
    }

    /// 远端是否自己开了鼠标上报（决定滚轮事件转给远端还是本地翻历史）。
    pub fn mouse_reporting(&self) -> bool {
        self.emu.mouse_level() > 0
    }

    /// 远端鼠标上报级别：0 关，1 点击，2 拖拽，3 移动。
    pub fn mouse_level(&self) -> u8 {
        self.emu.mouse_level()
    }

    pub fn app_cursor(&self) -> bool {
        self.emu.app_cursor()
    }

    /// 回到最新输出（有新键入时调用，和真实终端行为一致）。
    pub fn reset_scroll(&mut self) -> String {
        if self.emu.display_offset() == 0 {
            return String::new();
        }
        self.emu.scroll_display(Scroll::Bottom);
        self.cursor = None;
        let mut out = String::new();
        if self.debt > 0 {
            // 先清屏再补滚动：宿主当前显示的是历史视图，直接滚会把这份
            // 历史画面推进 scrollback；清屏后推入的是空白行，不污染历史。
            out.push_str("\x1b[2J");
            self.prev.clear();
            let debt = std::mem::take(&mut self.debt);
            self.emit_scroll(&mut out, debt);
        }
        out.push_str(&self.render());
        out
    }

    /// 清屏并丢弃渲染缓存。
    pub fn clear(&mut self) -> String {
        self.prev.clear();
        self.debt = 0;
        self.cursor = None;
        self.overlay = (0, 0);
        "\x1b[2J\x1b[H".to_string()
    }
}

/// 状态栏行：文本 + 空白填充（最后一列留空，避免触发终端自动换行）。
fn bar_row(text: &str, cols: usize) -> Vec<Snap> {
    let mut snaps = Vec::with_capacity(cols);
    let width = cols.saturating_sub(1);
    for ch in text.chars().take(width) {
        snaps.push(Snap { c: ch, ..Snap::default() });
    }
    snaps.resize(cols, Snap::default());
    snaps
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

    /// magic 词前缀逐字回显后，渲染定位的光标必须在回显字符右边，
    /// 而不是跳回字符左边（用户可见 bug：u/p/d/g/l 打下去光标在左边）。
    #[test]
    fn echo_overlay_keeps_cursor_right_of_echoed_chars() {
        let mut s = Screen::new(20, 3);
        let _ = s.feed(b"$ "); // 网格光标 0 基 (0,2)
        let _ = s.render();
        s.note_echo(b"u");
        let out = s.render();
        // 1 基 (1,4)：$ 之后 u 的右边
        assert!(out.contains("\x1b[1;4H"), "out={out:?}");
        s.note_echo(b"p");
        let out = s.render();
        assert!(out.contains("\x1b[1;5H"), "out={out:?}");
    }

    /// 冲刷前的擦除（\x08 \x08）把覆盖层光标收回网格光标处。
    #[test]
    fn echo_erase_returns_overlay_to_anchor() {
        let mut s = Screen::new(20, 3);
        let _ = s.feed(b"$ ");
        let _ = s.render();
        s.note_echo(b"up");
        s.note_echo(b"\x08 \x08\x08 \x08");
        let out = s.render();
        // 回到 1 基 (1,3)
        assert!(out.contains("\x1b[1;3H"), "out={out:?}");
    }

    /// "(sshu) " 提示符带换行：行偏移生效后列是绝对列。
    #[test]
    fn echo_overlay_newline_uses_absolute_col() {
        let mut s = Screen::new(20, 3);
        let _ = s.feed(b"prompt...$ "); // 光标 0 基 (0,10)
        let _ = s.render();
        s.note_echo(b"\r\n(sshu) ");
        let out = s.render();
        // 下一行行首起 7 列："（sshu) " 后 → 1 基 (2,8)
        assert!(out.contains("\x1b[2;8H"), "out={out:?}");
    }
}

#[cfg(test)]
mod scroll_probe {
    use super::*;

    fn lines(n: usize) -> Vec<u8> {
        (0..n).flat_map(|i| format!("{i:03}\r\n").into_bytes()).collect()
    }

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

    #[test]
    fn history_depth_follows_config() {
        let mut s = Screen::with_history(20, 3, 5000);
        let _ = s.feed(&lines(100));
        s.emu.scroll_display(Scroll::Top);
        // 100 行内容 + 结尾空行 - 3 行屏幕 = 98 行历史
        assert_eq!(s.emu.display_offset(), 98);
    }

    #[test]
    fn history_stops_at_configured_limit() {
        let mut s = Screen::with_history(20, 3, 10);
        let _ = s.feed(&lines(100));
        s.emu.scroll_display(Scroll::Top);
        assert_eq!(s.emu.display_offset(), 10);
    }

    #[test]
    fn scroll_lines_moves_and_clamps() {
        let mut s = Screen::with_history(20, 3, 1000);
        let _ = s.feed(&lines(40));
        let out = s.scroll_lines(5);
        assert_eq!(s.scroll_offset(), 5);
        assert!(!out.is_empty());
        s.scroll_lines(-100);
        assert_eq!(s.scroll_offset(), 0);
    }

    #[test]
    fn scroll_lines_ignored_in_fullscreen() {
        let mut s = Screen::with_history(20, 3, 1000);
        let _ = s.feed(&lines(40));
        s.feed(b"\x1b[?1049h");
        assert!(s.scroll_lines(5).is_empty());
        assert_eq!(s.scroll_offset(), 0);
    }

    #[test]
    fn scroll_pushes_newlines_to_host() {
        let mut s = Screen::new(20, 3);
        // 5 行内容 + 结尾空行 = 6 行，屏 3 行 → 宿主要滚 3 行
        let out = s.feed(b"a\r\nb\r\nc\r\nd\r\ne\r\n");
        assert_eq!(out.matches('\n').count(), 3, "out={out:?}");
        // 换行前先把光标挪到物理最后一行（状态栏行）
        assert!(out.contains("\x1b[4;1H"), "out={out:?}");
    }

    #[test]
    fn scroll_debt_flushed_on_return() {
        let mut s = Screen::new(20, 3);
        let _ = s.feed(b"a\r\nb\r\nc\r\nd\r\n"); // 滚 2 行
        let _ = s.scroll_page(true); // 开始查看历史
        assert!(s.scroll_offset() > 0);
        let out = s.feed(b"e\r\nf\r\n"); // 又滚 2 行 → 记账不发
        assert_eq!(out.matches('\n').count(), 0, "out={out:?}");
        let out = s.reset_scroll(); // 回到底部 → 清屏 + 补发
        assert!(out.contains("\x1b[2J"), "out={out:?}");
        assert_eq!(out.matches('\n').count(), 2, "out={out:?}");
        assert_eq!(s.scroll_offset(), 0);
    }

    #[test]
    fn alt_screen_output_does_not_scroll_host() {
        let mut s = Screen::new(20, 3);
        s.feed(b"\x1b[?1049h");
        let out = s.feed(&lines(50));
        assert_eq!(out.matches('\n').count(), 0, "out={out:?}");
    }

    #[test]
    fn bar_text_rendered_on_last_row() {
        let mut s = Screen::new(20, 3);
        s.set_bar("hello bar");
        let out = s.feed(b"hi");
        assert!(out.contains("hello bar"), "out={out:?}");
        assert!(out.contains("\x1b[4;1H"), "out={out:?}");
    }
}
