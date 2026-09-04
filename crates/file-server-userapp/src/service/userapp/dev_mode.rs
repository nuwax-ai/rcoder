//! dev 源码态链路（`[devrun]` 触发）：形态判定 / 源码目录 lock ensure / dev 编译。
//!
//! 任一 enabled 服务配了 `[devrun]` → 该 app 的 dev 形态 = 源码态：dev/start·
//! restart 不再部署 `.run` 产物，而是「dev 编译（三分派：`[devbuild]` 显式配置
//! 则执行；只配 `[devrun]` 的服务跳过——devrun 命令自足不消费产物；未配
//! `[devrun]` 的服务回落 `[build]` 刷新源码目录产物）→ ensure 源码目录
//! release.lock → app-cli 直接编排源码 workspace（`APP_CLI_RUN_PROFILE=dev`，
//! devrun 优先、run 兜底）」。未配置的 app 走产物态链路（编译 → zip →
//! `.run`）不受影响。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use file_server::error::{AppError, AppResult};
use file_server::service::build_manager::BuildManager;
use file_server::service::dev_server::log::{main_log_name, temp_log_name};
use file_server::service::dev_server::process::{CommandObservers, now_ms, run_command_to_log};
use shared_types::{BuildProgressEvent, DiscoveredProject};

use super::manifest::{
    ReleaseMetadata, build_release_lock, discover_projects, read_workspace_manifest,
};
use super::tasks::BuildTask;
use super::{required_release_metadata, spawn_build_log_pipe};

/// 源码目录 lock 文件名（与 app-cli `read_release_lock` 约定一致）。
const LOCK_FILE: &str = "release.lock.toml";

/// 该 workspace 的 dev 形态是否为源码态：任一 **enabled** 服务配了 `[devrun]`。
///
/// 单一事实源（dev/start 与 dev/restart 共用）；disabled 服务的 devrun 不触发
/// （与 lock 生成时 enabled 过滤一致，防「disabled 段把 app 拖进源码态」）。
pub fn dev_mode_enabled(ws: &Path) -> AppResult<bool> {
    let discovered = discover_ws_projects(ws)?;
    Ok(discovered
        .iter()
        .any(|project| project.manifest.project.enabled && project.manifest.devrun.is_some()))
}

