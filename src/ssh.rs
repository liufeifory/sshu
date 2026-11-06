use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{AuthResult, Config, Handle, Handler};
use russh::keys::{check_known_hosts, PublicKeyOrCertificate};
use russh::ChannelMsg;
use tokio::sync::mpsc;

use crate::error::SshuError;

impl From<russh::Error> for SshuError {
    fn from(e: russh::Error) -> Self {
        SshuError::new(format!("SSH 错误: {e}"))
    }
}

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub timeout_secs: u64,
    pub strict_host_key: bool,
}

pub struct ClientHandler {
    strict: bool,
    host: String,
    port: u16,
    fail: Arc<Mutex<Option<String>>>,
}

impl Handler for ClientHandler {
    type Error = SshuError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(_) => {
                set_fail(&self.fail, "服务器使用 SSH 证书主机键，本版未启用");
                return Ok(false);
            }
        };

        match host_key_decision(
            check_known_hosts(&self.host, self.port, key),
            self.strict,
            &self.host,
            self.port,
        ) {
            KeyDecision::Accept => Ok(true),
            KeyDecision::Reject(msg) => {
                set_fail(&self.fail, &msg);
                Ok(false)
            }
        }
    }
}

enum KeyDecision {
    Accept,
    Reject(String),
}

fn host_key_decision(
    result: Result<bool, russh::keys::Error>,
    strict: bool,
    host: &str,
    port: u16,
) -> KeyDecision {
    match result {
        Ok(true) => KeyDecision::Accept,
        Ok(false) => {
            if strict {
                KeyDecision::Reject(format!("未知主机键：{host}:{port}（--strict-host-key）"))
            } else {
                KeyDecision::Accept
            }
        }
        Err(russh::keys::Error::KeyChanged { .. }) => KeyDecision::Reject(format!(
            "主机密钥已变化，可能遭到中间人攻击：{host}:{port}"
        )),
        // known_hosts 解析/IO 错误不是“换键”，非严格模式下按未知主机处理。
        Err(e) => {
            if strict {
                KeyDecision::Reject(format!("无法校验主机键（{host}:{port}）: {e}"))
            } else {
                KeyDecision::Accept
            }
        }
    }
}

fn set_fail(fail: &Arc<Mutex<Option<String>>>, msg: &str) {
    if let Ok(mut guard) = fail.lock() {
        *guard = Some(msg.to_string());
    }
}

pub async fn with_timeout<T, F>(secs: u64, label: &str, fut: F) -> Result<T, SshuError>
where
    F: std::future::Future<Output = Result<T, SshuError>>,
{
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(r) => r,
        Err(_) => Err(SshuError::new(format!("{label}超时（{secs} 秒）"))),
    }
}

pub async fn connect(opts: ConnectOptions) -> Result<Handle<ClientHandler>, SshuError> {
    let config = Arc::new(Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    let fail = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        strict: opts.strict_host_key,
        host: opts.host.clone(),
        port: opts.port,
        fail: Arc::clone(&fail),
    };

    let fut = russh::client::connect(config, (opts.host.as_str(), opts.port), handler);
    let mut handle = match tokio::time::timeout(Duration::from_secs(opts.timeout_secs), fut).await {
        Err(_) => {
            return Err(SshuError::new(format!(
                "连接超时（{} 秒）",
                opts.timeout_secs
            )))
        }
        Ok(Err(e)) => {
            if let Some(msg) = fail.lock().ok().and_then(|g| g.clone()) {
                return Err(SshuError::new(msg));
            }
            return Err(SshuError::new(format!("连接失败: {e}")));
        }
        Ok(Ok(h)) => h,
    };

    let auth = with_timeout(opts.timeout_secs, "认证", async {
        handle
            .authenticate_password(opts.user, opts.password)
            .await
            .map_err(SshuError::from)
    })
    .await?;
    match auth {
        AuthResult::Success => Ok(handle),
        AuthResult::Failure { .. } => Err(SshuError::new("认证失败：用户名或密码不正确")),
    }
}

