//! UserApp workspace 多项目打包：两级 manifest → 遍历子项目 build_generic → 组装整体包。
//!
//! - workspace 定位统一 [`crate::workspace::resolve_userapp_dev`]（UserApp 开发卷, 容器无关）。
//!   workspace 根下有多个子项目（前端/后端/...）。
//! - file-server 严格读取 Manifest v1，并自动发现一级子项目。
//! - 组装成版本化整体包 `workspace-package-<release_id>.zip`，内含 release lock。
//!
//! 子模块：
//! - [`manifest`]：两级 manifest 类型 + 解析
//! - [`assemble`]：整体包 zip 组装（raw copy 子产物 + 入口文件 + pingap 配置写入）
//! - [`pingap`]：pingap 反代配置（`pingap.toml`）+ `.service-ports` 生成（独立可扩展）

mod assemble;
pub mod import;
mod manifest;
pub mod tasks;

// 重导出 manifest 类型：保持 userapp 模块公开面。
pub use manifest::{
    BuildSection, ProjectManifest, ProjectMeta, ProxySection, RunSection, WorkspaceManifest,
    WorkspaceMeta,
};

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::service::build_generic::{GenericBuildRequest, build_generic};
use crate::service::build_manager::BuildManager;

use assemble::assemble_workspace_package;
use manifest::{ReleaseMetadata, build_release_lock, read_workspace_manifest};

use tasks::{BuildProgressEvent, BuildTask, BuildTaskId, BuildTaskKind, BuildTaskStore};

pub(crate) use tasks::BuildTask as UserappBuildTask;

/// 整体包产物文件名（放在 workspace 根，供 `GET /api/userapp/static` 下载）。
pub const WORKSPACE_PACKAGE_PREFIX: &str = "workspace-package-";

