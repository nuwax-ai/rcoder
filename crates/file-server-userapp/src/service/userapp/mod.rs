//! UserApp workspace 多项目打包：两级 manifest → 遍历子项目 build_generic → 组装整体包。
//!
//! - workspace 定位统一 [`file_server::workspace::resolve_userapp_dev`]（UserApp 开发卷, 容器无关）。
//!   workspace 根下有多个子项目（前端/后端/...）。
//! - file-server 严格读取 Manifest v1，并自动发现一级子项目。
//! - 组装成版本化整体包 `builds/workspace-package-<release_id>.zip`，内含 release lock。
//!
//! 子模块：
//! - `manifest`：两级 manifest 类型 + 解析
//! - `assemble`：整体包 zip 组装（raw copy 子产物 + 入口文件 + pingap 配置写入）
//! - `pingap`：pingap 反代配置（`pingap.toml`）+ `.service-ports` 生成（独立可扩展）

mod assemble;
pub mod hygiene;
pub mod import;
mod manifest;
pub mod run_dir;
pub mod tasks;

// 重导出 manifest 类型：保持 userapp 模块公开面。
pub use manifest::{
    BuildSection, ProjectManifest, ProjectMeta, ProxySection, RunSection, WorkspaceManifest,
    WorkspaceMeta,
};

use std::path::PathBuf;
use std::sync::Arc;

use file_server::error::{AppError, AppResult};
use file_server::service::build_generic::{GenericBuildRequest, build_generic};
use file_server::service::build_manager::BuildManager;

use assemble::assemble_workspace_package;
use manifest::{ReleaseMetadata, build_release_lock, read_workspace_manifest};

use crate::models::{BuildTaskId, BuildTaskKind};
use tasks::{BuildProgressEvent, BuildTask, BuildTaskStore};

pub(crate) use tasks::BuildTask as UserappBuildTask;

/// 整体包产物文件名前缀（产物落 `{ws}/builds/` 子目录，见 [`WORKSPACE_BUILDS_DIR`]；
/// `GET /api/v1/userapp/static/{app_id}` 按 app 直下取包——缺省最新，`?release_id=` 指定版本）。
pub const WORKSPACE_PACKAGE_PREFIX: &str = "workspace-package-";

/// workspace 内的构建产物目录（整体包落 `{ws}/builds/`；模板 .gitignore 忽略）。
pub const WORKSPACE_BUILDS_DIR: &str = "builds";

/// SSE `log` 事件单行字节上限（事件流展示副本的截断线，文件落盘不受影响）。
const MAX_LOG_EVENT_LINE_BYTES: usize = 16 * 1024;

/// 按 UTF-8 字符边界截断（超限截到 `max` 内最大合法前缀，带省略号标记）。
fn truncate_at_char(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push('…');
    out
}

/// 拼 workspace 相对产物路径（`builds/workspace-package-{release_id}.zip`）——
/// build 创建响应/任务快照/Completed 事件的 `artifact_path` 同源。
pub fn workspace_artifact_rel_path(release_id: &str) -> String {
    format!("{WORKSPACE_BUILDS_DIR}/{WORKSPACE_PACKAGE_PREFIX}{release_id}.zip")
}

