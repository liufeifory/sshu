use std::time::Duration;

use sshu::hook::hook_command;
use sshu::osc7::Osc7Tracker;
use sshu::sftp::{HookOutcome, Sftp};
use sshu::ssh::{connect, probe_shell, spawn_shell, ConnectOptions, ShellEvent};
use tokio::time::timeout;

fn opts() -> ConnectOptions {
    ConnectOptions {
        host: std::env::var("SSHU_TEST_HOST").unwrap_or_else(|_| "192.168.32.129".into()),
        port: std::env::var("SSHU_TEST_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(22),
        user: std::env::var("SSHU_TEST_USER").unwrap_or_else(|_| "root".into()),
        password: std::env::var("SSHU_TEST_PASS").unwrap_or_else(|_| "111111".into()),
        timeout_secs: 15,
        strict_host_key: false,
    }
}

#[tokio::test]
#[ignore]
async fn hook_install_and_cwd_tracking() {
    let handle = connect(opts()).await.unwrap();
    let sftp = Sftp::open(&handle).await.unwrap();
    let shell = probe_shell(&handle).await;
    let cmd = match sftp.install_hook(shell.as_deref()).await {
        HookOutcome::Installed(c) => c,
        other => panic!("hook install failed: {other:?}"),
    };
    assert!(hook_command(shell.as_deref()).is_some());

    let shell_conn = spawn_shell(&handle, Some(cmd), 100, 30).await.unwrap();
    let (mut events, input) = (shell_conn.events, shell_conn.input);
    let mut tracker = Osc7Tracker::new();

    let got = timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(ShellEvent::Data(d)) => {
                    tracker.feed(&d);
                    if tracker.cwd() == Some("/root") {
                        return true;
                    }
                }
                Some(ShellEvent::Eof) | Some(ShellEvent::Error(_)) | None => return false,
            }
        }
    })
    .await;
    assert_eq!(got, Ok(true), "cwd={:?}", tracker.cwd());

    let _ = input.send(b"cd /tmp && pwd\r".to_vec());
    let got2 = timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(ShellEvent::Data(d)) => {
                    tracker.feed(&d);
                    if tracker.cwd() == Some("/tmp") {
                        return true;
                    }
                }
                Some(ShellEvent::Eof) | Some(ShellEvent::Error(_)) | None => return false,
            }
        }
    })
    .await;
    assert_eq!(got2, Ok(true), "cwd={:?}", tracker.cwd());
    let _ = input.send(b"exit\r".to_vec());
}

#[tokio::test]
#[ignore]
async fn mkdir_p_and_conflict() {
    let handle = connect(opts()).await.unwrap();
    let s = Sftp::open(&handle).await.unwrap();
    let _ = s.remove_tree_silent("/tmp/sshu_sftp_it").await;
    s.mkdir_p("/tmp/sshu_sftp_it/a/b").await.unwrap();
    s.write_small_file("/tmp/sshu_sftp_it/blocker", b"x").await.unwrap();
    let err = s.mkdir_p("/tmp/sshu_sftp_it/blocker/sub").await.unwrap_err();
    assert!(err.0.contains("不是目录"), "{}", err.0);
    let _ = s.remove_tree_silent("/tmp/sshu_sftp_it").await;
}

#[tokio::test]
#[ignore]
async fn missing_subsystem_is_clear() {
    let handle = connect(opts()).await.unwrap();
    let res = Sftp::open_subsystem(&handle, "definitely-not-sftp").await;
    match res {
        Ok(_) => panic!("不存在的 subsystem 应当失败"),
        Err(e) => assert!(e.0.contains("SFTP"), "{}", e.0),
    }
}
