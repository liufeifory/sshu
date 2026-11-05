use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client::Handle;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::SshuError;
use crate::hook::{hook_command, shell_name, BASHRC, HOOK_SH, ZSHRC};
use crate::paths::{self, Existing};
use crate::ssh::ClientHandler;

#[derive(Debug, Clone, PartialEq)]
pub struct TransferSummary {
    pub dest: String,
    pub size: u64,
    pub files: u64,
    pub skipped: u64,
    pub seconds: f64,
}

#[derive(Debug, Clone)]
pub enum HookOutcome {
    Installed(String),
    Unsupported(String),
    Failed(String),
}

#[derive(Clone)]
pub struct Sftp {
    inner: Arc<SftpSession>,
    home: String,
}

impl Sftp {
    pub async fn open(handle: &Handle<ClientHandler>) -> Result<Self, SshuError> {
        Self::open_subsystem(handle, "sftp").await
    }

    pub async fn open_subsystem(
        handle: &Handle<ClientHandler>,
        name: &str,
    ) -> Result<Self, SshuError> {
        crate::ssh::with_timeout(30, "SFTP 初始化", async {
            let ch = handle
                .channel_open_session()
                .await
                .map_err(|e| SshuError::new(format!("无法打开 SFTP 通道: {e}")))?;
            ch.request_subsystem(true, name)
                .await
                .map_err(|_| SshuError::new("远端未启用 SFTP 子系统"))?;
            let sftp = SftpSession::new(ch.into_stream()).await.map_err(|e| {
                SshuError::new(format!("SFTP 初始化失败: {e}（远端可能未启用 SFTP 子系统）"))
            })?;
            sftp.set_timeout(30);
            let home = sftp
                .canonicalize(".")
                .await
                .unwrap_or_else(|_| "/".to_string());
            Ok(Self {
                inner: Arc::new(sftp),
                home,
            })
        })
        .await
    }

    pub fn session(&self) -> Arc<SftpSession> {
        Arc::clone(&self.inner)
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    pub async fn stat_kind(&self, path: &str) -> Result<Existing, SshuError> {
        // 跟随符号链接（与 Python 的 sftp.stat 一致）；只有确实不存在才是 Missing。
        match self.inner.metadata(path).await {
            Ok(m) => Ok(if m.is_dir() {
                Existing::Dir
            } else {
                Existing::File
            }),
            Err(SftpError::Status(st)) if st.status_code == StatusCode::NoSuchFile => {
                Ok(Existing::Missing)
            }
            Err(e) => Err(SshuError::new(format!("无法读取远端路径 {path}: {e}"))),
        }
    }

    pub async fn mkdir_p(&self, path: &str) -> Result<(), SshuError> {
        let abs = path.starts_with('/');
        let mut cur = if abs { "/".to_string() } else { String::new() };
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if cur.is_empty() || cur.ends_with('/') {
                cur.push_str(part);
            } else {
                cur.push('/');
                cur.push_str(part);
            }
            match self.inner.metadata(&cur).await {
                Ok(m) => {
                    if !m.is_dir() {
                        return Err(SshuError::new(format!("远端路径 {cur} 已存在且不是目录")));
                    }
                }
                Err(_) => {
                    self.inner
                        .create_dir(&cur)
                        .await
                        .map_err(|e| SshuError::new(format!("无法创建远端目录 {cur}: {e}")))?;
                }
            }
        }
        Ok(())
    }

    pub async fn write_small_file(&self, path: &str, data: &[u8]) -> Result<(), SshuError> {
        let mut f = self
            .inner
            .create(path)
            .await
            .map_err(|e| SshuError::new(format!("无法写入远端文件 {path}: {e}")))?;
        f.write_all(data)
            .await
            .map_err(|e| SshuError::new(format!("无法写入远端文件 {path}: {e}")))?;
        f.flush()
            .await
            .map_err(|e| SshuError::new(format!("无法写入远端文件 {path}: {e}")))?;
        f.close()
            .await
            .map_err(|e| SshuError::new(format!("无法写入远端文件 {path}: {e}")))?;
        Ok(())
    }

