//! 本地编译工具：`app-cli build [--dev] [--deploy-dir <DIR>] [--only <IDS>]`。
//!
//! 与平台构建链（file-server-userapp）共享 `workspace-manifest` 的三分派决策
//! （[`workspace_manifest::ProjectManifest::devbuild_argv`]，单一事实源），把
//! 「逐服务执行编译命令 → 校验 artifact →（可选）组装产物态部署布局」搬到本地：
//! 无平台环境下与 `--gen-lock` + `serve` 组成三步闭环，验证 workspace 的
//! 可构建性与可运行性（典型：模板仓库的 docker 本地验证）。
//!
//! 边界：**本地工具，不做平台专属**——不打发布 zip、不上传、无任务 SSE /
//! BuildGuard 互斥 / cancel、不做静态产物路由一致性检查（那是平台构建期的护栏，
//! 见 frontend-detector）。失败收集后汇总呈现（与 gen-lock 同款「一轮修完」体验），
//! 任一失败则退出非零且不组装部署布局。

use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use workspace_manifest::{ProjectType, discover_projects_lenient};

/// 单服务编译任务（发现阶段产出）。
struct BuildTask {
    service_id: String,
    /// manifest 发现的相对目录名（与 release.lock.services[].dir 同源）。
    dir_label: String,
    project_path: PathBuf,
    argv: Vec<String>,
    artifact: String,
    is_static: bool,
}

/// 执行本地编译。
///
/// - `dev`：三分派（配 `[devbuild]` 执行之；仅 `[devrun]` 跳过；未配 `[devrun]`
///   回落 `[build].command`）。dev 产物落**源码目录**（平台源码态语义，serve
///   直接编排源码 workspace）。
/// - `deploy_dir`：产物态部署布局组装（与 `dev` 互斥——两态产物落点不同）。
/// - `only`：逗号分隔 service_id 过滤。
pub fn run(workspace: &Path, dev: bool, deploy_dir: Option<&Path>, only: Option<&str>) -> Result<()> {
    if dev && deploy_dir.is_some() {
        bail!("--dev 与 --deploy-dir 互斥：dev 产物落源码目录（serve 直接编排），产物态才组装部署布局");
    }

    // 发现 + 校验：宽松发现，全量问题一次呈现（与 --gen-lock 同款体验）。
    let (projects, issues) = discover_projects_lenient(workspace).context("discover projects")?;
    if !issues.is_empty() {
        println!("❌ manifest 校验发现 {} 个问题:", issues.len());
        for (index, issue) in issues.iter().enumerate() {
            println!("  {}. {}", index + 1, issue);
        }
        bail!("manifest validation failed with {} issue(s)", issues.len());
    }

    let only_ids: Option<Vec<String>> = only.map(|ids| {
        ids.split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect()
    });

    println!(
        "📦 app-cli build: workspace={} mode={} 服务数={}",
        workspace.display(),
        if dev { "dev(三分派)" } else { "产物态(全量)" },
        projects.len(),
    );

    let mut tasks: Vec<BuildTask> = Vec::new();
    let mut skipped: Vec<(String, &str)> = Vec::new();
    for project in &projects {
        let service_id = project.service_id().to_string();
        if let Some(ids) = &only_ids
            && !ids.iter().any(|id| id == &service_id)
        {
            continue;
        }
        let argv = if dev {
            match project.manifest.devbuild_argv() {
                Some(argv) => argv.to_vec(),
                None => {
                    skipped.push((service_id, "仅 [devrun]，跳过编译（devrun 自足）"));
                    continue;
                }
            }
        } else {
            project.manifest.build.command.clone()
        };
        tasks.push(BuildTask {
            service_id,
            dir_label: project.dir.clone(),
            project_path: workspace.join(&project.dir),
            argv,
            artifact: project.manifest.build.artifact.clone(),
            is_static: project.manifest.project.r#type == ProjectType::Static,
        });
    }

    // 逐服务执行：失败收集（一轮修完），任一失败则退出非零。
    let mut failed = 0usize;
    for task in &tasks {
        println!(
            "\n>>> [{}] $ {}   (cwd={})",
            task.service_id,
            task.argv.join(" "),
            task.project_path.display()
        );
        let started = Instant::now();
        let status = Command::new(&task.argv[0])
            .args(&task.argv[1..])
            .current_dir(&task.project_path)
            .status()
            .with_context(|| format!("spawn build command for {}", task.service_id))?;
        let elapsed = started.elapsed().as_secs_f64();
        if !status.success() {
            println!(
                "❌ [{}] build 失败（exit={}），{:.1}s",
                task.service_id,
                status.code().unwrap_or(-1),
                elapsed
            );
            failed += 1;
            continue;
        }
        let artifact_path = task.project_path.join(&task.artifact);
        if !artifact_present(&artifact_path, task.is_static) {
            println!(
                "❌ [{}] artifact 缺失或为空: {}",
                task.service_id,
                artifact_path.display()
            );
            failed += 1;
        } else {
            println!("✅ [{}] artifact={}（{:.1}s）", task.service_id, task.artifact, elapsed);
        }
    }

    println!("\n===== 构建汇总 =====");
    for (service_id, reason) in &skipped {
        println!("  SKIP  {:<24} {}", service_id, reason);
    }
    if failed > 0 {
        bail!("{} 个服务构建失败（部署布局未组装）", failed);
    }
    println!("  全部 {} 个服务构建成功", tasks.len());

    if let Some(dir) = deploy_dir {
        assemble_deploy_dir(workspace, &tasks, dir)?;
        println!(
            "\n✅ 部署布局已组装: {}（`app-cli --workspace {} serve` 即可产物态运行）",
            dir.display(),
            dir.display()
        );
    }
    Ok(())
}

