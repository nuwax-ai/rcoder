//! UserApp workspace 多项目打包：两级 manifest → 遍历子项目 build_generic → 组装整体包。
//!
//! - `app_id` = file-server `project_id`（复用 [`WorkspaceResolver::resolve_project`]，
//!   不新建 resolve_userapp）。workspace 根下有多个子项目（前端/后端/...）。
//! - file-server **只读** manifest 的 `[workspace]`+`[[projects]]`（驱动 build）和子项目的
//!   `[project]`+`[build]`；`[deploy]`（部署配置）由 Java `create_app` 读，file-server 不解析，
//!   保持轻量 + 与部署解耦。
//! - 组装成一个整体包 `workspace-package.zip`（各子项目产物解压到 `{path}/` + workspace 根的
//!   `start.sh` + `scripts/`），Java 一次取/upload/部署。
//!
//! 详见 `docs/userapp-development-design.md` §5。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::service::build_generic::build_generic;
use crate::service::build_manager::BuildManager;
use crate::workspace::{ProjectContext, WorkspaceResolver};

/// ZipWriter 绑定磁盘文件（组装整体包用）。别名避免在多个 helper 签名里重复长类型。
type ZipWriterFile = ::zip::ZipWriter<std::fs::File>;

/// 整体包产物文件名（放在 workspace 根，供 `GET /api/userapp/static` 下载）。
pub const WORKSPACE_PACKAGE_ZIP: &str = "workspace-package.zip";

/// `workspace.manifest.toml`（workspace 根）—— file-server 只读 `[workspace]` + `[[projects]]`。
///
/// `[deploy]` 不在此结构（serde 默认忽略未知字段），由 Java 读。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceManifest {
    pub workspace: WorkspaceMeta,
    #[serde(default)]
    pub projects: Vec<ProjectRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceMeta {
    pub name: String,
}

/// workspace.manifest.toml 的 `[[projects]]` 条目（子项目引用）。
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectRef {
    pub name: String,
    /// workspace 相对路径（如 `userapp-frontend`）。打包时 join 到 workspace 根。
    pub path: String,
}

/// `project.manifest.toml`（各子项目目录）—— file-server 只读 `[project]` + `[build]`。
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectManifest {
    pub project: ProjectMeta,
    pub build: BuildSection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectMeta {
    pub name: String,
    /// 项目类型（`node`/`java`/`python`/`rust`）。file-server 仅记录，不据此分支（cmd 透明执行）。
    #[serde(default)]
    pub r#type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildSection {
    /// native build 命令（容器内跑，如 `npm run build:standalone`），经 `sh -c` 执行。
    pub cmd: String,
    /// 产物相对路径（cwd = 子项目目录，如 `userapp-frontend.zip`）。
    pub artifact: String,
}

/// 一个已构建完成的子项目（path + 产物绝对路径），供组装阶段使用。
#[derive(Clone)]
struct BuiltProject {
    path: String,
    artifact: PathBuf,
}

/// workspace 多项目打包主流程。
///
/// 1. `resolve_project(app_id)` → workspace 根（app_id = project_id）
/// 2. 读 `workspace.manifest.toml` → 子项目列表
/// 3. 遍历子项目：读 `project.manifest.toml` → `build_generic(cmd, artifact, cwd={ws}/{path})`
/// 4. 组装整体包 `workspace-package.zip`（各子产物解压到 `{path}/` + workspace 根 start.sh/scripts）
///
/// 返回整体包绝对路径（`{workspace}/workspace-package.zip`）。`[deploy]` 不解析（Java 负责）。
pub async fn build_workspace_package(
    resolver: &dyn WorkspaceResolver,
    build_manager: &BuildManager,
    app_id: &str,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    timeout_secs: u64,
) -> AppResult<PathBuf> {
    // 1. workspace 根（app_id 复用 project_id）
    let ws = resolver
        .resolve_project(&ProjectContext {
            project_id: app_id.to_string(),
            tenant_id: tenant_id.map(str::to_string),
            space_id: space_id.map(str::to_string),
            isolation_type: None,
        })
        .await?;
    if !ws.is_dir() {
        return Err(AppError::resource(format!(
            "UserApp workspace not found: {} (app_id={app_id})",
            ws.display()
        )));
    }

    // 2. workspace manifest
    let manifest = read_workspace_manifest(&ws).await?;
    if manifest.projects.is_empty() {
        return Err(AppError::business(format!(
            "workspace.manifest.toml declares no [[projects]] (workspace=\"{}\")",
            manifest.workspace.name
        )));
    }

    // 3. 各子项目 build（log_dir = workspace/logs；build_generic 内建 create_dir_all）
    let log_dir = ws.join("logs");
    let mut built: Vec<BuiltProject> = Vec::with_capacity(manifest.projects.len());
    for proj in &manifest.projects {
        // path 安全校验 + 拼接（防 `../` 穿越 workspace）
        let proj_dir = crate::path_safety::ensure_within(&ws, &proj.path).map_err(|_| {
            AppError::validation(format!(
                "project path escapes workspace: {} (=\"{}\")",
                proj.path, proj.name
            ))
        })?;
        if !proj_dir.is_dir() {
            return Err(AppError::resource(format!(
                "project dir not found: {} (path={})",
                proj.name, proj.path
            )));
        }
        let proj_manifest = read_project_manifest(&proj_dir).await?;
        let artifact = build_generic(
            build_manager,
            app_id,
            &proj_manifest.build.cmd,
            &proj_dir,
            &proj_manifest.build.artifact,
            &log_dir,
            timeout_secs,
        )
        .await?;
        built.push(BuiltProject {
            path: proj.path.clone(),
            artifact,
        });
    }

    // 4. 组装整体包
    assemble_workspace_package(&ws, &built).await
}

async fn read_workspace_manifest(ws: &Path) -> AppResult<WorkspaceManifest> {
    let path = ws.join("workspace.manifest.toml");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AppError::resource(format!("read workspace.manifest.toml: {e}")))?;
    toml::from_str(&content)
        .map_err(|e| AppError::business(format!("parse workspace.manifest.toml: {e}")))
}