/// dev 编译（三分派，见 [`devbuild_argv`]）：配了 `[devbuild]` 的服务执行之；
/// 只配 `[devrun]` 的服务跳过（devrun 自足，emit Log 说明）；其余回落
/// `[build].command` 刷新源码目录产物。
///
/// 与发布编译（[`super::build_workspace_package`]）共用执行框架（BuildGuard 互斥、
/// SSE 事件序 building → log\* → build_ok/build_fail、pid 回写供 cancel kill、
/// temp_log 落 `ws/logs/<service_id>`），差异：不校验产物存在（devbuild 常为纯
/// 检查命令如 type-check，不产 artifact.zip）、不组 workspace 包。失败上抛
/// （dev 任务终态 Failed、不启动——与产物态同语义）。
pub async fn run_dev_builds(
    build_manager: &BuildManager,
    app_id: &str,
    ws: &Path,
    timeout_secs: u64,
    progress: Option<Arc<BuildTask>>,
) -> AppResult<()> {
    let enabled: Vec<DiscoveredProject> = discover_ws_projects(ws)?
        .into_iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();

    // 与发布编译同款互斥（同 app_id 的 /build、dev 任务并发防穿插）
    let _ws_guard = build_manager.try_start(app_id)?;

    for proj in &enabled {
        // 软取消：服务间检查（硬 cancel 靠外部 kill 进程组，见 cancel handler）。
        if let Some(p) = &progress
            && p.is_cancelled()
        {
            return Err(AppError::business("build cancelled by user"));
        }
        // devrun 自足跳过：不 emit Building/BuildOk（跳过的服务不进入编译事件
        // 序），仅 emit Log 让 SSE 可见。
        let Some(argv) = devbuild_argv(proj) else {
            tracing::info!(
                service = proj.service_id(),
                "[DEV_BUILD] devrun 自足，跳过编译"
            );
            if let Some(p) = &progress {
                p.emit(BuildProgressEvent::Log {
                    service: proj.service_id().to_string(),
                    line: "devrun 自足（不消费构建产物），跳过编译".to_string(),
                })
                .await;
            }
            continue;
        };
        if let Some(p) = &progress {
            p.emit(BuildProgressEvent::Building {
                service: proj.service_id().to_string(),
            })
            .await;
        }

        let (line_cb, line_task) = spawn_build_log_pipe(&progress, proj.service_id());
        let log_dir = ws.join("logs").join(proj.service_id());
        let proj_dir = file_server::path_safety::ensure_within(ws, &proj.dir).map_err(|_| {
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
        // on_pid 回调: spawn 子进程后回写 pid 到 task, 供 cancel kill 进程组。
        let pid_cb = progress.as_ref().map(|p| move |pid: u32| p.set_pid(pid));
        let pid_ref: Option<&(dyn Fn(u32) + Send + Sync)> =
            pid_cb.as_ref().map(|c| c as &(dyn Fn(u32) + Send + Sync));

        let (program, args) = argv.split_first().ok_or_else(|| {
            AppError::validation("dev build command must have at least one argv item")
        })?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        tokio::fs::create_dir_all(&log_dir)
            .await
            .map_err(|e| AppError::system(format!("create dev build log dir: {e}")))?;
        let now = now_ms();
        let build_result = run_command_to_log(
            program,
            &args,
            &proj_dir,
            &log_dir.join(main_log_name()),
            &log_dir.join(temp_log_name(now)),
            timeout_secs,
            CommandObservers {
                on_pid: pid_ref,
                on_line: Some(line_cb.clone()),
            },
        )
        .await;

        if let Some(p) = &progress {
            p.clear_pid();
        }
        // 关通道（管道已 drain）→ 排空 join → 才 emit 终态（保序约定见管道 fn 文档）
        drop(line_cb);
        if let Err(e) = line_task.await {
            tracing::warn!(error = %e, "log line consumer task join failed");
        }

        // 服务名前缀错误（与发布编译同款：多服务串行时快照 error 自明是哪个服务挂的）
        let outcome = build_result
            .map(|_| ())
            .map_err(|e| AppError::system(format!("{} dev build failed: {e}", proj.service_id())));
        match outcome {
            Ok(()) => {
                if let Some(p) = &progress {
                    p.emit(BuildProgressEvent::BuildOk {
                        service: proj.service_id().to_string(),
                    })
                    .await;
                }
            }
            Err(wrapped) => {
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
        }
    }
    Ok(())
}

/// 服务的 dev 编译命令三分派（按产物消费方判定）：
/// - 配了 `[devbuild]` → 用之（显式检查/准备意图：type-check、依赖安装等，
///   不要求产出 artifact）；
/// - 未配 `[devbuild]` 但配了 `[devrun]` → `None`（**跳过编译**：devrun 命令
///   自足跑源码，构建产物零消费者；dev 命令确实要消费产物的非常规用法，
///   显式配 `[devbuild]` 强制构建）；
/// - 都未配 → 回落 `[build].command`（run.command 在源码目录消费产物，需刷新）。
fn devbuild_argv(project: &DiscoveredProject) -> Option<&[String]> {
    if let Some(devbuild) = &project.manifest.devbuild {
        return Some(devbuild.command.as_slice());
    }
    if project.manifest.devrun.is_some() {
        return None;
    }
    Some(project.manifest.build.command.as_slice())
}

/// ensure 源码目录 `release.lock.toml`：无 lock、或任一 manifest 比 lock 新
/// （mtime）→ 重新生成；新鲜则 no-op。返回编排 workspace 根（= 源码 ws 本身，
/// app-cli `--workspace` 指向这里）。
///
/// metadata 与发布链同源（env 必备——发布编译同进程已依赖）；
/// `minimum_app_cli_version` 取共享常量（app-cli 版本线，见其 doc 的纪律）。
/// 幂等，重复调用安全。
pub async fn ensure_dev_lock(ws: &Path) -> AppResult<PathBuf> {
    let lock_path = ws.join(LOCK_FILE);
    if fresh_lock(ws, &lock_path).await {
        return Ok(ws.to_path_buf());
    }

    let manifest = read_workspace_manifest(ws).await?;
    let discovered = discover_ws_projects(ws)?;
    if discovered.is_empty() {
        return Err(AppError::business(format!(
            "no sub-projects found under workspace={}",
            ws.display()
        )));
    }
    let pingap_version = required_release_metadata("RCODER_PINGAP_VERSION")?;
    let pingap_commit = required_release_metadata("RCODER_PINGAP_COMMIT")?;
    let runtime_image_digest = required_release_metadata("RCODER_RUNTIME_IMAGE_DIGEST")?;
    let release_id = uuid::Uuid::now_v7().simple().to_string();
    let lock = build_release_lock(
        &manifest,
        &discovered,
        ReleaseMetadata {
            release_id: &release_id,
            pingap_version: &pingap_version,
            pingap_commit: &pingap_commit,
            minimum_app_cli_version: shared_types::MINIMUM_APP_CLI_VERSION,
            runtime_image_digest: &runtime_image_digest,
        },
    )
    .map_err(|e| AppError::business(e.to_string()))?;
    let content = toml::to_string_pretty(&lock)
        .map_err(|e| AppError::system(format!("serialize {LOCK_FILE}: {e}")))?;
    tokio::fs::write(&lock_path, content)
        .await
        .map_err(|e| AppError::system(format!("write {}: {e}", lock_path.display())))?;
    tracing::info!(
        release_id,
        lock = %lock_path.display(),
        "[DEV_LOCK] source-mode release lock generated"
    );
    Ok(ws.to_path_buf())
}

/// lock 存在且比全部 manifests 新 → 新鲜（不重锁）。
async fn fresh_lock(ws: &Path, lock_path: &Path) -> bool {
    let Some(lock_mtime) = mtime_of(lock_path).await else {
        return false;
    };
    // workspace.manifest.toml 本身也算输入（pingap/bridge_service 段变更需重锁）
    let mut inputs = vec![ws.join("workspace.manifest.toml")];
    // 仅扫一级子目录（与 discover 同面），mtime 比对无需解析内容
    let Ok(mut rd) = tokio::fs::read_dir(ws).await else {
        return false;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            inputs.push(entry.path().join("project.manifest.toml"));
        }
    }
    let mut stale = false;
    for input in &inputs {
        if let Some(mtime) = mtime_of(input).await
            && mtime > lock_mtime
        {
            stale = true;
            break;
        }
    }
    !stale
}

async fn mtime_of(path: &Path) -> Option<SystemTime> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|meta| meta.modified().ok())
}