/// artifact 存在性判定：进程态 = 非空文件（zip）；static = 非空目录。
fn artifact_present(path: &Path, is_static: bool) -> bool {
    if is_static {
        path.is_dir()
            && fs::read_dir(path)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false)
    } else {
        path.is_file()
            && fs::metadata(path).map(|meta| meta.len() > 0).unwrap_or(false)
    }
}

/// 组装产物态部署布局（平台 `.run` 部署语义的本地等价）：
/// - 每服务 `<deploy>/<dir>/` = artifact 展开：zip 解压到目录根（run.command 的
///   相对路径 ./server、app.jar、server.js 在此可解析）；static 拷贝内容目录到
///   `<deploy>/<dir>/<artifact>`（serve 的 static_content_dir 语义）；
/// - workspace 根 `release.lock.toml`（serve 的编排输入）与可选 `index.html`
///   （workspace 入口页）拷入 `<deploy>/`。
///
/// 幂等：目标服务目录先清旧再放。
fn assemble_deploy_dir(workspace: &Path, tasks: &[BuildTask], deploy_dir: &Path) -> Result<()> {
    let lock_src = workspace.join("release.lock.toml");
    if !lock_src.is_file() {
        bail!(
            "缺少 {} —— 先跑 `app-cli --gen-lock {}` 生成（serve 消费 lock 编排）",
            lock_src.display(),
            workspace.display()
        );
    }
    fs::create_dir_all(deploy_dir).with_context(|| format!("create {}", deploy_dir.display()))?;

    for task in tasks {
        let dst = deploy_dir.join(&task.dir_label);
        if dst.exists() {
            fs::remove_dir_all(&dst).with_context(|| format!("clean {}", dst.display()))?;
        }
        fs::create_dir_all(&dst)?;
        let artifact_path = task.project_path.join(&task.artifact);
        if task.is_static {
            copy_dir(&artifact_path, &dst.join(&task.artifact))?;
        } else {
            extract_zip(&artifact_path, &dst).with_context(|| {
                format!("extract {} -> {}", artifact_path.display(), dst.display())
            })?;
        }
        println!("  📁 {} -> {}", task.service_id, dst.display());
    }

    fs::copy(&lock_src, deploy_dir.join("release.lock.toml")).context("copy release.lock.toml")?;
    let index_src = workspace.join("index.html");
    if index_src.is_file() {
        fs::copy(&index_src, deploy_dir.join("index.html")).context("copy index.html")?;
    }
    Ok(())
}

/// 解压 zip 到目录根。zip-slip 防护：条目路径经 `enclosed_name` 归一，逃逸即拒；
/// 保留 unix 可执行位与**符号链接条目**（standalone 类产物用 symlink 指向
/// 依赖目录——写成普通文件会破坏模块解析）。
fn extract_zip(zip_path: &Path, dst: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)
        .with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("open zip archive")?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("zip entry #{index}"))?;
        let Some(relative) = entry.enclosed_name() else {
            bail!("zip 条目路径逃逸（拒绝解压）: {}", entry.name());
        };
        let out_path = dst.join(relative);
        #[cfg(unix)]
        if entry.is_symlink() {
            // 符号链接条目：内容即目标路径（相对/绝对均按原样创建）。
            // standalone 类产物用 symlink 指向依赖目录——写成普通文件会破坏
            // 模块解析（如 next 的 .next/node_modules/pg-<hash>）。
            let mut target = String::new();
            entry
                .read_to_string(&mut target)
                .with_context(|| format!("read symlink target {}", entry.name()))?;
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            if out_path.exists() || out_path.is_symlink() {
                fs::remove_file(&out_path)
                    .with_context(|| format!("remove {}", out_path.display()))?;
            }
            std::os::unix::fs::symlink(&target, &out_path).with_context(|| {
                format!("symlink {} -> {}", out_path.display(), target)
            })?;
            continue;
        }
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out_file = fs::File::create(&out_path)?;
            io::copy(&mut entry, &mut out_file)?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&out_path, fs::Permissions::from_mode(mode))
                    .with_context(|| format!("chmod {}", out_path.display()))?;
            }
        }
    }
    Ok(())
}