async fn read_project_manifest(proj_dir: &Path) -> AppResult<ProjectManifest> {
    let path = proj_dir.join("project.manifest.toml");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AppError::resource(format!("read project.manifest.toml: {e}")))?;
    toml::from_str(&content)
        .map_err(|e| AppError::business(format!("parse project.manifest.toml: {e}")))
}

/// 组装整体包：各子项目 zip **直接 raw copy**（加 `{path}/` 前缀，不二次压缩）+
/// workspace 根入口文件（start.sh + scripts/，小文件正常压缩）→ 写 `{workspace}/workspace-package.zip`。
///
/// 性能：用 `ZipWriter::raw_copy_file_rename` 搬运子项目 zip 条目的**原始压缩字节 + 元数据**
/// （CRC/size/压缩方法），避免「解压到磁盘 → 再压缩」（Next.js standalone 含 node_modules，
/// 上千文件二次压缩很慢）。入口文件（start.sh + scripts/）是小文件，直接 `start_file` 写入。
async fn assemble_workspace_package(
    ws: &Path,
    built: &[BuiltProject],
) -> AppResult<PathBuf> {
    let out = ws.join(WORKSPACE_PACKAGE_ZIP);
    let out_for_task = out.clone();
    let ws_for_task = ws.to_path_buf();
    let built_for_task = built.to_vec();

    tokio::task::spawn_blocking(move || -> AppResult<()> {
        // 失败时清理半成品 zip，避免后续 GET static 服务到一个损坏包（404 优于坏包）
        let result = (|| -> AppResult<()> {
            if let Some(parent) = out_for_task.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let out_file = std::fs::File::create(&out_for_task)
                .map_err(|e| AppError::file(format!("create workspace-package.zip: {e}")))?;
            let mut zw = ZipWriterFile::new(out_file);

            // 1. 各子项目 zip：raw copy（保留原始压缩字节，不二次压缩），加 {path}/ 前缀
            for proj in &built_for_task {
                merge_artifact_with_prefix(&mut zw, &proj.artifact, &proj.path)?;
            }

            // 2. workspace 根入口文件（start.sh + scripts/）：小文件，直接写入
            add_entry_files(&mut zw, &ws_for_task)?;

            zw.finish()
                .map_err(|e| AppError::file(format!("zip finish: {e}")))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&out_for_task);
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
fn add_entry_files(zw: &mut ::zip::ZipWriter<std::fs::File>, ws: &Path) -> AppResult<()> {
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
fn add_dir_entries(
    zw: &mut ZipWriterFile,
    dir: &Path,
    zip_prefix: &str,
) -> AppResult<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::zip; // 本地 zip::extract_to（解整体包做断言；非 test 代码用 ::zip 外部 crate raw copy）

    #[test]
    fn parse_workspace_manifest_ignores_deploy_section() {
        // [deploy] 不在 WorkspaceManifest 结构内 → serde 默认忽略（file-server 不解析部署配置）
        let toml_text = r#"
[workspace]
name = "my-userapp"

[[projects]]
name = "frontend"
path = "userapp-frontend"

[[projects]]
name = "backend"
path = "userapp-backend"

[deploy]
image = "some-image"
command = ["sh", "/app/code/start.sh"]
"#;
        let m: WorkspaceManifest = toml::from_str(toml_text).expect("parse workspace manifest");
        assert_eq!(m.workspace.name, "my-userapp");
        assert_eq!(m.projects.len(), 2);
        assert_eq!(m.projects[0].name, "frontend");
        assert_eq!(m.projects[0].path, "userapp-frontend");
        assert_eq!(m.projects[1].path, "userapp-backend");
    }

    #[test]
    fn parse_workspace_manifest_empty_projects_default() {
        let toml_text = r#"
[workspace]
name = "empty"
"#;
        let m: WorkspaceManifest = toml::from_str(toml_text).expect("parse empty workspace");
        assert!(m.projects.is_empty());
    }

    #[test]
    fn parse_project_manifest_with_type() {
        let toml_text = r#"
[project]
name = "frontend"
type = "node"

[build]
cmd = "npm run build:standalone"
artifact = "userapp-frontend.zip"
"#;
        let m: ProjectManifest = toml::from_str(toml_text).expect("parse project manifest");
        assert_eq!(m.project.name, "frontend");
        assert_eq!(m.project.r#type, "node");
        assert_eq!(m.build.cmd, "npm run build:standalone");
        assert_eq!(m.build.artifact, "userapp-frontend.zip");
    }

    #[test]
    fn parse_project_manifest_type_optional() {
        // type 缺省 → 空串（#[serde(default)]）
        let toml_text = r#"
[project]
name = "legacy"

[build]
cmd = "make"
artifact = "out.zip"
"#;
        let m: ProjectManifest = toml::from_str(toml_text).expect("parse project without type");
        assert_eq!(m.project.r#type, "");
    }

    // ── 整体包组装集成测试（不依赖部署，锁定 extract 子产物 + 拷入口文件 + pack 逻辑）──

    /// 用 zip crate 造一个「zip 根直接是文件」的产物包（模拟子项目 build 产物）。
    /// 注：本模块顶部 `use crate::service::zip;` 遮蔽了外部 zip crate 的名字 `zip`，
    /// 故此处用绝对路径 `::zip::` 引用外部 crate（extract_to 用本地 service::zip 才对）。
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
        // 干扰项：workspace.manifest.toml + logs/ 不应进整体包
        std::fs::write(ws_path.join("workspace.manifest.toml"), "[workspace]\nname=\"x\"\n").unwrap();

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

        let out = assemble_workspace_package(&ws_path, &built)
            .await
            .expect("assemble workspace package");
        assert_eq!(out.file_name().unwrap().to_str().unwrap(), WORKSPACE_PACKAGE_ZIP);

        // 解开整体包校验预定义结构
        let extract_dst = tempfile::tempdir().expect("extract tempdir");
        zip::extract_to(out, extract_dst.path().to_path_buf())
            .await
            .expect("extract workspace package");

        let root = extract_dst.path();
        // 各子项目产物在 {path}/ 前缀下（zip 根 → 子项目目录）
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-frontend/server.js")).unwrap(),
            "FE-SERVER"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-frontend/migrate.js")).unwrap(),
            "FE-MIGRATE"
        );
        // 嵌套路径经 raw_copy + 前缀后仍正确（.next/static/chunk.js → userapp-frontend/.next/static/chunk.js）
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-frontend/.next/static/chunk.js")).unwrap(),
            "FE-CHUNK"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("userapp-backend/server.js")).unwrap(),
            "BE-SERVER"
        );
        // workspace 根入口文件
        assert!(root.join("start.sh").is_file());
        assert!(root.join("scripts/lib/helpers.sh").is_file());
        // 干扰项不进整体包
        assert!(!root.join("workspace.manifest.toml").exists());
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

        let out = assemble_workspace_package(&ws_path, &built)
            .await
            .expect("assemble without entry files");
        let extract_dst = tempfile::tempdir().expect("extract tempdir");
        zip::extract_to(out, extract_dst.path().to_path_buf())
            .await
            .expect("extract");
        assert!(extract_dst
            .path()
            .join("userapp-frontend/server.js")
            .is_file());
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

        let result = assemble_workspace_package(&ws_path, &built).await;
        assert!(result.is_err(), "assemble should fail on corrupt artifact");

        // 失败后半成品 workspace-package.zip 必须被清理（否则后续 GET static 会服务到损坏包）
        let out = ws_path.join(WORKSPACE_PACKAGE_ZIP);
        assert!(
            !out.exists(),
            "partial workspace-package.zip should be cleaned up on failure"
        );
    }
}
