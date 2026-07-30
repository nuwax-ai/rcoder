//! computer_ws 共享 helper: 目录移动 / find_dir / 测试夹具命名 /
//! remove_top_level_dir (单顶层目录上提)。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{AppError, AppResult};

/// 单顶层目录上提一层 (对齐 nuwax removeTopLevelDir / removeTopLevelFolder):
/// 过滤隐藏项 (`.` 开头) + `node_modules` + extra 噪声名, 若仅剩 1 个目录, 则内容上提。
pub async fn remove_top_level_dir(dir: &Path, extra_excludes: &[&str]) -> AppResult<()> {
    let mut entries = fs::read_dir(dir).await?;
    let mut filtered: Vec<PathBuf> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if name == "node_modules" || extra_excludes.contains(&name.as_str()) {
            continue;
        }
        // 与 TS 一致：先统计所有非噪声条目，再判断唯一条目是否为目录。
        // 普通文件也必须参与计数，否则 `project/ + README.md` 会被错误上提。
        filtered.push(entry.path());
    }
    if filtered.len() != 1 {
        return Ok(());
    }
    let Some(only) = filtered.into_iter().next() else {
        return Ok(());
    };
    if !fs::symlink_metadata(&only).await?.file_type().is_dir() {
        return Ok(());
    }
    // 唯一顶层目录内容上提: rename 到临时名, 再逐项 rename 回 dir
    let parent = dir.parent().unwrap_or(dir).to_path_buf();
    let staging_guard = crate::service::temp_file::tempdir_in(parent, ".toplift-").await?;
    let staging = staging_guard.path().join("content");
    move_dir(&only, &staging).await?;
    let mut rd = fs::read_dir(&staging).await?;
    while let Some(child) = rd.next_entry().await? {
        let name = child.file_name();
        move_dir(&child.path(), &dir.join(&name)).await?;
    }
    Ok(())
}

/// 移动目录 (rename; 跨设备 fallback copy + rm, 对齐 nuwax moveDirectory EXDEV 降级)。
pub(super) async fn move_dir(src: &Path, dst: &Path) -> AppResult<()> {
    match fs::rename(src, dst).await {
        Ok(()) => return Ok(()),
        Err(error) if error.raw_os_error() == Some(nix::libc::EXDEV) => {}
        Err(error) => {
            return Err(AppError::system(format!(
                "move {} to {}: {error}",
                src.display(),
                dst.display()
            )));
        }
    }

    let file_type = fs::symlink_metadata(src).await?.file_type();
    if file_type.is_dir() {
        crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await?;
        fs::remove_dir_all(src).await?;
    } else if file_type.is_file() {
        fs::copy(src, dst).await?;
        fs::remove_file(src).await?;
    } else {
        return Err(AppError::system(format!(
            "cannot move unsupported file type across filesystems: {}",
            src.display()
        )));
    }
    Ok(())
}

/// 在 root 下查找 `name` 目录: 优先 root/name, 再查一层子目录 name/ (对齐 nuwax findDir)。
/// async: 原 sync `std::fs` 在 async create_workspace 里直接调会阻塞 worker 线程
/// (tokio 反模式 — async 中同步阻塞 IO), 改用 tokio::fs 非阻塞。
pub(super) async fn find_dir(root: &Path, name: &str) -> Option<PathBuf> {
    let direct = root.join(name);
    if is_dir(&direct).await {
        return Some(direct);
    }
    let mut entries = fs::read_dir(root).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let sub = entry.path().join(name);
        if is_dir(&sub).await {
            return Some(sub);
        }
    }
    None
}

/// 跟随符号链接判定目录 (等价 `std::path::Path::is_dir` 的 async 版, 用 fs::metadata 跟随 symlink)。
async fn is_dir(path: &Path) -> bool {
    fs::metadata(path)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

/// 当前时间纳秒 (仅用于生成唯一临时名; 避免直接 `new Date`)。
#[cfg(test)]
pub(super) fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remove_top_level_dir_lifts_single_dir() {
        let tmp = std::env::temp_dir().join(format!("fs_rtl_{}", now_nanos()));
        fs::create_dir_all(tmp.join("only").join("deep"))
            .await
            .unwrap();
        fs::write(tmp.join("only").join("a.txt"), "x")
            .await
            .unwrap();
        remove_top_level_dir(&tmp, &[]).await.unwrap();
        assert!(tmp.join("a.txt").exists());
        assert!(tmp.join("deep").is_dir());
        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn remove_top_level_dir_noop_on_multi() {
        let tmp = std::env::temp_dir().join(format!("fs_rtm_{}", now_nanos()));
        fs::create_dir_all(tmp.join("a")).await.unwrap();
        fs::create_dir_all(tmp.join("b")).await.unwrap();
        remove_top_level_dir(&tmp, &[]).await.unwrap();
        assert!(tmp.join("a").is_dir());
        assert!(tmp.join("b").is_dir());
        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn remove_top_level_dir_noop_when_file_is_also_present() {
        let tmp = std::env::temp_dir().join(format!("fs_rtf_{}", now_nanos()));
        fs::create_dir_all(tmp.join("project")).await.unwrap();
        fs::write(tmp.join("project").join("package.json"), "{}")
            .await
            .unwrap();
        fs::write(tmp.join("README.md"), "readme").await.unwrap();

        remove_top_level_dir(&tmp, &[]).await.unwrap();

        assert!(tmp.join("project").join("package.json").is_file());
        assert!(tmp.join("README.md").is_file());
        assert!(!tmp.join("package.json").exists());
        let _ = fs::remove_dir_all(&tmp).await;
    }
}
