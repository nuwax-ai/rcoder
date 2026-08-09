//! 版本化整体包 `workspace-package-<release_id>.zip` 组装。
//!
//! 各子项目 zip **直接 raw copy**（加 `{path}/` 前缀，不二次压缩）+ workspace 根入口文件
//! （`start.sh`/`scripts/`，小文件正常压缩）+ pingap 配置 + `.service-ports`。
//!
//! 性能：`ZipWriter::raw_copy_file_rename` 搬运子项目 zip 条目的原始压缩字节 + 元数据
//! （CRC/size/压缩方法），避免「解压到磁盘 → 再压缩」（Next.js standalone 含 node_modules，
//! 上千文件二次压缩很慢）。

use std::path::Path;

use crate::error::{AppError, AppResult};

use super::BuiltProject;
use super::manifest::ReleaseLock;

/// ZipWriter 绑定磁盘文件（组装整体包用）。别名避免在多个 helper 签名里重复长类型。
type ZipWriterFile = ::zip::ZipWriter<std::fs::File>;

/// 组装整体包。失败时清理半成品，避免静态下载服务返回损坏包。
pub(super) async fn assemble_workspace_package(
    ws: &Path,
    built: &[BuiltProject],
    release_lock: &ReleaseLock,
    file_name: &str,
) -> AppResult<std::path::PathBuf> {
    let out = ws.join(file_name);
    let out_for_task = out.clone();
    let ws_for_task = ws.to_path_buf();
    let built_for_task = built.to_vec();
    let lock_toml = toml::to_string_pretty(release_lock)
        .map_err(|e| AppError::system(format!("serialize release.lock.toml: {e}")))?;

    tokio::task::spawn_blocking(move || -> AppResult<()> {
        // 失败时清理半成品 zip，避免后续 GET static 服务到一个损坏包（404 优于坏包）
        let result = (|| -> AppResult<()> {
            if let Some(parent) = out_for_task.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let out_file = std::fs::File::create(&out_for_task)
                .map_err(|e| AppError::file(format!("create workspace package: {e}")))?;
            let mut zw = ZipWriterFile::new(out_file);

            // 1. 各子项目 zip：raw copy（保留原始压缩字节，不二次压缩），加 {path}/ 前缀
            for proj in &built_for_task {
                merge_artifact_with_prefix(&mut zw, &proj.artifact, &proj.path)?;
            }

            // 2. workspace 根入口文件（start.sh + scripts/）：小文件，直接写入
            add_entry_files(&mut zw, &ws_for_task)?;

            // 3. workspace.manifest.toml + 各 project.manifest.toml（app-cli 运行时读，编排子项目 + pingap）
            add_file_if_exists(&mut zw, &ws_for_task, "workspace.manifest.toml")?;
            zw.start_file("release.lock.toml", deflate_opts())
                .map_err(|e| AppError::file(format!("start_file release.lock.toml: {e}")))?;
            use std::io::Write;
            zw.write_all(lock_toml.as_bytes())?;
            for proj in &built_for_task {
                let rel = format!("{}/project.manifest.toml", proj.path);
                add_file_if_exists(&mut zw, &ws_for_task, &rel)?;
            }
            let pingap = ws_for_task.join("pingap");
            if pingap.is_dir() {
                add_dir_entries(&mut zw, &pingap, "pingap")?;
            }

            zw.finish()
                .map_err(|e| AppError::file(format!("zip finish: {e}")))?;
            Ok(())
        })();
        if result.is_err()
            && let Err(e) = std::fs::remove_file(&out_for_task)
        {
            tracing::warn!(error = %e, "cleanup failed assemble output file failed (skipping)");
        }
        result
    })
    .await
    .map_err(|e| AppError::system(format!("assemble task join: {e}")))??;

    Ok(out)
}

/// 把 `artifact` zip 的所有文件条目 raw copy 到 `zw`，条目名加 `prefix/` 前缀。
/// raw copy 直接搬运原始压缩字节 + 元数据（CRC/size/压缩方法），不二次压缩、不解压到磁盘。
fn merge_artifact_with_prefix(
    zw: &mut ZipWriterFile,
    artifact: &Path,
    prefix: &str,
) -> AppResult<()> {
    let file = std::fs::File::open(artifact)
        .map_err(|e| AppError::file(format!("open artifact {}: {e}", artifact.display())))?;
    let mut archive = ::zip::ZipArchive::new(file)
        .map_err(|e| AppError::file(format!("parse artifact {}: {e}", artifact.display())))?;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| AppError::file(format!("read entry {i}: {e}")))?;
        if entry.is_dir() {
            continue; // 目录条目跳过：文件条目带路径，解压时自动建目录
        }
        let orig_name = entry.name().to_string();
        // Zip Slip 防御：跳过含 `..` / 绝对路径 / 反斜杠 的源条目（子项目产物本应干净，保险）
        if orig_name.contains("..") || orig_name.starts_with('/') || orig_name.contains('\\') {
            tracing::warn!(entry = %orig_name, "skip unsafe zip entry in artifact");
            continue;
        }
        let new_name = format!("{prefix}/{orig_name}");
        zw.raw_copy_file_rename(entry, &new_name)
            .map_err(|e| AppError::file(format!("raw_copy {orig_name}: {e}")))?;
    }
    Ok(())
}