/// 递归拷贝目录（静态内容目录用；不追符号链接，产物目录为普通树）。
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)
        .with_context(|| format!("read {}", src.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// fixture：zip 服务（含可执行 server + 子目录文件）+ static 服务（dist/）+
    /// 根 release.lock.toml + index.html → 组装后布局与可执行位完整。
    #[test]
    fn assemble_deploy_dir_extracts_zip_and_copies_static() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path();

        // backend（进程态）：artifact.zip = server(0o755) + conf/app.toml
        let backend = ws.join("backend");
        fs::create_dir_all(backend.join("conf")).expect("mkdir backend");
        let zip_path = backend.join("artifact.zip");
        {
            let file = fs::File::create(&zip_path).expect("create zip");
            let mut writer = zip::ZipWriter::new(file);
            let executable: zip::write::SimpleFileOptions =
                zip::write::FileOptions::default().unix_permissions(0o755);
            writer.start_file("server", executable).expect("add server");
            writer.write_all(b"#!/bin/sh\n").expect("write server");
            let plain: zip::write::SimpleFileOptions = zip::write::FileOptions::default();
            writer.start_file("conf/app.toml", plain).expect("add conf");
            writer.write_all(b"key = 1\n").expect("write conf");
            // 符号链接条目：add_symlink 是 zip 写侧正规 API，读侧用 is_symlink 识别
            let plain_options: zip::write::SimpleFileOptions = zip::write::FileOptions::default();
            writer
                .add_symlink(
                    ".next/node_modules/pg-test",
                    "../../node_modules/pg",
                    plain_options,
                )
                .expect("add symlink");
            writer.finish().expect("finish zip");
        }

        // frontend（static）：dist/index.html
        let frontend = ws.join("frontend");
        fs::create_dir_all(frontend.join("dist")).expect("mkdir dist");
        fs::write(frontend.join("dist/index.html"), "<html></html>").expect("write index");

        fs::write(ws.join("release.lock.toml"), "# lock").expect("write lock");
        fs::write(ws.join("index.html"), "<html>entry</html>").expect("write entry");

        let tasks = [
            BuildTask {
                service_id: "backend".into(),
                dir_label: "backend".into(),
                project_path: backend,
                argv: vec!["true".into()],
                artifact: "artifact.zip".into(),
                is_static: false,
            },
            BuildTask {
                service_id: "frontend".into(),
                dir_label: "frontend".into(),
                project_path: frontend,
                argv: vec!["true".into()],
                artifact: "dist".into(),
                is_static: true,
            },
        ];

        let deploy = ws.join(".deploy-check");
        assemble_deploy_dir(ws, &tasks, &deploy).expect("assemble");

        assert!(deploy.join("backend/server").is_file(), "zip 根文件未解压");
        assert!(deploy.join("backend/conf/app.toml").is_file(), "zip 子目录未解压");
        assert!(deploy.join("frontend/dist/index.html").is_file(), "static 内容目录未拷贝");
        assert_eq!(
            fs::read_to_string(deploy.join("release.lock.toml")).expect("lock"),
            "# lock"
        );
        assert!(deploy.join("index.html").is_file(), "workspace 入口页未拷贝");
        // 符号链接条目解压为 symlink（内容 = 目标路径），不是普通文件
        assert!(
            deploy.join("backend/.next/node_modules/pg-test").is_symlink(),
            "zip 符号链接条目未按 symlink 创建"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(deploy.join("backend/server"))
                .expect("stat server")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "可执行位未保留");
        }

        // 幂等：二次组装清旧重来，不残留。
        assemble_deploy_dir(ws, &tasks, &deploy).expect("assemble twice");
        assert!(deploy.join("backend/server").is_file());
    }

    /// 缺 release.lock.toml → 明确报错（提示先跑 --gen-lock）。
    #[test]
    fn assemble_deploy_dir_requires_lock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path();
        let err = assemble_deploy_dir(ws, &[], &ws.join("deploy"))
            .expect_err("missing lock must fail");
        assert!(
            err.to_string().contains("gen-lock"),
            "错误应指引先跑 --gen-lock：{err}"
        );
    }
}
