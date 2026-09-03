//! 版本化整体包 `workspace-package-<release_id>.zip` 组装。
//!
//! 各子项目 zip **直接 raw copy**（加 `{path}/` 前缀，不二次压缩）+ workspace 根入口文件
//! （`start.sh`/`scripts/`，小文件正常压缩）+ pingap 配置 + `.service-ports`。
//!
//! 性能：`ZipWriter::raw_copy_file_rename` 搬运子项目 zip 条目的原始压缩字节 + 元数据
//! （CRC/size/压缩方法），避免「解压到磁盘 → 再压缩」（Next.js standalone 含 node_modules，
//! 上千文件二次压缩很慢）。

use std::path::Path;

use file_server::error::{AppError, AppResult};

use super::BuiltProject;
use super::manifest::ReleaseLock;

/// ZipWriter 绑定磁盘文件（组装整体包用）。别名避免在多个 helper 签名里重复长类型。
type ZipWriterFile = ::zip::ZipWriter<std::fs::File>;

/// 组装整体包。写入经 `.part` 临时文件 + rename 原子落盘——最终名只承载完整包
/// （GET static 按"最新文件名"选包，永不会命中写一半的半成品；中断残留的 `.part`
/// 不带 `.zip` 后缀，不会被选中，下次构建同 release_id 时截断重写）。
pub(super) async fn assemble_workspace_package(
    ws: &Path,
    built: &[BuiltProject],
    release_lock: &ReleaseLock,
    file_name: &str,
) -> AppResult<std::path::PathBuf> {
    let out = ws.join(file_name);
    let part = out.with_extension("zip.part");
    let out_for_task = out.clone();
    let part_for_task = part.clone();
    let ws_for_task = ws.to_path_buf();
    let built_for_task = built.to_vec();
    let lock_toml = toml::to_string_pretty(release_lock)
        .map_err(|e| AppError::system(format!("serialize release.lock.toml: {e}")))?;

    tokio::task::spawn_blocking(move || -> AppResult<()> {
        // 失败时清理半成品 .part（最终名只经 rename 出现，天然无半截包）
        let result = (|| -> AppResult<()> {
            if let Some(parent) = part_for_task.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let out_file = std::fs::File::create(&part_for_task)
                .map_err(|e| AppError::file(format!("create workspace package: {e}")))?;
            let mut zw = ZipWriterFile::new(out_file);

            // 1. 各子项目产物：zip（raw copy 保留原始压缩字节）或 static 目录
            //（type=static 的 artifact 为静态内容目录——递归打入 {path}/ 前缀）
            for proj in &built_for_task {
                if proj.artifact.is_dir() {
                    add_dir_entries(&mut zw, &proj.artifact, &proj.path)?;
                } else {
                    merge_artifact_with_prefix(&mut zw, &proj.artifact, &proj.path)?;
                }
            }

            // 2. workspace 根入口文件（start.sh + scripts/）：小文件，直接写入
            add_entry_files(&mut zw, &ws_for_task)?;

            // 3. workspace.manifest.toml + 各 project.manifest.toml（app-cli 运行时读，编排子项目 + pingap）
            add_file_if_exists(&mut zw, &ws_for_task, "workspace.manifest.toml")?;
            // workspace 首页（可选）：.run 解压后由 app-cli 内置静态服务 serve +
            // pingap 兜底路由展示
            add_file_if_exists(&mut zw, &ws_for_task, "index.html")?;
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

            // 4. database/ 目录（发布后平台自动按序执行其中的 .sql——见 rcoder 发布编排
            //    的 database_sql 阶段；失败仅日志不阻断）。workspace 根先（建库/扩展类），
            //    各子项目源目录补收（build 产物 zip 不含 database/）。
            let database = ws_for_task.join("database");
            if database.is_dir() {
                add_dir_entries(&mut zw, &database, "database")?;
            }
            for proj in &built_for_task {
                let proj_db = ws_for_task.join(&proj.path).join("database");
                if proj_db.is_dir() {
                    add_dir_entries(&mut zw, &proj_db, &format!("{}/database", proj.path))?;
                }
            }

            zw.finish()
                .map_err(|e| AppError::file(format!("zip finish: {e}")))?;
            // 原子落盘：rename 同目录内生效，最终名瞬间从无到有完整包
            std::fs::rename(&part_for_task, &out_for_task)
                .map_err(|e| AppError::file(format!("finalize workspace package: {e}")))?;
            Ok(())
        })();
        if result.is_err()
            && let Err(e) = std::fs::remove_file(&part_for_task)
        {
            tracing::warn!(error = %e, "cleanup partial artifact failed (skipping)");
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
    use file_server::service::zip; // 本地 zip::extract_to（解整体包做断言；非 test 代码用 ::zip 外部 crate raw copy）
    use shared_types::{LockedPingap, PingapMode};

    /// 产物相对路径（含 builds/ 子目录前缀——生产调用方传 rel_path 形态）。
    const TEST_PACKAGE_REL: &str = "builds/workspace-package-test-release.zip";
    /// 纯文件名段。
    const TEST_PACKAGE_FILE: &str = "workspace-package-test-release.zip";

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
        // workspace 首页（app-cli 静态服务 serve；存在才进包）
        std::fs::write(ws_path.join("index.html"), "<html>home</html>\n").unwrap();
        // database 目录（workspace 根 + 子项目；发布后平台自动执行其中的 .sql）
        std::fs::create_dir_all(ws_path.join("database")).unwrap();
        std::fs::write(
            ws_path.join("database/001_init.sql"),
            "CREATE TABLE IF NOT EXISTS t(id int);\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws_path.join("userapp-backend/database")).unwrap();
        std::fs::write(
            ws_path.join("userapp-backend/database/001_orders.sql"),
            "SELECT 1;\n",
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

        let out = assemble_workspace_package(&ws_path, &built, &test_lock(), TEST_PACKAGE_REL)
            .await
            .expect("assemble workspace package");
        assert_eq!(
            out.file_name().unwrap().to_str().unwrap(),
            TEST_PACKAGE_FILE
        );
        // 产物落 {ws}/builds/ 子目录（父目录自动创建）
        assert_eq!(
            out.parent().unwrap().file_name().unwrap().to_str().unwrap(),
            "builds"
        );

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
        // workspace 首页进包（.run 解压后 app-cli 静态服务 serve）
        assert_eq!(
            std::fs::read_to_string(root.join("index.html")).unwrap(),
            "<html>home</html>\n"
        );
        assert!(root.join("release.lock.toml").is_file());
        assert!(
            root.join("userapp-frontend/project.manifest.toml")
                .is_file()
        );
        assert!(root.join("userapp-backend/project.manifest.toml").is_file());
        // database 目录进包（根 + 子项目源目录补收——artifact.zip 不含 database/）
        assert_eq!(
            std::fs::read_to_string(root.join("database/001_init.sql")).unwrap(),
            "CREATE TABLE IF NOT EXISTS t(id int);\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-backend/database/001_orders.sql")).unwrap(),
            "SELECT 1;\n"
        );
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

        let out = assemble_workspace_package(&ws_path, &built, &test_lock(), TEST_PACKAGE_REL)
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

        let result =
            assemble_workspace_package(&ws_path, &built, &test_lock(), TEST_PACKAGE_REL).await;
        assert!(result.is_err(), "assemble should fail on corrupt artifact");

        // 失败后半成品版本包必须被清理。
        let out = ws_path.join(TEST_PACKAGE_REL);
        assert!(
            !out.exists(),
            "partial workspace package should be cleaned up on failure"
        );
    }
    /// static 目录产物：artifact 为目录时递归打入 {path}/ 前缀（zip 产物走
    /// raw copy 的既有分支不变）。
    #[tokio::test]
    async fn assemble_packs_static_directory_artifacts() {
        let ws = tempfile::tempdir().expect("ws tempdir");
        let ws_path = ws.path().to_path_buf();
        std::fs::write(
            ws_path.join("workspace.manifest.toml"),
            "schema_version=1\n[workspace]\nname=\"x\"\n[pingap]\nmode=\"managed\"\n",
        )
        .unwrap();
        // static 服务产物：目录（dist 含嵌套 assets）
        let dist = ws_path.join("frontend/dist");
        std::fs::create_dir_all(dist.join("assets")).expect("dist");
        std::fs::write(dist.join("index.html"), "<html>static</html>").expect("index");
        std::fs::write(dist.join("assets/app.js"), "js").expect("asset");

        let built = vec![BuiltProject {
            path: "frontend".into(),
            artifact: dist,
        }];
        let out = assemble_workspace_package(&ws_path, &built, &test_lock(), TEST_PACKAGE_REL)
            .await
            .expect("assemble");
        let extract_dst = tempfile::tempdir().expect("extract");
        zip::extract_to(out, extract_dst.path().to_path_buf())
            .await
            .expect("extract");
        let root = extract_dst.path();
        assert_eq!(
            std::fs::read_to_string(root.join("frontend/index.html")).unwrap(),
            "<html>static</html>"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("frontend/assets/app.js")).unwrap(),
            "js"
        );
    }
}