    pub async fn install_hook(&self, shell: Option<&str>) -> HookOutcome {
        let name = shell_name(shell);
        let Some(cmd) = hook_command(shell) else {
            return HookOutcome::Unsupported(shell.unwrap_or("").to_string());
        };
        let base = format!("{}/.sshu", self.home.trim_end_matches('/'));

        let res = async {
            self.mkdir_p(&base).await?;
            self.write_small_file(&format!("{base}/hook.sh"), HOOK_SH.as_bytes())
                .await?;
            if name == "zsh" {
                self.mkdir_p(&format!("{base}/zdotdir")).await?;
                self.write_small_file(&format!("{base}/zdotdir/.zshrc"), ZSHRC.as_bytes())
                    .await?;
            } else {
                self.write_small_file(&format!("{base}/bashrc"), BASHRC.as_bytes())
                    .await?;
            }
            Ok::<(), SshuError>(())
        }
        .await;

        match res {
            Ok(()) => HookOutcome::Installed(cmd),
            Err(e) => HookOutcome::Failed(e.0),
        }
    }

    /// 列举远端目录，返回 (文件名, 是否目录)，供 get 的 Tab 补全使用。
    pub async fn list_dir(&self, path: &str) -> Result<Vec<(String, bool)>, SshuError> {
        let rd = self
            .inner
            .read_dir(path)
            .await
            .map_err(|e| SshuError::new(format!("无法读取远端目录 {path}: {e}")))?;
        let entries: Vec<_> = rd.collect();
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            out.push((entry.file_name(), entry.file_type().is_dir()));
        }
        Ok(out)
    }

    /// 测试/清理用：递归删除远端路径，失败忽略。
    pub async fn remove_tree_silent(&self, path: &str) {
        match self.inner.symlink_metadata(path).await {
            Ok(m) if m.is_dir() => {
                if let Ok(rd) = self.inner.read_dir(path).await {
                    for entry in rd.collect::<Vec<_>>() {
                        Box::pin(self.remove_tree_silent(&entry.path())).await;
                    }
                }
                let _ = self.inner.remove_dir(path).await;
            }
            Ok(_) => {
                let _ = self.inner.remove_file(path).await;
            }
            Err(_) => {}
        }
    }

    pub async fn read_small_file(&self, path: &str) -> Result<Vec<u8>, SshuError> {
        let mut f = self
            .inner
            .open(path)
            .await
            .map_err(|e| SshuError::new(format!("无法读取远端文件 {path}: {e}")))?;
        let mut out = Vec::new();
        f.read_to_end(&mut out)
            .await
            .map_err(|e| SshuError::new(format!("无法读取远端文件 {path}: {e}")))?;
        Ok(out)
    }


    pub fn local_existing(raw: &str) -> Existing {
        let expanded = paths::expand_home(raw);
        match std::fs::metadata(&expanded) {
            Ok(m) if m.is_dir() => Existing::Dir,
            Ok(_) => Existing::File,
            Err(_) => Existing::Missing,
        }
    }

    pub fn local_is_dir(path: &Path) -> Existing {
        match std::fs::metadata(path) {
            Ok(m) if m.is_dir() => Existing::Dir,
            Ok(_) => Existing::File,
            Err(_) => Existing::Missing,
        }
    }
}

pub type Progress<'a> = Box<dyn FnMut(&str, u64, u64) + Send + 'a>;

impl Sftp {
    pub async fn upload(
        &self,
        local: &Path,
        target: Option<&str>,
        cwd: Option<&str>,
        progress: &mut Progress<'_>,
    ) -> Result<TransferSummary, SshuError> {
        let local_path = PathBuf::from(paths::expand_home(&local.to_string_lossy()));
        let local_abs = if local_path.is_absolute() {
            local_path
        } else {
            std::env::current_dir().unwrap_or_default().join(local_path)
        };
        if !local_abs.exists() {
            return Err(SshuError::new(format!(
                "本地路径不存在: {}",
                local_abs.display()
            )));
        }
        let name = local_abs
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("upload")
            .to_string();

        let existing = match target {
            Some(t) => {
                let abs = paths::abs_remote(t, cwd, &self.home)?;
                self.stat_kind(&abs).await?
            }
            None => Existing::Missing,
        };
        let dest = paths::remote_dest(target, &name, cwd, &self.home, existing)?;
        if dest.create_dir {
            self.mkdir_p(&posix_dirname(&dest.path)).await?;
        }

        let started = Instant::now();
        let (size, files) = if local_abs.is_dir() {
            self.mkdir_p(&dest.path).await?;
            self.upload_dir(&local_abs, &dest.path, progress).await?
        } else {
            let n = self
                .put_file(&local_abs, &dest.path, &dest.path, progress)
                .await?;
            (n, 1)
        };

        Ok(TransferSummary {
            dest: dest.path,
            size,
            files,
            skipped: 0,
            seconds: started.elapsed().as_secs_f64(),
        })
    }

