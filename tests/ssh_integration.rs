use std::time::Duration;

use sshu::ssh::{connect, probe_shell, spawn_shell, ConnectOptions, ShellEvent};
use tokio::time::timeout;

fn opts() -> ConnectOptions {
    ConnectOptions {
        host: std::env::var("SSHU_TEST_HOST").unwrap_or_else(|_| "192.168.32.129".into()),
        port: std::env::var("SSHU_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(22),
        user: std::env::var("SSHU_TEST_USER").unwrap_or_else(|_| "root".into()),
        password: std::env::var("SSHU_TEST_PASS").unwrap_or_else(|_| "111111".into()),
        timeout_secs: 15,
        strict_host_key: false,
    }
}

#[tokio::test]
#[ignore]
async fn connects_and_runs_command() {
    let handle = connect(opts()).await.unwrap();
    let ch = handle.channel_open_session().await.unwrap();
    ch.exec(true, "printf 'OK:%s' \"$SHELL\"").await.unwrap();
    let (mut read, _write) = ch.split();
    let mut out = Vec::new();
    while let Some(msg) = read.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => out.extend_from_slice(&data),
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    assert!(
        String::from_utf8_lossy(&out).starts_with("OK:/"),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
}

#[tokio::test]
#[ignore]
async fn wrong_password_fails_cleanly() {
    let mut o = opts();
    o.password = "definitely-wrong".into();
    let res = connect(o).await;
    match res {
        Ok(_) => panic!("认证应当失败"),
        Err(e) => assert!(e.0.contains("认证失败"), "{}", e.0),
    }
}

#[tokio::test]
#[ignore]
async fn probe_shell_reports_bash() {
    let handle = connect(opts()).await.unwrap();
    let shell = probe_shell(&handle).await.expect("probe_shell returned None");
    assert!(shell.contains("bash") || shell.contains("sh"));
}

#[tokio::test]
#[ignore]
async fn shell_streams_output() {
    let handle = connect(opts()).await.unwrap();
    let shell = spawn_shell(handle, None, 100, 30).await.unwrap();
    let (mut events, input) = (shell.events, shell.input);
    let got = timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(ShellEvent::Data(d)) if !d.is_empty() => return true,
                Some(ShellEvent::Eof) | Some(ShellEvent::Error(_)) | None => return false,
                _ => {}
            }
        }
    })
    .await;
    assert_eq!(got, Ok(true));
    let _ = input.send(b"exit\r".to_vec());
}

#[tokio::test]
#[ignore]
async fn shell_reports_eof_on_exit() {
    let handle = connect(opts()).await.unwrap();
    let shell = spawn_shell(handle, None, 80, 24).await.unwrap();
    let (mut events, input) = (shell.events, shell.input);
    // wait for first prompt data, then exit
    let _ = timeout(Duration::from_secs(10), async {
        while let Some(ev) = events.recv().await {
            if let ShellEvent::Data(d) = ev {
                if !d.is_empty() {
                    return;
                }
            }
        }
    })
    .await;
    let _ = input.send(b"exit\r".to_vec());
    let got = timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(ShellEvent::Eof) | None => return true,
                Some(ShellEvent::Error(_)) => return false,
                _ => {}
            }
        }
    })
    .await;
    assert_eq!(got, Ok(true), "no Eof after exit");
}

#[tokio::test]
#[ignore]
async fn exec_shell_reports_eof_on_exit() {
    let handle = connect(opts()).await.unwrap();
    let shell = spawn_shell(handle, Some("exec /bin/bash -i".to_string()), 80, 24)
        .await
        .unwrap();
    let (mut events, input) = (shell.events, shell.input);
    let _ = timeout(Duration::from_secs(10), async {
        while let Some(ev) = events.recv().await {
            if let ShellEvent::Data(d) = ev {
                if !d.is_empty() {
                    return;
                }
            }
        }
    })
    .await;
    let _ = input.send(b"exit\r".to_vec());
    let got = timeout(Duration::from_secs(8), async {
        loop {
            match events.recv().await {
                Some(ShellEvent::Eof) | None => return true,
                Some(ShellEvent::Error(_)) => return true,
                _ => {}
            }
        }
    })
    .await;
    assert_eq!(got, Ok(true), "no Eof/Error after exec shell exit");
}

#[tokio::test]
#[ignore]
async fn hook_shell_reports_eof_on_exit() {
    use sshu::sftp::{HookOutcome, Sftp};
    let handle = connect(opts()).await.unwrap();
    let sftp = Sftp::open(&handle).await.unwrap();
    let shell_path = probe_shell(&handle).await;
    let cmd = match sftp.install_hook(shell_path.as_deref()).await {
        HookOutcome::Installed(c) => c,
        other => panic!("hook: {other:?}"),
    };
    let shell = spawn_shell(handle, Some(cmd), 80, 24).await.unwrap();
    let (mut events, input) = (shell.events, shell.input);
    let _ = timeout(Duration::from_secs(10), async {
        while let Some(ev) = events.recv().await {
            if let ShellEvent::Data(d) = ev {
                if !d.is_empty() {
                    return;
                }
            }
        }
    })
    .await;
    let _ = input.send(b"exit\r".to_vec());
    let got = timeout(Duration::from_secs(8), async {
        loop {
            match events.recv().await {
                Some(ShellEvent::Eof) | None => return true,
                Some(ShellEvent::Error(_)) => return true,
                _ => {}
            }
        }
    })
    .await;
    assert_eq!(got, Ok(true), "no Eof/Error after hook shell exit");
}