#[derive(Debug, Clone)]
pub struct WorkspaceBuildArtifact {
    pub release_id: String,
    /// 产物绝对路径（`{ws}/builds/{file_name}`）。
    pub path: PathBuf,
    /// 相对 workspace 根的产物路径（`builds/{file_name}`）——快照/事件的
    /// `artifact_path` 信息字段（取包走 `/api/v1/userapp/static/{app_id}`，带
    /// `?release_id={release_id}` 精确取本版本）。
    pub rel_path: String,
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
/// 4. `assemble::assemble_workspace_package` 组装整体包（含 pingap 配置 + `.service-ports`）
///
/// `release_id` 由调用方预生成（start_build_task 在任务创建时生成并预置进 task
/// 快照——创建响应即可返回确定性产物路径），本函数不再内部生成。
/// 返回版本化整体包及其 release ID、摘要和大小。
pub async fn build_workspace_package(
    config: &file_server::Config,
    build_manager: &BuildManager,
    app_id: &str,
    release_id: &str,
    timeout_secs: u64,
    progress: Option<Arc<BuildTask>>,
) -> AppResult<WorkspaceBuildArtifact> {
    // 1. workspace 根（UserApp 开发卷, 容器无关）
    let ws = file_server::workspace::resolve_userapp_dev(app_id, None, config)?;
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
        if let Some(p) = &progress {
            if p.is_cancelled() {
                return Err(AppError::business("build cancelled by user"));
            }
            p.emit(BuildProgressEvent::Building {
                service: proj.service_id().to_string(),
            })
            .await;
        }
        // 构建输出逐行转 Log 事件：unbounded 通道 + 独立消费 task（emit 为 async，
        // 管道回调同步 send 行）。run_command_to_log 返回前已 drain 管道——返回后
        // drop 闭包关通道 → 消费 task 排空 join → 才 emit BuildOk/BuildFail，
        // 顺序严格为 building → log* → build_ok/build_fail。
        let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let line_task = {
            let task = progress.clone();
            let service = proj.service_id().to_string();
            tokio::spawn(async move {
                let mut rx = line_rx;
                while let Some(line) = rx.recv().await {
                    if let Some(task) = &task {
                        task.emit(BuildProgressEvent::Log {
                            service: service.clone(),
                            line,
                        })
                        .await;
                    }
                }
            })
        };
        let line_cb: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |line: &str| {
            // 超长行截断（UTF-8 边界安全）：minified 产物输出单行可达数 MB，
            // 无界行 × 回放环 4000 条会吃掉数百 MB 内存。只截事件流展示副本，
            // 文件落盘保持完整（深度排障看日志文件）。
            let line = truncate_at_char(line, MAX_LOG_EVENT_LINE_BYTES);
            // 接收端关闭（理论不达）时静默丢弃该行
            drop(line_tx.send(line));
        });
        // 构建日志按 service_id 归档（稳定身份：改目录名日志归档连续，且与
        // 运行时 /app/logs/<service_id> 同轴）
        let log_dir = ws.join("logs").join(proj.service_id());
        // path 安全校验 + 拼接（防 `../` 穿越 workspace）
        let proj_dir = file_server::path_safety::ensure_within(&ws, &proj.dir).map_err(|_| {
            AppError::validation(format!(
                "project path escapes workspace: {} (service_id={})",
                proj.dir,
                proj.service_id()
            ))
        })?;
        if !proj_dir.is_dir() {
            return Err(AppError::resource(format!(
                "project dir not found: service_id={} (path={})",
                proj.dir,
                proj.service_id()
            )));
        }
        // on_pid 回调: spawn build 子进程后回写 pid 到 task, 供 cancel kill 进程组。
        let pid_cb = progress.as_ref().map(|p| move |pid: u32| p.set_pid(pid));
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
                on_line: Some(line_cb.clone()),
            },
            &_ws_guard,
        )
        .await;
        // 子进程已退出(或超时被 kill),pid 即将失效,清零缩短 stale-pid 窗口(#2)。
        if let Some(p) = &progress {
            p.clear_pid();
        }
        // 关通道（管道已 drain，行全部入队）→ 消费 task 排空 join：
        // 保证日志行全部先于本服务的 build_ok/build_fail 终态序。
        drop(line_cb);
        if let Err(e) = line_task.await {
            tracing::warn!(error = %e, "log line consumer task join failed");
        }
        let artifact = match build_result {
            Ok(a) => a,
            Err(e) => {
                // service_id 前缀：多服务 workspace 串行构建，快照 error 必须自明是
                // 哪个服务挂的（源错误已嵌该服务构建输出的尾部日志）。
                let wrapped = AppError::system(format!("{} build failed: {e}", proj.service_id()));
                // cancel(kill 进程组)导致的失败不 emit（终态 Cancelled 由 cancel handler 置）；
                // 否则 emit 服务级 BuildFail（任务级 Failed 由顶层 start_*_task 统一 emit）。
                if let Some(p) = &progress
                    && !p.is_cancelled()
                {
                    p.emit(BuildProgressEvent::BuildFail {
                        service: proj.service_id().to_string(),
                        error: wrapped.to_string(),
                    })
                    .await;
                }
                return Err(wrapped);
            }
        };
        if let Some(p) = &progress {
            p.emit(BuildProgressEvent::BuildOk {
                service: proj.service_id().to_string(),
            })
            .await;
        }
        built.push(BuiltProject {
            path: proj.dir.clone(),
            artifact,
        });
    }

    let pingap_version = required_release_metadata("RCODER_PINGAP_VERSION")?;
    let pingap_commit = required_release_metadata("RCODER_PINGAP_COMMIT")?;
    let runtime_image_digest = required_release_metadata("RCODER_RUNTIME_IMAGE_DIGEST")?;
    let lock = build_release_lock(
        &manifest,
        &discovered,
        ReleaseMetadata {
            release_id,
            pingap_version: &pingap_version,
            pingap_commit: &pingap_commit,
            minimum_app_cli_version: env!("CARGO_PKG_VERSION"),
            runtime_image_digest: &runtime_image_digest,
        },
    )
    .map_err(|e| AppError::business(e.to_string()))?;
    let file_name = format!("{WORKSPACE_PACKAGE_PREFIX}{release_id}.zip");
    // 产物落 {ws}/builds/ 子目录（assemble 的 ws.join(file_name) + create_dir_all(parent)
    // 天然支持含子目录前缀的 file_name）
    let rel_path = workspace_artifact_rel_path(release_id);
    let path = assemble_workspace_package(&ws, &built, &lock, &rel_path).await?;
    let (sha256, size_bytes) = hash_file(&path).await?;
    // 磁盘卫生（构建完成后顺带）：制品/temp 保留最近 N 个、main 日志按天数、
    // .staging 残留。fail-safe——清理失败不影响构建结果，仅 warn 留痕。
    let dev_log_dir = config.log_base_dir.join(app_id);
    let stats = hygiene::sweep_workspace(
        &ws,
        &dev_log_dir,
        config.build_artifact_retain_count,
        config.build_log_retention_days,
    )
    .await;
    tracing::info!(
        app_id,
        artifacts_removed = stats.artifacts_removed,
        temp_logs_removed = stats.temp_logs_removed,
        main_logs_removed = stats.main_logs_removed,
        staging_dirs_removed = stats.staging_dirs_removed,
        "[HYGIENE] workspace sweep done"
    );
    Ok(WorkspaceBuildArtifact {
        release_id: release_id.to_string(),
        path,
        rel_path,
        file_name,
        sha256,
        size_bytes,
    })
}

