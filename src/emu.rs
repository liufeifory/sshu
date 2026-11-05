//! 影子终端模拟器：把远端字节喂给 alacritty_terminal 的 VT 状态机，
//! 跟踪全屏模式（alt screen / 应用光标键）和光标位置，供本地注入决策
//! （magic 拦截、状态栏滚动区域管理）使用。远端输出本身仍原样透传给
//! 本地终端，模拟器只做"状态影子"。

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

/// 终端尺寸（无回滚缓冲：影子模拟器只关心当前屏幕状态）。
#[derive(Clone, Copy)]
struct Dims {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.screen_lines
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

    /// 喂入远端字节，更新模拟器状态。
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// 远端是否进入全屏/接管模式（1049 alt screen 或 smkx 应用光标键）。
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
        self.cols = (cols.max(1)) as usize;
        self.rows = (rows.max(1)) as usize;
        self.term
            .resize(Dims { columns: self.cols, screen_lines: self.rows });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_screen_enter_exit() {
        let mut e = Emu::new(80, 24);
        assert!(!e.fullscreen());
        e.feed(b"\x1b[?1049h");
        assert!(e.fullscreen());
        e.feed(b"\x1b[?1049l");
        assert!(!e.fullscreen());
    }

    #[test]
    fn smkx_app_cursor_tracks_enter_exit() {
        let mut e = Emu::new(80, 24);
        e.feed(b"\x1b[?1h");
        assert!(e.fullscreen());
        e.feed(b"\x1b[?1l");
        assert!(!e.fullscreen());
    }

    #[test]
    fn similar_sequences_do_not_misfire() {
        let mut e = Emu::new(80, 24);
        // 这些序列都与 ?1h/?1049 无关，不得误触发全屏判定。
        e.feed(b"\x1b[?12l");
        e.feed(b"\x1b[?1034h");
        e.feed(b"\x1b[?1000l");
        e.feed(b"\x1b[?1006h");
        assert!(!e.fullscreen());
    }

    #[test]
    fn mixed_1049_and_smkx_in_one_chunk() {
        // vim 常见进入序列：一次写多个模式切换，必须全部正确解析。
        let mut e = Emu::new(80, 24);
        e.feed(b"\x1b[?1049h\x1b[?1h");
        assert!(e.fullscreen());
        e.feed(b"\x1b[?1l\x1b[?1049l");
        assert!(!e.fullscreen());
    }

    #[test]
    fn cursor_tracks_text_and_newline() {
        let mut e = Emu::new(80, 24);
        e.feed(b"abc");
        assert_eq!(e.cursor(), (0, 3));
        e.feed(b"\r\nxyz");
        assert_eq!(e.cursor(), (1, 3));
    }

    #[test]
    fn cursor_position_after_cup() {
        let mut e = Emu::new(80, 24);
        e.feed(b"\x1b[34;1H");
        // 34 行超出 24 行屏幕，光标应被钳制到底部（第 24 行，行号 23）。
        assert_eq!(e.cursor().0, 23);
        assert_eq!(e.cursor().1, 0);
    }

    #[test]
    fn resize_updates_cursor_clamp() {
        let mut e = Emu::new(80, 24);
        e.feed(b"\x1b[5;10H");
        assert_eq!(e.cursor(), (4, 9));
        e.resize(80, 4);
        assert_eq!(e.cursor().0, 3);
    }

    #[test]
    fn scroll_region_does_not_confuse_fullscreen() {
        let mut e = Emu::new(80, 24);
        e.feed(b"\x1b[1;29r");
        assert!(!e.fullscreen());
    }
}
