//! 鼠标事件解析：只认滚轮，其余事件交给调用方决定吞掉还是转给远端。
//!
//! 支持两种编码：SGR（`ESC [ < b ; x ; y M/m`，`?1006h` 之后的标准格式）
//! 与 X10（`ESC [ M` + 3 字节）。Windows Terminal / xterm 默认都是 SGR。

/// 解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    /// 一个完整事件 + 该序列占用的字节数。
    Event(Event, usize),
    /// 看起来是鼠标序列但还没收全，等下一批字节。
    Partial,
    /// 不是鼠标序列，按普通按键处理。
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    WheelUp,
    WheelDown,
    Other,
}

/// 解码按键码：低 2 位是键号，64/65 是滚轮上/下（4/8/16 是修饰键位）。
fn decode(code: u32) -> Event {
    match code & !(4 | 8 | 16) {
        64 => Event::WheelUp,
        65 => Event::WheelDown,
        _ => Event::Other,
    }
}

/// 从缓冲区头部解析一个鼠标事件。
pub fn parse(buf: &[u8]) -> Parsed {
    if buf.starts_with(b"\x1b[<") {
        return parse_sgr(buf);
    }
    if buf.starts_with(b"\x1b[M") {
        // X10：3 个字节，各 +32 编码。
        return if buf.len() < 6 {
            Parsed::Partial
        } else {
            Parsed::Event(decode(buf[3].saturating_sub(32) as u32), 6)
        };
    }
    Parsed::Other
}

fn parse_sgr(buf: &[u8]) -> Parsed {
    // ESC [ < code ; x ; y M|m
    let mut i = 3;
    while i < buf.len() && (buf[i].is_ascii_digit() || buf[i] == b';') {
        i += 1;
    }
    if i >= buf.len() {
        // 还没有终止符；超过 32 字节说明不是合法序列，别再等。
        return if buf.len() > 32 { Parsed::Other } else { Parsed::Partial };
    }
    if buf[i] != b'M' && buf[i] != b'm' {
        return Parsed::Other;
    }
    let code = buf[3..i]
        .split(|b| *b == b';')
        .next()
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.parse::<u32>().ok());
    match code {
        Some(c) => Parsed::Event(decode(c), i + 1),
        None => Parsed::Other,
    }
}

/// 在一段按键字节里找到鼠标序列的起点（用于把「普通按键 + 鼠标」拆开）。
pub fn find_start(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == 0x1b {
            if buf[i + 1] == b'[' && (buf[i + 2] == b'<' || buf[i + 2] == b'M') {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_wheel_up() {
        assert_eq!(parse(b"\x1b[<64;10;20M"), Parsed::Event(Event::WheelUp, 12));
    }

    #[test]
    fn sgr_wheel_down_with_modifier() {
        assert_eq!(parse(b"\x1b[<69;3;4M"), Parsed::Event(Event::WheelDown, 10));
    }

    #[test]
    fn sgr_click_is_other() {
        assert_eq!(parse(b"\x1b[<0;3;4M"), Parsed::Event(Event::Other, 9));
        assert_eq!(parse(b"\x1b[<0;3;4m"), Parsed::Event(Event::Other, 9));
    }

    #[test]
    fn sgr_partial_waits() {
        assert_eq!(parse(b"\x1b[<64;10"), Parsed::Partial);
        assert_eq!(parse(b"\x1b[<"), Parsed::Partial);
    }

    #[test]
    fn x10_wheel() {
        // 64+32=96 -> '`'，坐标 +32
        assert_eq!(parse(b"\x1b[M`\x23\x24"), Parsed::Event(Event::WheelUp, 6));
        assert_eq!(parse(b"\x1b[Ma\x23\x24"), Parsed::Event(Event::WheelDown, 6));
    }

    #[test]
    fn plain_keys_are_not_mouse() {
        assert_eq!(parse(b"a"), Parsed::Other);
        assert_eq!(parse(b"\x1b"), Parsed::Other);
        assert_eq!(parse(b"\x1b[A"), Parsed::Other);
        assert_eq!(parse(b"\x1b[5~"), Parsed::Other);
    }

    #[test]
    fn find_start_locates_mouse() {
        assert_eq!(find_start(b"abc\x1b[<64;1;2M"), Some(3));
        assert_eq!(find_start(b"\x1b[<64;1;2M"), Some(0));
        assert_eq!(find_start(b"abc"), None);
        assert_eq!(find_start(b"\x1b[A"), None);
    }
}