#[derive(Debug, Clone)]
pub struct WorkspaceBuildArtifact {
    pub release_id: String,
    pub path: PathBuf,
    pub file_name: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// 一个已构建完成的子项目（path + 产物绝对路径），供组装阶段使用。
#[derive(Clone)]
struct BuiltProject {
    path: String,
    artifact: PathBuf,
}

/// workspace 多项目打包主流程。
///
/// 1. `resolve_userapp_dev(app_id)` → workspace 根（UserApp 开发卷）
/// 2. 读 `workspace.manifest.toml` → 子项目列表
/// 3. 遍历子项目：读 `project.manifest.toml` → `build_generic(cmd, artifact, cwd={ws}/{path})`
/// 4. [`assemble::assemble_workspace_package`] 组装整体包（含 pingap 配置 + `.service-ports`）
///
/// 返回版本化整体包及其 release ID、摘要和大小。
pub async fn build_workspace_package(
    config: &crate::Config,
    build_manager: &BuildManager,
    app_id: &str,
    timeout_secs: u64,
    progress: Option<&BuildTask>,
) -> AppResult<WorkspaceBuildArtifact> {
    // 1. workspace 根（UserApp 开发卷, 容器无关）
    let ws = crate::workspace::resolve_userapp_dev(app_id, None, config)?;
    if !ws.is_dir() {
        return Err(AppError::resource(format!(
            "UserApp workspace not found: {} (app_id={app_id})",
            ws.display()
        )));
    }

    // 2. 严格解析 workspace Manifest v1。
    let manifest = read_workspace_manifest(&ws).await?;

    // 3. 自动发现子项目（扫描含 project.manifest.toml 的一级子目录）
    let discovered = manifest::discover_projects(&ws)
        .map_err(|e| AppError::system(format!("discover projects in {}: {e}", ws.display())))?;
    if discovered.is_empty() {
        return Err(AppError::business(format!(
            "no sub-projects found (no project.manifest.toml in any subdirectory of workspace=\"{}\")",
            manifest.workspace.name
        )));
    }

    // 4. 各子项目 build（log_dir = workspace/logs/<dir>；分项目日志方便排查哪个构建失败）
    let enabled: Vec<_> = discovered
        .iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();

    // 整个 workspace 构建周期持有 app_id 的 BuildGuard(项目互斥 + 1 个全局 permit):
    // 避免子项目构建间隙释放锁导致同 app_id 构建穿插、首个构建中途 409 失败(#13)。
    // guard 以引用传给每个子项目 build_generic,跨整个 for 循环不释放。
    let _ws_guard = build_manager.try_start(app_id)?;

    let mut built: Vec<BuiltProject> = Vec::with_capacity(enabled.len());
    for proj in enabled {
        // 软取消：服务间检查（硬 cancel 靠外部 kill 进程组，见 cancel handler）。
        // 不在此 emit 终态（Cancelled 由 cancel handler / 顶层 task 统一 emit）。
        if let Some(p) = progress {
            if p.is_cancelled() {
                return Err(AppError::business("build cancelled by user"));
            }
            p.emit(BuildProgressEvent::Building {
                service: proj.name().to_string(),
            })
            .await;
        }
        let log_dir = ws.join("logs").join(&proj.dir);
        // path 安全校验 + 拼接（防 `../` 穿越 workspace）
        let proj_dir = crate::path_safety::ensure_within(&ws, &proj.dir).map_err(|_| {
            AppError::validation(format!(
                "project path escapes workspace: {} (=\"{}\")",
                proj.dir,
                proj.name()
            ))
        })?;
        if !proj_dir.is_dir() {
            return Err(AppError::resource(format!(
                "project dir not found: {} (path={})",
                proj.name(),
                proj.dir
            )));
        }
        // on_pid 回调: spawn build 子进程后回写 pid 到 task, 供 cancel kill 进程组。
        let pid_cb = progress.map(|p| move |pid: u32| p.set_pid(pid));
        let pid_ref: Option<&(dyn Fn(u32) + Send + Sync)> =
            pid_cb.as_ref().map(|c| c as &(dyn Fn(u32) + Send + Sync));
        let build_result = build_generic(
            &GenericBuildRequest {
                argv: &proj.manifest.build.command,
                cwd: &proj_dir,
                artifact_rel: &proj.manifest.build.artifact,
                log_dir: &log_dir,
                timeout_secs,
                on_pid: pid_ref,
            },
            &_ws_guard,
        )
        .await;
        // 子进程已退出(或超时被 kill),pid 即将失效,清零缩短 stale-pid 窗口(#2)。
        if let Some(p) = progress {
            p.clear_pid();
        }
        let artifact = match build_result {
            Ok(a) => a,
            Err(e) => {
                // cancel(kill 进程组)导致的失败不 emit（终态 Cancelled 由 cancel handler 置）；
                // 否则 emit 服务级 BuildFail（任务级 Failed 由顶层 start_*_task 统一 emit）。
                if let Some(p) = progress
                    && !p.is_cancelled()
                {
                    p.emit(BuildProgressEvent::BuildFail {
                        service: proj.name().to_string(),
                        error: e.to_string(),
                    })
                    .await;
                }
                return Err(e);
            }
        };
        if let Some(p) = progress {
            p.emit(BuildProgressEvent::BuildOk {
                service: proj.name().to_string(),
            })
            .await;
        }
        built.push(BuiltProject {
            path: proj.dir.clone(),
            artifact,
        });
    }

    let release_id = uuid::Uuid::now_v7().simple().to_string();
    let pingap_version = required_release_metadata("RCODER_PINGAP_VERSION")?;
    let pingap_commit = required_release_metadata("RCODER_PINGAP_COMMIT")?;
    let runtime_image_digest = required_release_metadata("RCODER_RUNTIME_IMAGE_DIGEST")?;
    let lock = build_release_lock(
        &manifest,
        &discovered,
        ReleaseMetadata {
            release_id: &release_id,
            pingap_version: &pingap_version,
            pingap_commit: &pingap_commit,
            minimum_app_cli_version: env!("CARGO_PKG_VERSION"),
            runtime_image_digest: &runtime_image_digest,
        },
    )
    .map_err(|e| AppError::business(e.to_string()))?;
    let file_name = format!("{WORKSPACE_PACKAGE_PREFIX}{release_id}.zip");
    let path = assemble_workspace_package(&ws, &built, &lock, &file_name).await?;
    let (sha256, size_bytes) = hash_file(&path).await?;
    Ok(WorkspaceBuildArtifact {
        release_id,
        path,
        file_name,
        sha256,
        size_bytes,
    })
}

/// 异步发起 build 任务（不阻塞，立即返 taskId）。进度事件经 task 流出（SSE/轮询）。
///
/// 同 app_id 互斥由 `build_workspace_package` 最外层 `try_start(app_id)` 持有的
/// `BuildGuard` 保证(覆盖整个构建周期,跨所有子项目)。重复构建立即返回 409 fail-fast
/// (非排队);该 guard 同时占用 1 个全局并发 permit,以引用传给每个子项目 build_generic。
/// 非循环路径的 Err（如 release lock env 缺失）由这里兜底 emit Failed。
pub async fn start_build_task(
    store: &BuildTaskStore,
    config: &Arc<crate::Config>,
    build_manager: Arc<BuildManager>,
    app_id: String,
    timeout_secs: u64,
) -> Result<BuildTaskId, AppError> {
    // 容量耗尽(全活跃任务达上限)→ 立即拒绝,不再越过上限插入(#12)。
    let task = store
        .create(app_id.clone(), BuildTaskKind::Build)
        .await
        .map_err(|e| AppError::business(e.to_string()))?;
    // 预 resolve workspace 根并存入 task,供 logs/SSE handler 解析日志目录
    // ({workspace}/logs/{service}/)。resolve 失败则 emit Failed 终态,不 spawn。
    match crate::workspace::resolve_userapp_dev(&app_id, None, config) {
        Ok(ws) => task.set_workspace_root(ws).await,
        Err(e) => {
            task.emit(BuildProgressEvent::Failed {
                error: format!("resolve workspace: {e}"),
            })
            .await;
            return Ok(task.id.clone());
        }
    }
    let task_spawn = task.clone();
    let config = Arc::clone(config);
    tokio::spawn(async move {
        let result = build_workspace_package(
            &config,
            build_manager.as_ref(),
            &app_id,
            timeout_secs,
            Some(&task_spawn),
        )
        .await;
        // 终态统一由此 emit：build_workspace_package 只发非终态进度（Building/BuildOk/BuildFail）。
        // Ok → Completed；Err 且非 cancel → Failed（cancel 的 Cancelled 已由 cancel handler emit）。
        match result {
            Ok(artifact) => {
                task_spawn
                    .emit(BuildProgressEvent::Completed {
                        release_id: artifact.release_id.clone(),
                        sha256: artifact.sha256.clone(),
                        size_bytes: artifact.size_bytes,
                        file_name: artifact.file_name.clone(),
                    })
                    .await;
            }
            Err(e) => {
                if !task_spawn.is_cancelled() && !task_spawn.is_terminal().await {
                    task_spawn
                        .emit(BuildProgressEvent::Failed {
                            error: e.to_string(),
                        })
                        .await;
                }
            }
        }
    });
    Ok(task.id.clone())
}

fn required_release_metadata(name: &str) -> AppResult<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AppError::system(format!(
                "required release metadata environment variable is missing: {name}"
            ))
        })
}

async fn hash_file(path: &std::path::Path) -> AppResult<(String, u64)> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;

    // 流式读取：整个 workspace 发布包（多项目 Next.js standalone，可达数百 MB~GB）
    // 不能一次性 read 进内存算 hash（高并发发布会 OOM）。用固定 64KB buffer 循环 update。
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| AppError::file(format!("open built workspace package: {e}")))?;
    let size = file
        .metadata()
        .await
        .map_err(|e| AppError::file(format!("stat built workspace package: {e}")))?
        .len();

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| AppError::file(format!("read built workspace package: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}