/// 把 workspace 根的入口文件写入 `zw`：`start.sh`（文件）+ `scripts/`（递归）。
/// 这些是小文件，直接 `start_file` 写入（Deflated 压缩）。其它（manifest、源码、logs）不进包。
fn add_entry_files(zw: &mut ZipWriterFile, ws: &Path) -> AppResult<()> {
    let start = ws.join("start.sh");
    if start.is_file() {
        zw.start_file("start.sh", deflate_opts())
            .map_err(|e| AppError::file(format!("start_file start.sh: {e}")))?;
        let mut f = std::fs::File::open(&start)?;
        std::io::copy(&mut f, zw)?;
    }
    let scripts = ws.join("scripts");
    if scripts.is_dir() {
        add_dir_entries(zw, &scripts, "scripts")?;
    }
    Ok(())
}

/// 递归把 `dir` 下普通文件作为 `{zip_prefix}/...` 条目写入 `zw`（跳过符号链接，不跟随）。
fn add_dir_entries(zw: &mut ZipWriterFile, dir: &Path, zip_prefix: &str) -> AppResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let zip_name = format!("{zip_prefix}/{name}");
        if ft.is_dir() {
            add_dir_entries(zw, &entry.path(), &zip_name)?;
        } else if ft.is_file() {
            zw.start_file(&zip_name, deflate_opts())
                .map_err(|e| AppError::file(format!("start_file {zip_name}: {e}")))?;
            let mut f = std::fs::File::open(entry.path())?;
            std::io::copy(&mut f, zw)?;
        }
    }
    Ok(())
}

/// 新建一个 Deflated 压缩的 SimpleFileOptions（每次 start_file 用一份，避免 Clone/Copy 纠结）。
fn deflate_opts() -> ::zip::write::SimpleFileOptions {
    ::zip::write::SimpleFileOptions::default()
        .compression_method(::zip::CompressionMethod::Deflated)
}