/// 异步发起 build 任务（不阻塞，立即返 task_id + 预生成的产物相对路径）。进度事件
/// 经 task 流出（SSE/轮询）。
///
/// 同 app_id 互斥由 `build_workspace_package` 最外层 `try_start(app_id)` 持有的
/// `BuildGuard` 保证(覆盖整个构建周期,跨所有子项目)。重复构建立即返回 409 fail-fast
/// (非排队);该 guard 同时占用 1 个全局并发 permit,以引用传给每个子项目 build_generic。
/// 非循环路径的 Err（如 release lock env 缺失）由这里兜底 emit Failed。
///
/// 返回 `(task_id, artifact_path)`——artifact_path 为预生成的确定性产物相对路径
/// （release_id 在此预生成，创建响应即可返回）。
pub async fn start_build_task(
    store: &BuildTaskStore,
    config: &Arc<file_server::Config>,
    build_manager: Arc<BuildManager>,
    app_id: String,
    timeout_secs: u64,
) -> Result<(BuildTaskId, String), AppError> {
    // 容量耗尽(全活跃任务达上限)→ 立即拒绝,不再越过上限插入(#12)。
    let task = store
        .create(app_id.clone(), BuildTaskKind::Build)
        .await
        .map_err(|e| AppError::business(e.to_string()))?;
    // release_id 预生成并预置进快照：创建响应（BuildCreatedData.artifact_path）与
    // pending 期轮询即可见确定性产物路径；build_workspace_package 消费同一值,
    // Completed 事件携带一致路径覆盖（两处同源）。
    let release_id = uuid::Uuid::now_v7().simple().to_string();
    let artifact_path = workspace_artifact_rel_path(&release_id);
    task.set_artifact_path(release_id.clone(), artifact_path.clone())
        .await;
    // 预 resolve workspace 根并存入 task,供 logs/SSE handler 解析日志目录
    // ({workspace}/logs/{service}/)。resolve 失败则 emit Failed 终态,不 spawn。
    match file_server::workspace::resolve_userapp_dev(&app_id, None, config) {
        Ok(ws) => task.set_workspace_root(ws).await,
        Err(e) => {
            task.emit(BuildProgressEvent::Failed {
                error: format!("resolve workspace: {e}"),
            })
            .await;
            return Ok((task.id.clone(), artifact_path));
        }
    }
    let task_spawn = task.clone();
    let config = Arc::clone(config);
    tokio::spawn(async move {
        let result = build_workspace_package(
            &config,
            build_manager.as_ref(),
            &app_id,
            &release_id,
            timeout_secs,
            Some(task_spawn.clone()),
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
                        artifact_path: artifact.rel_path.clone(),
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
    Ok((task.id.clone(), artifact_path))
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