    async fn put_file(
        &self,
        local: &Path,
        remote: &str,
        name: &str,
        progress: &mut Progress<'_>,
    ) -> Result<u64, SshuError> {
        let mut src = tokio::fs::File::open(local)
            .await
            .map_err(|e| SshuError::new(format!("无法读取本地文件 {}: {e}", local.display())))?;
        let size = src.metadata().await.map(|m| m.len()).unwrap_or(0);
        let mut dst = self
            .inner
            .create(remote)
            .await
            .map_err(|e| SshuError::new(format!("无法写入远端文件 {remote}: {e}")))?;

        let mut buf = vec![0u8; 32 * 1024];
        let mut done = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .await
                .map_err(|e| SshuError::new(format!("读取本地文件失败: {e}")))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])
                .await
                .map_err(|e| SshuError::new(format!("写入远端文件 {remote} 失败: {e}")))?;
            done += n as u64;
            progress(name, done, size);
        }
        dst.flush()
            .await
            .map_err(|e| SshuError::new(format!("写入远端文件 {remote} 失败: {e}")))?;
        dst.close()
            .await
            .map_err(|e| SshuError::new(format!("关闭远端文件 {remote} 失败: {e}")))?;
        Ok(done)
    }

    async fn upload_dir(
        &self,
        local: &Path,
        remote: &str,
        progress: &mut Progress<'_>,
    ) -> Result<(u64, u64), SshuError> {
        let mut entries = tokio::fs::read_dir(local)
            .await
            .map_err(|e| SshuError::new(format!("无法读取本地目录 {}: {e}", local.display())))?;
        let mut size = 0u64;
        let mut count = 0u64;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| SshuError::new(format!("无法读取本地目录 {}: {e}", local.display())))?
        {
            let ft = entry
                .file_type()
                .await
                .map_err(|e| SshuError::new(format!("无法读取本地文件类型: {e}")))?;
            let name = entry.file_name().to_string_lossy().replace('\\', "/");
            let rpath = format!("{}/{}", remote.trim_end_matches('/'), name);
            if ft.is_dir() {
                self.mkdir_p(&rpath).await?;
                let (s, c) = Box::pin(self.upload_dir(&entry.path(), &rpath, progress)).await?;
                size += s;
                count += c;
            } else {
                size += self
                    .put_file(&entry.path(), &rpath, &rpath, progress)
                    .await?;
                count += 1;
            }
        }
        Ok((size, count))
    }

    pub async fn download(
        &self,
        remote: &str,
        local: Option<&Path>,
        cwd: Option<&str>,
        progress: &mut Progress<'_>,
    ) -> Result<TransferSummary, SshuError> {
        let rpath = paths::abs_remote(remote, cwd, &self.home)?;
        let meta = self
            .inner
            .metadata(&rpath)
            .await
            .map_err(|_| SshuError::new(format!("远端路径不存在: {rpath}")))?;
        let name = paths::remote_name(&rpath);
        if paths::safe_local_name(&name).is_none() {
            return Err(SshuError::new(format!(
                "远端文件名不安全，拒绝写入本地: {name}"
            )));
        }
        let local_raw = local.map(|p| paths::expand_home(&p.to_string_lossy()));
        let existing = match &local_raw {
            Some(s) => Sftp::local_existing(s),
            None => Existing::Missing,
        };
        let startup = std::env::current_dir().unwrap_or_default();
        let dest = paths::local_dest(local_raw.as_deref(), &name, &startup, existing);
        if dest.create_dir {
            if let Some(parent) = dest.path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| SshuError::new(format!("无法创建本地目录: {e}")))?;
            }
        }

        let started = Instant::now();
        let (size, files, skipped) = if meta.is_dir() {
            tokio::fs::create_dir_all(&dest.path)
                .await
                .map_err(|e| SshuError::new(format!("无法创建本地目录: {e}")))?;
            self.download_dir(&rpath, &dest.path, progress).await?
        } else {
            if let Some(parent) = dest.path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| SshuError::new(format!("无法创建本地目录: {e}")))?;
            }
            let n = self
                .get_file(&rpath, &dest.path, &rpath, progress, meta.mtime)
                .await?;
            (n, 1, 0)
        };

        Ok(TransferSummary {
            dest: dest.path.display().to_string(),
            size,
            files,
            skipped,
            seconds: started.elapsed().as_secs_f64(),
        })
    }

    async fn get_file(
        &self,
        remote: &str,
        local: &Path,
        name: &str,
        progress: &mut Progress<'_>,
        mtime: Option<u32>,
    ) -> Result<u64, SshuError> {
        let mut src = self
            .inner
            .open(remote)
            .await
            .map_err(|e| SshuError::new(format!("无法读取远端文件 {remote}: {e}")))?;
        let size = src.metadata().await.map(|m| m.size.unwrap_or(0)).unwrap_or(0);
        let mut dst = tokio::fs::File::create(local)
            .await
            .map_err(|e| SshuError::new(format!("无法写入本地文件 {}: {e}", local.display())))?;

        let mut buf = vec![0u8; 32 * 1024];
        let mut done = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .await
                .map_err(|e| SshuError::new(format!("读取远端文件 {remote} 失败: {e}")))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])
                .await
                .map_err(|e| SshuError::new(format!("写入本地文件失败: {e}")))?;
            done += n as u64;
            progress(name, done, size);
        }
        dst.flush()
            .await
            .map_err(|e| SshuError::new(format!("写入本地文件失败: {e}")))?;
        drop(dst);

        if let Some(m) = mtime {
            let t = std::time::UNIX_EPOCH + Duration::from_secs(m as u64);
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(local) {
                let _ = f.set_modified(t);
            }
        }
        Ok(done)
    }

    async fn download_dir(
        &self,
        remote: &str,
        local: &Path,
        progress: &mut Progress<'_>,
    ) -> Result<(u64, u64, u64), SshuError> {
        let rd = self
            .inner
            .read_dir(remote)
            .await
            .map_err(|e| SshuError::new(format!("无法读取远端目录 {remote}: {e}")))?;
        let entries: Vec<_> = rd.collect();

        let mut size = 0u64;
        let mut count = 0u64;
        let mut skipped = 0u64;
        for entry in entries {
            let raw_name = entry.file_name();
            let Some(safe_name) = paths::safe_local_name(&raw_name) else {
                skipped += 1;
                continue;
            };
            let name = safe_name.to_string();
            let rpath = entry.path();
            let lpath = local.join(&name);
            let ft = entry.file_type();
            if ft.is_dir() {
                tokio::fs::create_dir_all(&lpath)
                    .await
                    .map_err(|e| SshuError::new(format!("无法创建本地目录: {e}")))?;
                let (s, c, k) = Box::pin(self.download_dir(&rpath, &lpath, progress)).await?;
                size += s;
                count += c;
                skipped += k;
            } else if ft.is_symlink() {
                skipped += 1;
            } else {
                let m = entry.metadata();
                size += self
                    .get_file(&rpath, &lpath, &rpath, progress, m.mtime)
                    .await?;
                count += 1;
            }
        }
        Ok((size, count, skipped))
    }
}

fn posix_dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => path[..i].to_string(),
        None => ".".to_string(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_home_expansion() {
        assert_eq!(Sftp::local_existing("~"), Existing::Dir);
        assert_eq!(
            Sftp::local_existing("~/definitely_missing_sshu_xyz"),
            Existing::Missing
        );
    }
}
