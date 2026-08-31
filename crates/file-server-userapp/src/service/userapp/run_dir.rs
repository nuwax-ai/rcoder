//! dev 部署运行目录（`{ws}/.run`）：制品 zip 解压 → 校验 → 原子换入。
//!
//! 与生产部署物完全一致（对齐 app-cli `deploy.rs` 的 `.staging → code` 模型）：
//! 构建产物 zip 的顶层即 workspace 根（`workspace.manifest.toml` +
//! `release.lock.toml` + 各子项目产物），解压后直接作 `app-cli --workspace`。
//! 换目录用 rename（旧 `.run` → `.previous`，staging → `.run`）——运行中进程
//! 持旧 inode 不受影响；解压/校验失败时旧 `.run` 原样保留（对齐"失败保留现场"）。

use std::path::{Path, PathBuf};

use file_server::error::{AppError, AppResult};
use file_server::service::zip;

use super::workspace_artifact_rel_path;

/// dev 运行目录名（解压产物，`app-cli --workspace` 指向这里）。
pub const RUN_DIR: &str = ".run";
/// 上一版现场（swap 时旧 `.run` rename 到此；保留一份供排障/回退参考，下次覆盖）。
pub const PREVIOUS_DIR: &str = ".previous";
/// 解压过渡目录（`{ws}/.staging/{release_id}`；swap 后为空，残留由 hygiene 清）。
pub const STAGING_DIR: &str = ".staging";

/// 把 `builds/workspace-package-{release_id}.zip` 解压换入 `{ws}/.run`。
///
/// 返回 `.run` 绝对路径。任何一步失败：`.run` 不动（旧版照常可跑），
/// staging 残留留给 hygiene 清理。
pub async fn prepare_run_dir(ws: &Path, release_id: &str) -> AppResult<PathBuf> {
    let zip_path = ws.join(workspace_artifact_rel_path(release_id));
    if !zip_path.is_file() {
        return Err(AppError::resource(format!(
            "deploy package missing: {} (release_id={release_id})",
            zip_path.display()
        )));
    }
    // 幂等：同 release_id 重复部署先清旧 staging
    let staging = ws.join(STAGING_DIR).join(release_id);
    if staging.exists() {
        tokio::fs::remove_dir_all(&staging).await.map_err(|e| {
            AppError::system(format!("clean stale staging {}: {e}", staging.display()))
        })?;
    }
    zip::extract_to(zip_path, staging.clone()).await?;

    // 校验解压物：workspace 与 release lock 双要件（app-cli 硬依赖）。
    for required in ["workspace.manifest.toml", "release.lock.toml"] {
        if !staging.join(required).is_file() {
            return Err(AppError::business(format!(
                "deploy package missing {required} at zip root (release_id={release_id})"
            )));
        }
    }

    // 原子换入：旧 .run → .previous（覆盖删旧），staging → .run。
    let run = ws.join(RUN_DIR);
    let previous = ws.join(PREVIOUS_DIR);
    if run.exists() {
        if previous.exists() {
            tokio::fs::remove_dir_all(&previous)
                .await
                .map_err(|e| AppError::system(format!("remove old {}: {e}", previous.display())))?;
        }
        tokio::fs::rename(&run, &previous).await.map_err(|e| {
            AppError::system(format!(
                "swap {} → {}: {e}",
                run.display(),
                previous.display()
            ))
        })?;
    }
    tokio::fs::rename(&staging, &run)
        .await
        .map_err(|e| AppError::system(format!("promote staging → {}: {e}", run.display())))?;
    tracing::info!(
        release_id,
        run = %run.display(),
        previous = %previous.display(),
        "[RUN_DIR] dev deploy swapped"
    );
    Ok(run)
}

/// 运行目录定位（启动用；不存在时 None——未部署过）。
pub fn run_dir_of(ws: &Path) -> PathBuf {
    ws.join(RUN_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::userapp::WORKSPACE_BUILDS_DIR;
    use std::io::Write;

    /// 造一个最小合法制品 zip（workspace.manifest.toml + release.lock.toml + start.sh）。
    fn make_package(dir: &Path, release_id: &str) -> PathBuf {
        let builds = dir.join(WORKSPACE_BUILDS_DIR);
        std::fs::create_dir_all(&builds).expect("builds dir");
        let zip_path = builds.join(format!("workspace-package-{release_id}.zip"));
        let file = std::fs::File::create(&zip_path).expect("zip file");
        let mut writer = ::zip::ZipWriter::new(file);
        let options = ::zip::write::SimpleFileOptions::default()
            .compression_method(::zip::CompressionMethod::Stored);
        writer
            .start_file("workspace.manifest.toml", options)
            .expect("start manifest");
        writer
            .write_all(b"schema_version = 1\n")
            .expect("write manifest");
        writer
            .start_file("release.lock.toml", options)
            .expect("start lock");
        writer
            .write_all(b"release_id = \"x\"\n")
            .expect("write lock");
        writer.start_file("start.sh", options).expect("start sh");
        writer.write_all(b"#!/bin/sh\n").expect("write sh");
        writer.finish().expect("finish zip");
        zip_path
    }

    #[tokio::test]
    async fn first_deploy_extracts_and_promotes_run_dir() {
        let ws = tempfile::tempdir().expect("ws");
        make_package(ws.path(), "rel-1");
        let run = prepare_run_dir(ws.path(), "rel-1").await.expect("prepare");
        assert!(run.ends_with(RUN_DIR));
        assert!(run.join("workspace.manifest.toml").is_file());
        assert!(run.join("release.lock.toml").is_file());
        assert!(run.join("start.sh").is_file());
        // staging 已被 promote 走（不存在）
        assert!(!ws.path().join(STAGING_DIR).join("rel-1").exists());
    }

    #[tokio::test]
    async fn second_deploy_rotates_previous() {
        let ws = tempfile::tempdir().expect("ws");
        make_package(ws.path(), "rel-1");
        make_package(ws.path(), "rel-2");
        prepare_run_dir(ws.path(), "rel-1").await.expect("first");
        // 在 rel-1 的 .run 里放标记文件
        std::fs::write(ws.path().join(RUN_DIR).join("marker-rel-1"), "1").expect("marker");
        let run = prepare_run_dir(ws.path(), "rel-2").await.expect("second");
        // 新 .run 来自 rel-2（无 marker），旧内容轮换进 .previous
        assert!(!run.join("marker-rel-1").exists());
        assert!(ws.path().join(PREVIOUS_DIR).join("marker-rel-1").is_file());
    }

    #[tokio::test]
    async fn missing_package_keeps_existing_run_dir_untouched() {
        let ws = tempfile::tempdir().expect("ws");
        make_package(ws.path(), "rel-1");
        prepare_run_dir(ws.path(), "rel-1").await.expect("first");
        let err = prepare_run_dir(ws.path(), "rel-missing")
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("deploy package missing"));
        // 旧 .run 原样
        assert!(ws.path().join(RUN_DIR).join("start.sh").is_file());
        assert!(!ws.path().join(PREVIOUS_DIR).exists());
    }
}