/// 如果 `ws_root/rel_path` 存在，作为 `rel_path` 条目写入 `zw`（manifest 文件打包）。
fn add_file_if_exists(zw: &mut ZipWriterFile, ws_root: &Path, rel_path: &str) -> AppResult<()> {
    let src = ws_root.join(rel_path);
    if src.is_file() {
        zw.start_file(rel_path, deflate_opts())
            .map_err(|e| AppError::file(format!("start_file {rel_path}: {e}")))?;
        let mut f = std::fs::File::open(&src)?;
        std::io::copy(&mut f, zw)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::zip; // 本地 zip::extract_to（解整体包做断言；非 test 代码用 ::zip 外部 crate raw copy）
    use shared_types::{LockedPingap, PingapMode};

    const TEST_PACKAGE: &str = "workspace-package-test-release.zip";

    fn test_lock() -> ReleaseLock {
        ReleaseLock {
            schema_version: 1,
            release_id: "test-release".into(),
            workspace_name: "test".into(),
            pingap: LockedPingap {
                mode: PingapMode::Managed,
                config: None,
                version: "test".into(),
                commit: "test".into(),
            },
            minimum_app_cli_version: "0.1.3".into(),
            runtime_image_digest: "sha256:test".into(),
            services: Vec::new(),
            bridge_service: None,
        }
    }

    /// 用 zip crate 造一个「zip 根直接是文件」的产物包（模拟子项目 build 产物）。
    fn make_flat_zip(zip_path: &Path, entries: &[(&str, &str)]) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(p) = zip_path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let file = std::fs::File::create(zip_path)?;
        let mut zw = ::zip::ZipWriter::new(file);
        let opts = ::zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            zw.start_file(name, opts)?;
            zw.write_all(content.as_bytes())?;
        }
        zw.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn assemble_merges_projects_and_entry_files_into_workspace_package() {
        // workspace 根：start.sh + scripts/lib/helpers.sh + 2 子项目产物 zip（模拟 build 产物）
        let ws = tempfile::tempdir().expect("ws tempdir");
        let ws_path = ws.path().to_path_buf();
        std::fs::write(ws_path.join("start.sh"), "#!/bin/bash\nexit 0\n").unwrap();
        std::fs::create_dir_all(ws_path.join("scripts/lib")).unwrap();
        std::fs::write(ws_path.join("scripts/lib/helpers.sh"), "log() { :; }\n").unwrap();
        // workspace.manifest.toml（app-cli 运行时读，应进包）
        std::fs::write(
            ws_path.join("workspace.manifest.toml"),
            "schema_version=1\n[workspace]\nname=\"x\"\n[pingap]\nmode=\"managed\"\n",
        )
        .unwrap();

        let fe_zip = ws_path.join("userapp-frontend/userapp-frontend.zip");
        let be_zip = ws_path.join("userapp-backend/userapp-backend.zip");
        make_flat_zip(
            &fe_zip,
            &[
                ("server.js", "FE-SERVER"),
                ("migrate.js", "FE-MIGRATE"),
                // 嵌套路径（真实 Next.js standalone 有深层 .next/static/...）—— 锁定 raw_copy + 前缀对嵌套路径正确
                (".next/static/chunk.js", "FE-CHUNK"),
            ],
        )
        .unwrap();
        make_flat_zip(&be_zip, &[("server.js", "BE-SERVER")]).unwrap();
        // project.manifest.toml（子项目目录已由 make_flat_zip 创建）
        std::fs::write(
            ws_path.join("userapp-frontend/project.manifest.toml"),
            "[project]\nname=\"frontend\"\n[build]\ncmd=\"echo\"\nartifact=\"x.zip\"\n",
        )
        .unwrap();
        std::fs::write(
            ws_path.join("userapp-backend/project.manifest.toml"),
            "[project]\nname=\"backend\"\n[build]\ncmd=\"echo\"\nartifact=\"x.zip\"\n",
        )
        .unwrap();

        let built = vec![
            BuiltProject {
                path: "userapp-frontend".into(),
                artifact: fe_zip,
            },
            BuiltProject {
                path: "userapp-backend".into(),
                artifact: be_zip,
            },
        ];

        let out = assemble_workspace_package(&ws_path, &built, &test_lock(), TEST_PACKAGE)
            .await
            .expect("assemble workspace package");
        assert_eq!(out.file_name().unwrap().to_str().unwrap(), TEST_PACKAGE);

        // 解开整体包校验预定义结构
        let extract_dst = tempfile::tempdir().expect("extract tempdir");
        zip::extract_to(out, extract_dst.path().to_path_buf())
            .await
            .expect("extract workspace package");

        let root = extract_dst.path();
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-frontend/server.js")).unwrap(),
            "FE-SERVER"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-frontend/migrate.js")).unwrap(),
            "FE-MIGRATE"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-frontend/.next/static/chunk.js")).unwrap(),
            "FE-CHUNK"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-backend/server.js")).unwrap(),
            "BE-SERVER"
        );
        assert!(root.join("start.sh").is_file());
        assert!(root.join("scripts/lib/helpers.sh").is_file());
        // manifest 进包（app-cli 运行时读）
        assert!(root.join("workspace.manifest.toml").is_file());
        assert!(root.join("release.lock.toml").is_file());
        assert!(
            root.join("userapp-frontend/project.manifest.toml")
                .is_file()
        );
        assert!(root.join("userapp-backend/project.manifest.toml").is_file());
    }

    #[tokio::test]
    async fn assemble_skips_missing_entry_files_gracefully() {
        // workspace 根无 start.sh / scripts/（极端精简场景）→ 仍能组装，只含子项目产物
        let ws = tempfile::tempdir().expect("ws tempdir");
        let ws_path = ws.path().to_path_buf();

        let fe_zip = ws_path.join("userapp-frontend/userapp-frontend.zip");
        make_flat_zip(&fe_zip, &[("server.js", "X")]).unwrap();
        let built = vec![BuiltProject {
            path: "userapp-frontend".into(),
            artifact: fe_zip,
        }];

        let out = assemble_workspace_package(&ws_path, &built, &test_lock(), TEST_PACKAGE)
            .await
            .expect("assemble without entry files");
        let extract_dst = tempfile::tempdir().expect("extract tempdir");
        zip::extract_to(out, extract_dst.path().to_path_buf())
            .await
            .expect("extract");
        assert!(
            extract_dst
                .path()
                .join("userapp-frontend/server.js")
                .is_file()
        );
        assert!(!extract_dst.path().join("start.sh").exists());
    }

    #[tokio::test]
    async fn assemble_cleans_up_partial_output_on_failure() {
        // 损坏 artifact（非合法 zip）→ merge_artifact_with_prefix 解析失败 → 整体失败
        let ws = tempfile::tempdir().expect("ws tempdir");
        let ws_path = ws.path().to_path_buf();
        let bad_zip = ws_path.join("bad/bad.zip");
        std::fs::create_dir_all(bad_zip.parent().unwrap()).unwrap();
        std::fs::write(&bad_zip, "not a zip").unwrap();
        let built = vec![BuiltProject {
            path: "bad".into(),
            artifact: bad_zip,
        }];

        let result = assemble_workspace_package(&ws_path, &built, &test_lock(), TEST_PACKAGE).await;
        assert!(result.is_err(), "assemble should fail on corrupt artifact");

        // 失败后半成品版本包必须被清理。
        let out = ws_path.join(TEST_PACKAGE);
        assert!(
            !out.exists(),
            "partial workspace package should be cleaned up on failure"
        );
    }
}
