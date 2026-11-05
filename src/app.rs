use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client::Handle;
use tokio::sync::mpsc;

use crate::cli::{parse_prefix, Args};
use crate::error::SshuError;
use crate::input::{Action, Command, InputState};
use crate::osc7::Osc7Tracker;
use crate::sftp::{HookOutcome, Progress, Sftp, TransferSummary};
use crate::ssh::{probe_shell, spawn_shell, ClientHandler, Shell, ShellEvent};
use crate::status::{StatusLine, StatusSlot, TransferStatus};
use crate::term::Console;

pub enum AppError {
    OpenShell(SshuError),
    Runtime(SshuError),
}

impl From<SshuError> for AppError {
    fn from(e: SshuError) -> Self {
        AppError::Runtime(e)
    }
}

pub async fn run(
    handle: Handle<ClientHandler>,
    sftp: Option<Sftp>,
    console: Console,
    args: &Args,
) -> Result<(), AppError> {
    let prefix = parse_prefix(&args.prefix).map_err(|e| AppError::Runtime(SshuError::new(e)))?;
    let mut input = InputState::new(prefix, args.magic);
    let mut osc = Osc7Tracker::new();
    let mut status = StatusSlot::new();
    let gate = TransferGate::default();

    let shell_name = probe_shell(&handle).await;
    let mut hook_cmd = None;
    let mut hook_warning = None;
    if args.hook {
        match &sftp {
            Some(s) => match s.install_hook(shell_name.as_deref()).await {
                HookOutcome::Installed(cmd) => hook_cmd = Some(cmd),
                HookOutcome::Unsupported(sh) => {
                    hook_warning = Some(format!(
                        "远端 shell 不是 bash/zsh（{sh}），cwd 无法自动跟踪，请显式指定目录"
                    ))
                }
                HookOutcome::Failed(e) => hook_warning = Some(format!(
                    "cwd 钩子安装失败（{e}），请用 put <本地> <远端目录> 显式指定目录"
                )),
            },
            None => {
                hook_warning =
                    Some("SFTP 不可用，cwd 钩子无法安装；put/get 也不可用".to_string())
            }
        }
    }

    let (cols, rows) = console.size();
    let mut shell = spawn_shell(handle, hook_cmd, cols, rows)
        .await
        .map_err(AppError::OpenShell)?;

    let reader = console.reader();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if input_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let (st_tx, mut st_rx) = mpsc::unbounded_channel::<TransferStatus>();

    // 清屏：远端 MOTD/提示符从干净的第一行开始；不再打印横幅
    // （用法见 README；--debug 日志与钩子警告仍会输出到屏幕）。
    let _ = console.write_all(b"\x1b[2J\x1b[H");
    if let Some(w) = &hook_warning {
        let _ = console.write_all(format!("[警告] {w}\r\n").as_bytes());
    }

    let mut bar = Bar::new(cols, rows);
    bar.setup(&console);

    let mut last_status = String::new();
    let mut last_version = osc.version();
    let mut last_size = (cols, rows);
    // 全屏应用（top/vim/less 等发 smkx/1049）进入时临时取消了滚动区域，
    // 等下一个 OSC7 提示符（应用已退出、shell 提示符恢复）再补回来。
    let mut region_dirty = false;

    loop {
        let mut disconnected = false;
        tokio::select! {
            Some(bytes) = input_rx.recv() => {
                crate::debuglog::log_bytes("console-in", &bytes);
                for action in input.feed(&bytes) {
                    if let Action::Complete(line) = action {
                        // Tab 补全：远端路径需要 SFTP 异步列目录，就地 await。
                        crate::debuglog::log("cmd", &format!("complete: {line:?}"));
                        let comp =
                            crate::complete::compute(&line, &sftp, osc.cwd()).await;
                        let mut acts = Vec::new();
                        input.apply_completion(comp, &mut acts);
                        for a in acts {
                            dispatch(a, &shell, &console, &gate, &mut status, &sftp, &osc, &st_tx);
                        }
                    } else {
                        dispatch(action, &shell, &console, &gate, &mut status, &sftp, &osc, &st_tx);
                    }
                }
            }
            Some(ev) = shell.events.recv() => {
                match ev {
                    ShellEvent::Data(d) => {
                        crate::debuglog::log_bytes("remote-out", &d);
                        let _ = console.write_all(&d);
                        osc.feed(&d);
                        if osc.version() != last_version {
                            last_version = osc.version();
                            input.note_prompt();
                            if region_dirty {
                                region_dirty = false;
                                if let Some(seq) = bar_setup_sequence(console.size().1) {
                                    let _ = console.write_all(seq.as_bytes());
                                }
                            }
                        }
                        // smkx（ESC[?1h）与 1049 都是全屏应用接管的标志：
                        // 必须临时取消滚动区域，否则应用退出时的光标定位
                        // （如 top 退出时 ESC[34;1H 跳到屏幕底部之外）会被
                        // 区域夹持，导致后续输出渲染错乱甚至不可见。
                        if contains(&d, b"\x1b[?1049h") || contains(&d, b"\x1b[?1h") {
                            input.set_alt_screen(true);
                            let _ = console.write_all(b"\x1b[r");
                            region_dirty = true;
                        }
                        if contains(&d, b"\x1b[?1049l") || contains(&d, b"\x1b[?1l") {
                            input.set_alt_screen(false);
                        }
                    }
                    ShellEvent::Eof => {
                        crate::debuglog::log("state", "shell eof");
                        let _ = console.write_all("\r\n[连接已断开]\r\n".as_bytes());
                        disconnected = true;
                    }
                    ShellEvent::Error(e) => {
                        crate::debuglog::log("state", &format!("shell error: {e}"));
                        let _ = console.write_all(format!("\r\n[连接错误] {e}\r\n").as_bytes());
                        disconnected = true;
                    }
                }
            }
            Some(st) = st_rx.recv() => {
                crate::debuglog::log("xfer", &format!("{st:?}"));
                if matches!(&st, TransferStatus::Done(_) | TransferStatus::Failed(_)) {
                    gate.finish();
                }
                status.set(st, Instant::now());
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                // 空闲唤醒仅用于状态栏计时（完成后 3 秒自动清除）；
                // 悬空的 magic 前缀不再超时冲刷，保证慢速打字也能拦截。
            }
        }

        if disconnected {
            break;
        }

        let size = console.size();
        if size != last_size {
            last_size = size;
            bar.teardown(&console);
            let _ = shell.resize.send(size);
            bar = Bar::new(size.0, size.1);
            bar.setup(&console);
            last_status.clear();
        }

        match status.poll(Instant::now()) {
            StatusLine::Show(text) => {
                if text != last_status {
                    bar.draw(&console, &text);
                    last_status = text;
                }
            }
            StatusLine::Clear => {
                if !last_status.is_empty() {
                    bar.draw(&console, "");
                    last_status.clear();
                }
            }
            StatusLine::Nothing => {}
        }
    }

    bar.teardown(&console);
    Ok(())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 统一处理输入状态机产出的动作（输入分支与空闲 tick 共用）。
fn dispatch(
    action: Action,
    shell: &Shell,
    console: &Console,
    gate: &TransferGate,
    status: &mut StatusSlot,
    sftp: &Option<Sftp>,
    osc: &Osc7Tracker,
    st_tx: &mpsc::UnboundedSender<TransferStatus>,
) {
    match action {
        Action::ToRemote(b) => {
            crate::debuglog::log_bytes("to-remote", &b);
            let _ = shell.input.send(b);
        }
        Action::Local(b) => {
            crate::debuglog::log_bytes("local-echo", &b);
            let _ = console.write_all(&b);
        }
        // Tab 补全在上层调用方处理，不会走到 dispatch。
        Action::Complete(_) => {}
        Action::Run(cmd) => {
            match cmd {
                // lcd / lll 是纯本地操作，不占用传输门控。
                Command::Lcd(dir) => crate::local::lcd(dir, console),
                Command::Lll => crate::local::lll(console),
                Command::Upload(_) | Command::Download(_) => {
                    crate::debuglog::log("cmd", &format!("run {cmd:?}"));
                    if !gate.try_begin() {
                        let msg = "\u{000d}\u{000a}已有传输在进行中，请等待完成\u{000d}\u{000a}";
                        let _ = console.write_all(msg.as_bytes());
                    } else {
                        status.begin();
                        start_command(sftp, &cmd, osc.cwd(), st_tx);
                    }
                }
            }
        }
    }
}

fn start_command(
    sftp: &Option<Sftp>,
    cmd: &Command,
    cwd: Option<&str>,
    tx: &mpsc::UnboundedSender<TransferStatus>,
) {
    let Some(sftp) = sftp.clone() else {
        let _ = tx.send(TransferStatus::Failed(
            "远端未启用 SFTP 子系统，无法传输文件".to_string(),
        ));
        return;
    };
    let cwd = cwd.map(str::to_string);
    let tx = tx.clone();

    match cmd {
        Command::Upload(argv) => {
            let local = PathBuf::from(&argv[0]);
            let target = argv.get(1).cloned();
            tokio::spawn(async move {
                let mut progress = throttled(tx.clone());
                let res = sftp
                    .upload(&local, target.as_deref(), cwd.as_deref(), &mut progress)
                    .await;
                finish(tx, res);
            });
        }
        Command::Download(argv) => {
            let remote = argv[0].clone();
            let local = argv.get(1).map(PathBuf::from);
            tokio::spawn(async move {
                let mut progress = throttled(tx.clone());
                let res = sftp
                    .download(&remote, local.as_deref(), cwd.as_deref(), &mut progress)
                    .await;
                finish(tx, res);
            });
        }
        Command::Lcd(_) | Command::Lll => {
            // 纯本地操作在 dispatch 中已处理，不会走到这里。
        }
    }
}

fn throttled(tx: mpsc::UnboundedSender<TransferStatus>) -> Progress<'static> {
    let mut last: Option<Instant> = None;
    Box::new(move |name: &str, done: u64, total: u64| {
        let due = last.map_or(true, |t| t.elapsed() >= Duration::from_millis(150));
        if due || done == total {
            last = Some(Instant::now());
            let _ = tx.send(TransferStatus::Running {
                name: name.to_string(),
                done,
                total,
            });
        }
    })
}

