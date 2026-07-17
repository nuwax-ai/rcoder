//! computer_ws 共享 helper: 目录移动 / 临时目录命名 / find_dir / now_nanos /
//! remove_top_level_dir (单顶层目录上提)。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::AppResult;

/// 单顶层目录上提一层 (对齐 nuwax removeTopLevelDir / removeTopLevelFolder):
/// 过滤隐藏项 (`.` 开头) + `node_modules` + extra 噪声名, 若仅剩 1 个目录, 则内容上提。
pub async fn remove_top_level_dir(dir: &Path, extra_excludes: &[&str]) {
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    let mut filtered: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if name == "node_modules" || extra_excludes.contains(&name.as_str()) {
            continue;
        }
        if let Ok(ft) = entry.file_type().await
            && ft.is_dir()
        {
            filtered.push(entry.path());
        }
    }
    if filtered.len() != 1 {
        return;
    }
    let Some(only) = filtered.into_iter().next() else {
        return;
    };
    // 唯一顶层目录内容上提: rename 到临时名, 再逐项 rename 回 dir
    let staging = dir.join(format!(".toplift_{}", now_nanos()));
    if fs::rename(&only, &staging).await.is_err() {
        return;
    }
    if let Ok(mut rd) = fs::read_dir(&staging).await {
        while let Ok(Some(child)) = rd.next_entry().await {
            let name = child.file_name();
            let _ = move_dir(&child.path(), &dir.join(&name)).await;
        }
    }
    let _ = fs::remove_dir_all(&staging).await;
}

/// 移动目录 (rename; 跨设备 fallback copy + rm, 对齐 nuwax moveDirectory EXDEV 降级)。
pub(super) async fn move_dir(src: &Path, dst: &Path) -> AppResult<()> {
    if fs::rename(src, dst).await.is_err() {
        // rename 失败 (跨设备 EXDEV / 其他) → copy + rm; copy 层会抛真实错误
        crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await?;
        let _ = fs::remove_dir_all(src).await;
    }
    Ok(())
}

/// 在 base 的父(或自身)目录下建临时目录名 (尽量同设备, 便于 rename)。
pub(super) fn temp_sibling(base: &Path, prefix: &str) -> PathBuf {
    let parent = base.parent().unwrap_or_else(|| Path::new("/tmp"));
    parent.join(format!(".{prefix}_{}", now_nanos()))
}

/// 在 root 下查找 `name` 目录: 优先 root/name, 再查一层子目录 name/ (对齐 nuwax findDir)。
pub(super) fn find_dir(root: &Path, name: &str) -> Option<PathBuf> {
    let direct = root.join(name);
    if direct.is_dir() {
        return Some(direct);
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let sub = entry.path().join(name);
            if sub.is_dir() {
                return Some(sub);
            }
        }
    }
    None
}

/// 当前时间纳秒 (仅用于生成唯一临时名; 避免直接 `new Date`)。
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
        remove_top_level_dir(&tmp, &[]).await;
        assert!(tmp.join("a.txt").exists());
        assert!(tmp.join("deep").is_dir());
        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn remove_top_level_dir_noop_on_multi() {
        let tmp = std::env::temp_dir().join(format!("fs_rtm_{}", now_nanos()));
        fs::create_dir_all(tmp.join("a")).await.unwrap();
        fs::create_dir_all(tmp.join("b")).await.unwrap();
        remove_top_level_dir(&tmp, &[]).await;
        assert!(tmp.join("a").is_dir());
        assert!(tmp.join("b").is_dir());
        let _ = fs::remove_dir_all(&tmp).await;
    }
}