fn discover_ws_projects(ws: &Path) -> AppResult<Vec<DiscoveredProject>> {
    discover_projects(ws)
        .map_err(|e| AppError::system(format!("discover projects in {}: {e}", ws.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_ws() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.keep();
        fs::create_dir_all(path.join("frontend")).expect("frontend dir");
        fs::create_dir_all(path.join("backend")).expect("backend dir");
        path
    }

    fn write_manifest(dir: &Path, service_id: &str, extra: &str) {
        let content = format!(
            "schema_version = 1\n\
             [project]\nservice_id = '{service_id}'\nname = '{service_id}'\ntype = 'node'\n\
             {extra}\n\
             [build]\ncommand = ['true']\nartifact = 'artifact.zip'\n\
             [run]\ncommand = ['true']\n"
        );
        fs::write(dir.join("project.manifest.toml"), content).expect("write manifest");
    }

    /// 有 enabled 服务配 [devrun] → 源码态。
    #[test]
    fn devrun_on_any_enabled_service_enables_source_mode() {
        let ws = temp_ws();
        write_manifest(
            &ws.join("frontend"),
            "frontend",
            "[devrun]\ncommand = ['vite']",
        );
        write_manifest(&ws.join("backend"), "backend", "");
        assert!(dev_mode_enabled(&ws).expect("dev mode"));
    }

    /// 全部未配 → 产物态（现状链路）。
    #[test]
    fn no_devrun_keeps_artifact_mode() {
        let ws = temp_ws();
        write_manifest(&ws.join("frontend"), "frontend", "");
        write_manifest(&ws.join("backend"), "backend", "");
        assert!(!dev_mode_enabled(&ws).expect("dev mode"));
    }

    /// disabled 服务的 devrun 不触发（与 lock enabled 过滤一致）。严格 discover
    /// 对全 disabled workspace 本身报错（与产物态同语义），故用「disabled+devrun
    /// 与 enabled 无 devrun 并存」的合法形态验证。
    #[test]
    fn disabled_service_devrun_does_not_enable_source_mode() {
        let ws = temp_ws();
        write_manifest(
            &ws.join("frontend"),
            "frontend",
            "enabled = false\n[devrun]\ncommand = ['vite']",
        );
        write_manifest(&ws.join("backend"), "backend", "");
        assert!(
            !dev_mode_enabled(&ws).expect("dev mode"),
            "disabled devrun must not flip the workspace to source mode"
        );
    }

    /// 三态之一：devrun + devbuild → 执行 devbuild（显式检查/准备意图优先）。
    #[test]
    fn devbuild_argv_prefers_explicit_devbuild() {
        let ws = temp_ws();
        write_manifest(
            &ws.join("frontend"),
            "frontend",
            "[devbuild]\ncommand = ['pnpm', 'type-check']\n[devrun]\ncommand = ['vite']",
        );
        write_manifest(&ws.join("backend"), "backend", "");
        let discovered = discover_ws_projects(&ws).expect("discover");
        let frontend = discovered
            .iter()
            .find(|p| p.service_id() == "frontend")
            .expect("frontend");
        assert_eq!(
            devbuild_argv(frontend).map(Vec::from),
            Some(vec!["pnpm".to_string(), "type-check".to_string()])
        );
    }

    /// 三态之二：devrun 自足（未配 devbuild）→ 跳过编译。
    #[test]
    fn devrun_without_devbuild_skips_compile() {
        let ws = temp_ws();
        write_manifest(
            &ws.join("frontend"),
            "frontend",
            "[devrun]\ncommand = ['vite']",
        );
        let discovered = discover_ws_projects(&ws).expect("discover");
        let frontend = discovered
            .iter()
            .find(|p| p.service_id() == "frontend")
            .expect("frontend");
        assert_eq!(
            devbuild_argv(frontend),
            None,
            "devrun-only service must skip compile (devrun self-sufficient)"
        );
    }

    /// 三态之三：未配 devrun → 回落 [build].command（run.command 消费源码目录产物）。
    #[test]
    fn no_devrun_falls_back_to_build() {
        let ws = temp_ws();
        write_manifest(&ws.join("backend"), "backend", "");
        let discovered = discover_ws_projects(&ws).expect("discover");
        let backend = discovered
            .iter()
            .find(|p| p.service_id() == "backend")
            .expect("backend");
        assert_eq!(
            devbuild_argv(backend).map(Vec::from),
            Some(vec!["true".to_string()])
        );
    }

    fn set_mtime(path: &Path, mtime: SystemTime) {
        let file = fs::File::options().write(true).open(path).expect("open");
        file.set_modified(mtime).expect("set mtime");
    }

    /// fresh_lock 判定：无 lock → 不新鲜（需生成）；lock 比全部 manifest 新 →
    /// 新鲜（no-op）；任一 manifest 比 lock 新（agent 改过）→ 重锁。
    #[tokio::test]
    async fn fresh_lock_tracks_manifest_mtime() {
        let ws = temp_ws();
        fs::write(
            ws.join("workspace.manifest.toml"),
            "schema_version = 1\n[workspace]\nname = 'ws'\n",
        )
        .expect("workspace manifest");
        write_manifest(&ws.join("frontend"), "frontend", "");
        let lock_path = ws.join(LOCK_FILE);

        // 无 lock → 不新鲜
        assert!(!fresh_lock(&ws, &lock_path).await);

        // lock 存在且最新 → 新鲜（用显式 mtime 控制，避免文件系统时间精度 flaky）
        fs::write(&lock_path, "schema_version = 1\n").expect("lock");
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        set_mtime(&ws.join("workspace.manifest.toml"), base);
        set_mtime(&ws.join("frontend").join("project.manifest.toml"), base);
        set_mtime(&lock_path, base + std::time::Duration::from_secs(10));
        assert!(fresh_lock(&ws, &lock_path).await);

        // manifest 改动（mtime 新于 lock）→ 不新鲜
        set_mtime(
            &ws.join("frontend").join("project.manifest.toml"),
            base + std::time::Duration::from_secs(20),
        );
        assert!(!fresh_lock(&ws, &lock_path).await);
    }
}
