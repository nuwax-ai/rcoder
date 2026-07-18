//! import-project (对齐 nuwax computerFileUtils.importProject):
//! 解压 zip → removeTopLevelDir → 白名单保留合并到工作区 (失败从备份回滚)。

use std::path::Path;

use tokio::fs;

use crate::error::{AppError, AppResult};

use super::IMPORT_PRESERVED;
use super::helpers::{move_dir, remove_top_level_dir, temp_sibling};

pub struct ImportResult {
    pub user_id: String,
    pub cid: String,
    pub target_dir: String,
}

/// import-project 核心 (对齐 nuwax importProject):
/// 1. 写临时 zip + 解压到 extractRoot (失败则不动 target)
/// 2. `remove_top_level_dir` (单顶层目录上提一层)
/// 3. 备份 target 非白名单条目 → backupDir
/// 4. 清空 target 非白名单条目
/// 5. 合并 extractRoot → target (跳过白名单名), 失败则从 backupDir 回滚
/// 6. 清理 backupDir + extractRoot
pub async fn import_project(target_dir: &Path, zip_data: Vec<u8>) -> AppResult<ImportResult> {
    // 临时解压目录 (target 父目录下, 便于同设备 rename)
    let extract_root = temp_sibling(target_dir, "import_extract");
    fs::create_dir_all(&extract_root).await?;
    // ZIP 必须放在解压目录之外，否则会被 merge_extracted 当作项目文件复制到工作区。
    let tmp_zip = temp_sibling(target_dir, "import_source").with_extension("zip");
    // 解压 (zip::extract_to 内部 safe_zip_entry 防穿越)
    let extract_res: AppResult<()> = async {
        fs::write(&tmp_zip, &zip_data).await?;
        crate::service::zip::extract_to(tmp_zip.clone(), extract_root.clone()).await
    }
    .await;
    let remove_zip_result = fs::remove_file(&tmp_zip).await;
    if let Err(e) = extract_res {
        let _ = fs::remove_dir_all(&extract_root).await;
        return Err(e);
    }
    if let Err(error) = remove_zip_result
        && error.kind() != std::io::ErrorKind::NotFound
    {
        let _ = fs::remove_dir_all(&extract_root).await;
        return Err(AppError::system(format!(
            "remove import archive {}: {error}",
            tmp_zip.display()
        )));
    }
    // 单顶层目录上提
    if let Err(error) = remove_top_level_dir(&extract_root, &[]).await {
        let _ = fs::remove_dir_all(&extract_root).await;
        return Err(error);
    }

    // 备份 target 非白名单条目 (移动到 backupDir)
    let backup_dir = temp_sibling(target_dir, "import_backup");
    let _ = fs::remove_dir_all(&backup_dir).await;
    backup_except_preserved(target_dir, &backup_dir).await?;

    // 清空 target 非白名单条目 (此时 target 仅剩白名单)
    clear_except_preserved(target_dir).await?;

    // 合并 extractRoot → target (跳过白名单名), 失败回滚
    let merge_res = merge_extracted(&extract_root, target_dir).await;
    if let Err(merge_err) = merge_res {
        // 回滚: 清空刚合并的非白名单条目 + 从备份恢复
        let _ = clear_except_preserved(target_dir).await;
        let _ = restore_from_backup(&backup_dir, target_dir).await;
        let _ = fs::remove_dir_all(&extract_root).await;
        let _ = fs::remove_dir_all(&backup_dir).await;
        return Err(merge_err);
    }
    // 成功: 清理
    let _ = fs::remove_dir_all(&extract_root).await;
    let _ = fs::remove_dir_all(&backup_dir).await;

    Ok(ImportResult {
        user_id: String::new(),
        cid: String::new(),
        target_dir: target_dir.to_string_lossy().to_string(),
    })
}

