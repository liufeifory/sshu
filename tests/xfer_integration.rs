use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use sshu::sftp::{Progress, Sftp};
use sshu::ssh::{connect, ConnectOptions};
use tokio::time::Duration;

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

async fn run_exec(handle: &russh::client::Handle<sshu::ssh::ClientHandler>, cmd: &str) -> String {
    let ch = handle.channel_open_session().await.unwrap();
    ch.exec(true, cmd).await.unwrap();
    let (mut read, _w) = ch.split();
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), read.wait()).await {
            Ok(Some(russh::ChannelMsg::Data { data })) => out.extend_from_slice(&data),
            Ok(Some(russh::ChannelMsg::Eof)) | Ok(Some(russh::ChannelMsg::Close)) | Ok(None) => break,
            Ok(Some(_)) => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn temp_local(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("sshu_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
#[ignore]
async fn file_roundtrip_progress_and_mtime() {
    let handle = connect(opts()).await.unwrap();
    let s = Sftp::open(&handle).await.unwrap();
    let remote_root = format!("/tmp/sshu_xfer_file_{}", std::process::id());
    s.remove_tree_silent(&remote_root).await;
    let tmp = temp_local("xfer1");
    let src = tmp.join("hello.txt");
    std::fs::write(&src, b"hello sshu\n").unwrap();

    let log = Arc::new(Mutex::new(Vec::<(String, u64, u64)>::new()));
    let log2 = Arc::clone(&log);
    let mut prog: Progress = Box::new(move |n, d, t| {
        log2.lock().unwrap().push((n.to_string(), d, t));
    });

    let target = format!("{remote_root}/");
    let up = s
        .upload(&src, Some(target.as_str()), None, &mut prog)
        .await
        .unwrap();
    assert_eq!(up.dest, format!("{remote_root}/hello.txt"));
    assert_eq!(s.read_small_file(&format!("{remote_root}/hello.txt")).await.unwrap(), b"hello sshu\n");
    assert!(!log.lock().unwrap().is_empty(), "progress callback did not fire");

    let touched = run_exec(
        &handle,
        &format!("touch -d @1600000000 {remote_root}/hello.txt && echo TOUCHED"),
    )
    .await;
    assert!(touched.contains("TOUCHED"), "{touched}");

    let dl_dir = tmp.join("dl");
    let dl_raw = format!("{}\\", dl_dir.display());
    let dl = s
        .download(
            &format!("{remote_root}/hello.txt"),
            Some(Path::new(&dl_raw)),
            None,
            &mut prog,
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dl.dest).unwrap(), b"hello sshu\n");
    let mtime = std::fs::metadata(&dl.dest)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!((mtime as i64 - 1_600_000_000).abs() <= 2, "mtime={mtime}");

    s.remove_tree_silent(&remote_root).await;
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
#[ignore]
async fn dir_unicode_bigfile_and_symlink_skip() {
    let handle = connect(opts()).await.unwrap();
    let s = Sftp::open(&handle).await.unwrap();
    let remote_root = format!("/tmp/sshu_xfer_dir_{}", std::process::id());
    s.remove_tree_silent(&remote_root).await;

    let tmp = temp_local("xfer2");
    let pkg = tmp.join("pkg");
    std::fs::create_dir_all(pkg.join("sub")).unwrap();
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(pkg.join("sub").join("a.bin"), &data).unwrap();
    std::fs::write(pkg.join("中文 名称.txt"), "你好\n".as_bytes()).unwrap();

    let mut prog: Progress = Box::new(|_, _, _| {});
    let target = format!("{remote_root}/");
    let up = s
        .upload(&pkg, Some(target.as_str()), None, &mut prog)
        .await
        .unwrap();
    assert_eq!(up.dest, format!("{remote_root}/pkg"));
    assert_eq!(
        s.read_small_file(&format!("{remote_root}/pkg/sub/a.bin")).await.unwrap(),
        data
    );

    run_exec(&handle, &format!("ln -sf a.bin {remote_root}/pkg/sub/link.bin")).await;

    let out = tmp.join("out");
    let out_raw = format!("{}\\", out.display());
    let dl = s
        .download(
            &format!("{remote_root}/pkg"),
            Some(Path::new(&out_raw)),
            None,
            &mut prog,
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(out.join("pkg").join("sub").join("a.bin")).unwrap(), data);
    assert_eq!(
        std::fs::read_to_string(out.join("pkg").join("中文 名称.txt")).unwrap(),
        "你好\n"
    );
    assert!(dl.skipped >= 1, "skipped={}", dl.skipped);
    assert!(!out.join("pkg").join("sub").join("link.bin").exists());

    s.remove_tree_silent(&remote_root).await;
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
#[ignore]
async fn missing_remote_errors() {
    let handle = connect(opts()).await.unwrap();
    let s = Sftp::open(&handle).await.unwrap();
    let mut prog: Progress = Box::new(|_, _, _| {});
    let err = s
        .download("/tmp/sshu_no_such_path", None, None, &mut prog)
        .await
        .unwrap_err();
    assert!(err.0.contains("不存在"), "{}", err.0);
}

#[tokio::test]
#[ignore]
async fn upload_through_symlinked_dir() {
    let handle = connect(opts()).await.unwrap();
    let s = Sftp::open(&handle).await.unwrap();
    let root = format!("/tmp/sshu_link_{}", std::process::id());
    s.remove_tree_silent(&root).await;
    run_exec(
        &handle,
        &format!("mkdir -p {root}/real && ln -s real {root}/link"),
    )
    .await;

    let tmp = temp_local("link");
    let src = tmp.join("index.html");
    std::fs::write(&src, b"ok").unwrap();
    let mut prog: Progress = Box::new(|_, _, _| {});
    let target = format!("{root}/link");
    let up = s
        .upload(&src, Some(target.as_str()), None, &mut prog)
        .await
        .unwrap();
    assert_eq!(up.dest, format!("{root}/link/index.html"));
    assert_eq!(
        s.read_small_file(&format!("{root}/link/index.html")).await.unwrap(),
        b"ok"
    );

    s.remove_tree_silent(&root).await;
    let _ = std::fs::remove_dir_all(&tmp);
}
