//! 子项目日志轮转写入器。
//!
//! 从子进程 stdout/stderr pipe 读行 → 写到带大小轮转的日志文件。
//! 文件结构：`<dir>.out.log`（当前）→ `<dir>.out.1.log`（上一次）→ `<dir>.out.2.log`（更早，最多 N 个）。
//! 写模式：append（不 truncate，重启不丢历史日志）。

use std::path::{Path, PathBuf};

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::warn;

/// 默认单文件最大 10MB。
const DEFAULT_MAX_SIZE: u64 = 10 * 1024 * 1024;
/// 默认保留 3 个轮转文件。
const DEFAULT_MAX_BACKUPS: usize = 3;

/// 从 `pipe`（子进程 stdout/stderr）逐行读 → 写到 `base_path` 带轮转的文件。
pub async fn pipe_to_rotating_file<R>(
    pipe: R,
    base_path: PathBuf,
    max_size: Option<u64>,
    max_backups: Option<usize>,
) where
    R: AsyncRead + Unpin,
{
    let max_size = max_size.unwrap_or(DEFAULT_MAX_SIZE);
    let max_backups = max_backups.unwrap_or(DEFAULT_MAX_BACKUPS);

    let mut writer = match RotatingWriter::new(&base_path, max_size, max_backups).await {
        Ok(w) => w,
        Err(e) => {
            warn!("rotating writer init failed for {}: {e}, logs will be lost", base_path.display());
            // 读掉 pipe 内容（防止子进程 SIGPIPE），但不落盘
            let mut buf = vec![0u8; 4096];
            let mut reader = pipe;
            while reader.read(&mut buf).await.unwrap_or(0) > 0 {}
            return;
        }
    };

    let reader = BufReader::new(pipe);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        writer.write_line(&line).await;
    }
    // 管道结束（子进程退出）→ 刷盘，保证最后几行落盘（drop 不保证）
    writer.flush().await;
}

struct RotatingWriter {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max_size: u64,
    max_backups: usize,
}

impl RotatingWriter {
    async fn new(path: &Path, max_size: u64, max_backups: usize) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            size,
            max_size,
            max_backups,
        })
    }

    async fn write_line(&mut self, line: &str) {
        if self.size >= self.max_size {
            self.rotate().await;
        }
        let Some(ref mut file) = self.file else {
            return;
        };
        let data = format!("{line}\n");
        if file.write_all(data.as_bytes()).await.is_ok() {
            self.size += data.len() as u64;
        }
    }

    /// 刷盘：tokio::fs::File 的写入经 spawn_blocking，需显式 `flush` + `sync_all`
    /// 才能保证数据持久化（drop 不保证末尾写入落盘 → 会丢最后几行）。
    async fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush().await;
            let _ = f.sync_all().await;
        }
    }

    async fn rotate(&mut self) {
        // 关闭当前文件
        if let Some(mut f) = self.file.take() {
            let _ = f.flush().await;
        }

        // rename .log → .1.log → .2.log → ...（删超过 max_backups 的）
        for i in (1..self.max_backups).rev() {
            let from = rotate_path(&self.path, i);
            let to = rotate_path(&self.path, i + 1);
            let _ = tokio::fs::rename(&from, &to).await;
        }
        let backup1 = rotate_path(&self.path, 1);
        let _ = tokio::fs::rename(&self.path, &backup1).await;

        // 新建空 .log
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
        {
            Ok(new_file) => {
                self.file = Some(new_file);
                self.size = 0;
            }
            Err(e) => {
                warn!("rotate: create new {} failed: {e}", self.path.display());
                self.file = None;
            }
        }
    }
}

/// `<base>.out.log` → `<base>.out.1.log`（num=1）
fn rotate_path(base: &Path, num: usize) -> PathBuf {
    let mut s = base.to_string_lossy().to_string();
    if s.ends_with(".log") {
        s.truncate(s.len() - 4);
    }
    PathBuf::from(format!("{s}.{num}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn rotate_path_appends_number() {
        let p = Path::new("/tmp/app.out.log");
        assert_eq!(rotate_path(p, 1), PathBuf::from("/tmp/app.out.1.log"));
        assert_eq!(rotate_path(p, 2), PathBuf::from("/tmp/app.out.2.log"));
    }

    #[tokio::test]
    async fn rotating_writer_rotates_on_size() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.out.log");

        let mut writer = RotatingWriter::new(&path, 100, 3).await.unwrap();
        for i in 0..5 {
            writer.write_line(&format!("line-{i}: some content here")).await;
        }
        writer.flush().await;
        drop(writer);

        assert!(path.is_file(), "current .log exists");
        assert!(rotate_path(&path, 1).is_file(), ".1.log exists (rotated)");
        let current = std::fs::read_to_string(&path).unwrap();
        assert!(current.contains("line-4"), "current has last line: {current}");
        let backup1 = std::fs::read_to_string(rotate_path(&path, 1)).unwrap();
        assert!(
            backup1.contains("line-0") || backup1.contains("line-1"),
            ".1.log has old content: {backup1}"
        );
    }

    #[tokio::test]
    async fn rotating_writer_append_no_truncate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("app.out.log");
        std::fs::write(&path, "existing\n").unwrap();

        let mut writer = RotatingWriter::new(&path, 1_000_000, 3).await.unwrap();
        writer.write_line("new line").await;
        writer.flush().await;
        drop(writer);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("existing"), "append preserves: {content}");
        assert!(content.contains("new line"), "append adds: {content}");
    }

    #[tokio::test]
    async fn pipe_reads_all_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("svc.out.log");

        let data = "line1\nline2\nline3\n";
        let cursor = Cursor::new(data.as_bytes().to_vec());
        pipe_to_rotating_file(cursor, path.clone(), None, None).await;

        let content = std::fs::read_to_string(&path).unwrap();
        for l in &["line1", "line2", "line3"] {
            assert!(content.contains(l), "missing {l}: {content}");
        }
    }
}