fn finish(tx: mpsc::UnboundedSender<TransferStatus>, res: Result<TransferSummary, SshuError>) {
    let _ = tx.send(match res {
        Ok(r) => TransferStatus::Done(r),
        Err(e) => TransferStatus::Failed(e.0),
    });
}

/// 同一时间只允许一个传输（与 Python 版一致）。
#[derive(Clone, Default)]
pub struct TransferGate {
    busy: Arc<AtomicBool>,
}

impl TransferGate {
    pub fn try_begin(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn finish(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

fn bar_setup_sequence(rows: u16) -> Option<String> {
    if rows < 2 {
        return None;
    }
    // 只设滚动区（最后一行留给状态栏）；光标留在当前位置，
    // 远端 MOTD/提示符从那里继续输出。
    Some(format!("\x1b[1;{}r", rows - 1))
}

struct Bar {
    cols: u16,
    rows: u16,
    active: bool,
}

impl Bar {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            active: false,
        }
    }

    fn setup(&mut self, console: &Console) {
        let Some(seq) = bar_setup_sequence(self.rows) else {
            return;
        };
        let _ = console.write_all(seq.as_bytes());
        self.active = true;
    }

    fn draw(&self, console: &Console, text: &str) {
        let width = self.cols.saturating_sub(1) as usize;
        if self.active {
            let t: String = text.chars().take(width).collect();
            let seq = format!("\x1b7\x1b[{};1H\x1b[K{}\x1b8", self.rows, t);
            let _ = console.write_all(seq.as_bytes());
        } else {
            let mut line: String = text.chars().take(width).collect();
            while line.chars().count() < width {
                line.push(' ');
            }
            let _ = console.write_all(format!("\r{line}\r").as_bytes());
        }
    }

    fn teardown(&self, console: &Console) {
        if self.active {
            let seq = format!("\x1b[r\x1b[{};1H\x1b[K", self.rows);
            let _ = console.write_all(seq.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_allows_one_transfer_at_a_time() {
        let gate = TransferGate::default();
        assert!(gate.try_begin());
        assert!(!gate.try_begin());
        gate.finish();
        assert!(gate.try_begin());
    }

    #[test]
    fn bar_setup_only_sets_scroll_region() {
        let seq = bar_setup_sequence(30).expect("rows >= 2");
        assert_eq!(seq, "\x1b[1;29r");
        assert!(bar_setup_sequence(1).is_none());
    }

    #[test]
    fn smkx_detection_not_confused_by_other_modes() {
        // smkx/1049 触发全屏模式；其他 ?1xxx 序列不得误匹配。
        assert!(contains(b"\x1b[?1h\x1b=", b"\x1b[?1h"));
        assert!(contains(b"\x1b[?1049h", b"\x1b[?1049h"));
        assert!(!contains(b"\x1b[?1049h", b"\x1b[?1h"));
        assert!(!contains(b"\x1b[?1034h", b"\x1b[?1h"));
        assert!(contains(b"\x1b[?1l\x1b>", b"\x1b[?1l"));
        assert!(!contains(b"\x1b[?12l", b"\x1b[?1l"));
        assert!(!contains(b"\x1b[?1000l", b"\x1b[?1l"));
        assert!(!contains(b"\x1b[?1049l", b"\x1b[?1l"));
    }
}