/// 用 exec 通道探测登录 shell；失败返回 None（钩子退化为显式目录）。
pub async fn probe_shell(handle: &Handle<ClientHandler>) -> Option<String> {
    let ch = handle.channel_open_session().await.ok()?;
    ch.exec(true, r#"printf "%s" "${SHELL:-/bin/sh}""#).await.ok()?;
    let (mut read, _write) = ch.split();

    let fut = async {
        let mut out = Vec::new();
        while let Some(msg) = read.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        out
    };
    let out = tokio::time::timeout(Duration::from_secs(10), fut).await.ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 通过 exec 通道读取交互 shell 的真实 cwd：找出该用户最新的带 tty 的
/// shell 进程（su 切换后的新 shell pid 最大），readlink /proc/<pid>/cwd。
/// 用于 `su` / `su -` 之后 OSC 7 断供时的精确 cwd 回退——只读，零注入。
pub async fn probe_cwd(handle: &Handle<ClientHandler>, user: &str) -> Option<String> {
    // user 来自提示符解析，做个白名单防注入
    if user.is_empty() || !user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        return None;
    }
    let ch = handle.channel_open_session().await.ok()?;
    let script = format!(
        r#"P=$(ps -o pid= -o tty= -o args= -u '{}' 2>/dev/null | awk '$2!~/\?/ && $3 ~ /bash|zsh|ksh|sh$/ {{print $1}}' | sort -n | tail -1); [ -n "$P" ] && readlink /proc/$P/cwd"#,
        user
    );
    ch.exec(true, script.as_str()).await.ok()?;
    let (mut read, _write) = ch.split();

    let fut = async {
        let mut out = Vec::new();
        while let Some(msg) = read.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        out
    };
    let out = tokio::time::timeout(Duration::from_secs(10), fut).await.ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.starts_with('/') {
        Some(s)
    } else {
        None
    }
}

#[derive(Debug)]
pub enum ShellEvent {
    Data(Vec<u8>),
    Eof,
    Error(String),
}

pub struct Shell {
    pub events: mpsc::UnboundedReceiver<ShellEvent>,
    pub input: mpsc::UnboundedSender<Vec<u8>>,
    pub resize: mpsc::UnboundedSender<(u16, u16)>,
}

pub async fn spawn_shell(
    handle: &Handle<ClientHandler>,
    cmd: Option<String>,
    cols: u16,
    rows: u16,
) -> Result<Shell, SshuError> {
    let ch = handle
        .channel_open_session()
        .await
        .map_err(|e| SshuError::new(format!("无法打开会话: {e}")))?;
    ch.request_pty(
        true,
        "xterm-256color",
        cols as u32,
        rows as u32,
        0,
        0,
        &[],
    )
    .await
    .map_err(|e| SshuError::new(format!("无法申请 PTY: {e}")))?;

    match cmd {
        Some(c) => ch
            .exec(true, c)
            .await
            .map_err(|e| SshuError::new(format!("无法启动 shell: {e}")))?,
        None => ch
            .request_shell(true)
            .await
            .map_err(|e| SshuError::new(format!("无法启动 shell: {e}")))?,
    }

    let (mut read, write) = ch.split();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let err_tx = ev_tx.clone();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (rs_tx, mut rs_rx) = mpsc::unbounded_channel::<(u16, u16)>();

    tokio::spawn(async move {
        while let Some(msg) = read.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if ev_tx.send(ShellEvent::Data(data.to_vec())).is_err() {
                        return;
                    }
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    if ev_tx.send(ShellEvent::Data(data.to_vec())).is_err() {
                        return;
                    }
                }
                ChannelMsg::Eof
                | ChannelMsg::Close
                | ChannelMsg::ExitStatus { .. }
                | ChannelMsg::ExitSignal { .. } => {
                    let _ = ev_tx.send(ShellEvent::Eof);
                    return;
                }
                _ => {}
            }
        }
        // 通道直接关闭（对端退出）时 wait() 返回 None，也必须通知前台。
        let _ = ev_tx.send(ShellEvent::Eof);
    });

    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(bytes) = in_rx.recv() => {
                    if write.data_bytes(bytes).await.is_err() {
                        let _ = err_tx.send(ShellEvent::Error("发送数据失败".to_string()));
                        break;
                    }
                }
                Some((w, h)) = rs_rx.recv() => {
                    let _ = write.window_change(w as u32, h as u32, 0, 0).await;
                }
                else => break,
            }
        }
    });

    Ok(Shell {
        events: ev_rx,
        input: in_tx,
        resize: rs_tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::Error as KeyError;

    #[tokio::test]
    async fn timeout_helper_fires() {
        let r: Result<(), SshuError> = with_timeout(0, "认证", async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        })
        .await;
        assert!(r.unwrap_err().0.contains("超时"));
    }

    #[test]
    fn host_key_decisions() {
        let d = |r, strict| host_key_decision(r, strict, "h", 22);
        assert!(matches!(d(Ok(true), false), KeyDecision::Accept));
        assert!(matches!(d(Ok(false), false), KeyDecision::Accept));
        match d(Ok(false), true) {
            KeyDecision::Reject(m) => assert!(m.contains("未知"), "{m}"),
            _ => panic!("strict unknown should reject"),
        }
        match d(Err(KeyError::KeyChanged { line: 1 }), false) {
            KeyDecision::Reject(m) => assert!(m.contains("已变化"), "{m}"),
            _ => panic!("changed key should reject"),
        }
        // 非 KeyChanged 的解析/IO 错误：非严格模式不应把它误判成 MITM
        assert!(matches!(
            d(Err(KeyError::NoHomeDir), false),
            KeyDecision::Accept
        ));
        match d(Err(KeyError::NoHomeDir), true) {
            KeyDecision::Reject(m) => assert!(m.contains("无法校验"), "{m}"),
            _ => panic!("strict parse error should reject"),
        }
    }
}