/// 把 target 非白名单条目移动到 backup_dir (对齐 nuwax backupWorkspaceExceptPreserved)。
async fn backup_except_preserved(target: &Path, backup_dir: &Path) -> AppResult<()> {
    fs::create_dir_all(backup_dir).await?;
    let mut entries = fs::read_dir(target).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if IMPORT_PRESERVED.contains(&name.as_str()) {
            continue;
        }
        let src = entry.path();
        let dst = backup_dir.join(&name);
        move_dir(&src, &dst).await?;
    }
    Ok(())
}

/// 清空 target 非白名单条目 (对齐 nuwax clearWorkspaceExceptPreserved)。
async fn clear_except_preserved(target: &Path) -> AppResult<()> {
    let mut entries = fs::read_dir(target).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if IMPORT_PRESERVED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type().await?;
        let r = if ft.is_dir() {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        };
        // 不存在视为已清 (NotFound 不计错)
        if let Err(e) = r
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(AppError::system(format!("clear {}: {e}", path.display())));
        }
    }
    Ok(())
}

/// 合并 extract_root 内容到 target (跳过白名单名, 对齐 nuwax mergeExtractedIntoWorkspace)。
async fn merge_extracted(extract_root: &Path, target: &Path) -> AppResult<()> {
    fs::create_dir_all(target).await?;
    let mut entries = fs::read_dir(extract_root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        // 跳过白名单名 (保留 target 已有的 .git/.agents 等)
        if IMPORT_PRESERVED.contains(&name.as_str()) {
            continue;
        }
        let src = entry.path();
        let dst = target.join(&name);
        move_dir(&src, &dst).await?;
    }
    Ok(())
}

/// 从 backup_dir 恢复非白名单条目到 target (对齐 nuwax restoreWorkspaceFromBackup)。
async fn restore_from_backup(backup_dir: &Path, target: &Path) -> AppResult<()> {
    if !crate::service::fs_util::path_exists(backup_dir).await? {
        return Ok(());
    }
    fs::create_dir_all(target).await?;
    let mut entries = fs::read_dir(backup_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if IMPORT_PRESERVED.contains(&name.as_str()) {
            continue;
        }
        let src = entry.path();
        let dst = target.join(&name);
        move_dir(&src, &dst).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn import_project_preserves_dotgit() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_imp_{}",
            crate::service::computer_ws::helpers::now_nanos()
        ));
        // 现有工作区含 .git (需保留) 和 old.txt (应被覆盖)
        fs::create_dir_all(tmp.join(".git").join("refs"))
            .await
            .unwrap();
        fs::write(tmp.join(".git").join("HEAD"), "ref")
            .await
            .unwrap();
        fs::write(tmp.join("old.txt"), "old").await.unwrap();
        // 打包新 zip: src/ 下含 new.txt (单顶层 src → 上提)
        let zip_root = std::env::temp_dir().join(format!(
            "fs_zip_{}",
            crate::service::computer_ws::helpers::now_nanos()
        ));
        fs::create_dir_all(zip_root.join("src")).await.unwrap();
        fs::write(zip_root.join("src").join("new.txt"), "new")
            .await
            .unwrap();
        // 输出 ZIP 必须位于被打包目录之外，避免把 ZIP 自身递归打进测试夹具。
        let zip_path = zip_root.with_extension("zip");
        crate::service::zip::pack_dir(zip_root.clone(), zip_path.clone(), Vec::new(), Vec::new())
            .await
            .unwrap();
        let zip_data = fs::read(&zip_path).await.unwrap();
        let _ = import_project(&tmp, zip_data).await.unwrap();
        // .git 保留, old.txt 被移除, new.txt 出现
        assert!(tmp.join(".git").join("HEAD").exists());
        assert!(!tmp.join("old.txt").exists());
        assert!(tmp.join("new.txt").exists());
        assert!(!tmp.join("src.zip").exists());
        let _ = fs::remove_dir_all(&tmp).await;
        let _ = fs::remove_dir_all(&zip_root).await;
        let _ = fs::remove_file(&zip_path).await;
    }
}
