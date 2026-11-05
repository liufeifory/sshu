//! 终端后端：原始字节读写 + 终端模式恢复。
//! Windows 走 Win32 控制台 API；Unix 走 termios 原始模式 + stdin/stdout。

use std::io;

use crate::error::SshuError;

/// 关闭终端默认开启的鼠标与焦点上报，避免把鼠标事件当成键盘
/// 输入转发给远端 shell（症状：登录后满屏 35;72;17M 之类的坐标序列）。
pub const DISABLE_MOUSE_REPORTING: &[u8] =
    b"[?1000l[?1002l[?1003l[?1006l[?1004l";

// ---------------------------------------------------------------------------
// Windows 后端
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod backend {
    use super::*;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo,
        GetStdHandle, SetConsoleCP, SetConsoleMode, SetConsoleOutputCP,
        CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// 在 UTF-8 控制台代码页下读取密码，避免中文提示乱码。
    pub fn prompt_password(prompt: &str) -> io::Result<String> {
        unsafe {
            let old_out = GetConsoleOutputCP();
            let old_in = GetConsoleCP();
            SetConsoleOutputCP(65001);
            SetConsoleCP(65001);
            let result = rpassword::prompt_password(prompt);
            if old_out != 0 {
                SetConsoleOutputCP(old_out);
            }
            if old_in != 0 {
                SetConsoleCP(old_in);
            }
            result
        }
    }

    /// Windows 控制台后端：原始 VT 字节读写 + 模式/代码页恢复。
    pub struct Console {
        hin: isize,
        hout: isize,
        in_mode: u32,
        out_mode: u32,
        cp_in: u32,
        cp_out: u32,
        opened: Vec<isize>,
        restored: bool,
    }

    impl Console {
        pub fn open() -> Result<Self, SshuError> {
            unsafe {
                let mut opened = Vec::new();
                let unavailable = || {
                    SshuError::new(
                        "sshu 需要 Windows 控制台（Windows Terminal / cmd / PowerShell）",
                    )
                };

                let hin =
                    open_handle(STD_INPUT_HANDLE, "CONIN$", &mut opened).ok_or_else(unavailable)?;
                let hout = open_handle(STD_OUTPUT_HANDLE, "CONOUT$", &mut opened)
                    .ok_or_else(unavailable)?;

                let mut in_mode = 0u32;
                if GetConsoleMode(hin, &mut in_mode) == 0 {
                    close_all(&opened);
                    return Err(unavailable());
                }
                let mut out_mode = 0u32;
                if GetConsoleMode(hout, &mut out_mode) == 0 {
                    close_all(&opened);
                    return Err(unavailable());
                }

            // 保留 ENABLE_QUICK_EDIT_MODE：传统控制台的鼠标选择复制 / 右键粘贴依赖它。
            // 泄漏进来的鼠标/焦点 VT 事件由 input.rs 的 filter_events 统一丢弃；
            // 远端程序的鼠标上报在远端输出侧也已用 ?1000l 等序列关闭。
            let new_in = (in_mode
                & !(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT))
                | ENABLE_EXTENDED_FLAGS
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
                if SetConsoleMode(hin, new_in) == 0 {
                    close_all(&opened);
                    return Err(unavailable());
                }
                if SetConsoleMode(hout, out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                    SetConsoleMode(hin, in_mode);
                    close_all(&opened);
                    return Err(unavailable());
                }

                let cp_in = GetConsoleCP();
                let cp_out = GetConsoleOutputCP();
                SetConsoleCP(65001);
                SetConsoleOutputCP(65001);

                let console = Self {
                    hin: hin as isize,
                    hout: hout as isize,
                    in_mode,
                    out_mode,
                    cp_in,
                    cp_out,
                    opened,
                    restored: false,
                };
                let _ = console.write_all(super::DISABLE_MOUSE_REPORTING);
                Ok(console)
            }
        }

        pub fn size(&self) -> (u16, u16) {
            unsafe {
                let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
                if GetConsoleScreenBufferInfo(self.hout as HANDLE, &mut info) != 0 {
                    let w = info.srWindow.Right - info.srWindow.Left + 1;
                    let h = info.srWindow.Bottom - info.srWindow.Top + 1;
                    if w > 0 && h > 0 {
                        return (w as u16, h as u16);
                    }
                }
            }
            (80, 24)
        }

        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut n = 0u32;
            let ok = unsafe {
                ReadFile(
                    self.hin as HANDLE,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(n as usize)
        }

        pub fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
            while !data.is_empty() {
                let mut n = 0u32;
                let ok = unsafe {
                    WriteFile(
                        self.hout as HANDLE,
                        data.as_ptr().cast(),
                        data.len() as u32,
                        &mut n,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "控制台写入失败"));
                }
                data = &data[n as usize..];
            }
            Ok(())
        }

        pub fn reader(&self) -> ConsoleReader {
            ConsoleReader { hin: self.hin }
        }

        pub fn restore(&mut self) {
            if self.restored {
                return;
            }
            self.restored = true;
            unsafe {
                SetConsoleMode(self.hin as HANDLE, self.in_mode);
                SetConsoleMode(self.hout as HANDLE, self.out_mode);
                if self.cp_in != 0 {
                    SetConsoleCP(self.cp_in);
                }
                if self.cp_out != 0 {
                    SetConsoleOutputCP(self.cp_out);
                }
                // 不在这里 CloseHandle 自开的 CONIN$/CONOUT$：读线程可能正阻塞
                // 在 ReadFile 上，关闭句柄会让进程卡在退出阶段。句柄由进程退出回收。
                let _ = &self.opened;
            }
        }
    }

    /// 只读输入句柄，可移到独立线程做阻塞 ReadFile。
    #[derive(Clone, Copy)]
    pub struct ConsoleReader {
        hin: isize,
    }

    impl ConsoleReader {
        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut n = 0u32;
            let ok = unsafe {
                ReadFile(
                    self.hin as HANDLE,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(n as usize)
        }
    }

    impl Drop for Console {
        fn drop(&mut self) {
            self.restore();
        }
    }

    unsafe fn open_handle(std_id: u32, name: &str, opened: &mut Vec<isize>) -> Option<HANDLE> {
        let h = GetStdHandle(std_id);
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            let mut mode = 0u32;
            if GetConsoleMode(h, &mut mode) != 0 {
                return Some(h);
            }
        }

        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let h2 = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if !h2.is_null() && h2 != INVALID_HANDLE_VALUE {
            opened.push(h2 as isize);
            Some(h2)
        } else {
            None
        }
    }

    fn close_all(opened: &[isize]) {
        unsafe {
            for h in opened {
                CloseHandle(*h as HANDLE);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unix 后端（Linux / macOS）
// ---------------------------------------------------------------------------
#[cfg(unix)]
mod backend {
    use super::*;

    /// Unix 终端后端：termios 原始模式 + fd 0/1 读写。
    pub struct Console {
        saved: libc::termios,
        restored: bool,
    }

    impl Console {
        pub fn open() -> Result<Self, SshuError> {
            if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
                return Err(SshuError::new("sshu 需要在终端（TTY）中运行"));
            }
            let mut t = unsafe { std::mem::zeroed::<libc::termios>() };
            if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
                return Err(SshuError::new(format!(
                    "无法读取终端设置: {}",
                    io::Error::last_os_error()
                )));
            }
            let saved = t;

            // raw 模式：关回显/行缓冲/信号（Ctrl-C、Ctrl-D 等作为原始字节转发远端），
            // 关 ICRNL（\r 原样发出），关 IXON（Ctrl-S/Ctrl-Q 不再冻结输出）。
            t.c_iflag &= !(libc::IGNBRK
                | libc::BRKINT
                | libc::PARMRK
                | libc::ISTRIP
                | libc::INLCR
                | libc::IGNCR
                | libc::ICRNL
                | libc::IXON);
            t.c_oflag &= !libc::OPOST;
            t.c_lflag &=
                !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
            t.c_cflag &= !(libc::CSIZE | libc::PARENB);
            t.c_cflag |= libc::CS8;
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            if unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) } != 0 {
                return Err(SshuError::new(format!(
                    "无法设置终端原始模式: {}",
                    io::Error::last_os_error()
                )));
            }

            let console = Self { saved, restored: false };
            let _ = console.write_all(super::DISABLE_MOUSE_REPORTING);
            Ok(console)
        }

        pub fn size(&self) -> (u16, u16) {
            let mut ws = libc::winsize {
                ws_row: 0,
                ws_col: 0,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0
                && ws.ws_col > 0
                && ws.ws_row > 0
            {
                return (ws.ws_col, ws.ws_row);
            }
            (80, 24)
        }

        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                return Ok(n as usize);
            }
        }

        pub fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
            while !data.is_empty() {
                let n = unsafe { libc::write(1, data.as_ptr().cast(), data.len()) };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "终端写入失败"));
                }
                data = &data[n as usize..];
            }
            Ok(())
        }

        pub fn reader(&self) -> ConsoleReader {
            ConsoleReader
        }

        pub fn restore(&mut self) {
            if self.restored {
                return;
            }
            self.restored = true;
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.saved);
            }
        }
    }

    /// 只读输入句柄，可移到独立线程做阻塞 read。
    #[derive(Clone, Copy)]
    pub struct ConsoleReader;

    impl ConsoleReader {
        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                return Ok(n as usize);
            }
        }
    }

    impl Drop for Console {
        fn drop(&mut self) {
            self.restore();
        }
    }

    /// rpassword 自身处理 termios，直接透传。
    pub fn prompt_password(prompt: &str) -> io::Result<String> {
        rpassword::prompt_password(prompt)
    }
}

pub use backend::{prompt_password, Console, ConsoleReader};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disables_mouse_and_focus_reporting() {
        let s = std::str::from_utf8(DISABLE_MOUSE_REPORTING).unwrap();
        for seq in ["?1000l", "?1002l", "?1003l", "?1006l", "?1004l"] {
            assert!(s.contains(seq), "{seq} missing in {s:?}");
        }
    }
}
