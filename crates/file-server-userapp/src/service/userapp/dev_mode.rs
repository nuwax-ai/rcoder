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
    ReleaseMetadata, build_release_lock, discover_projects_async, read_workspace_manifest,
};
use super::tasks::BuildTask;
use super::{required_release_metadata, spawn_build_log_pipe};

/// 源码目录 lock 文件名（与 app-cli `read_release_lock` 约定一致）。
const LOCK_FILE: &str = "release.lock.toml";

/// 该 workspace 的 dev 形态是否为源码态：任一 **enabled** 服务配了 `[devrun]`。
///
/// 单一事实源（dev/start 与 dev/restart 共用）；disabled 服务的 devrun 不触发
/// （与 lock 生成时 enabled 过滤一致，防「disabled 段把 app 拖进源码态」）。
pub async fn dev_mode_enabled(ws: &Path) -> AppResult<bool> {
    let discovered = discover_ws_projects(ws).await?;
    Ok(discovered
        .iter()
        .any(|project| project.manifest.project.enabled && project.manifest.devrun.is_some()))
}

/// dev 编译（三分派，见 `workspace_manifest::ProjectManifest::devbuild_argv`
/// ——单一事实源，与 app-cli 本地 `build` 子命令共用）：配了 `[devbuild]` 的
/// 服务执行之；只配 `[devrun]` 的服务跳过（devrun 自足，emit Log 说明）；
/// 其余回落 `[build].command` 刷新源码目录产物。
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
    let enabled: Vec<DiscoveredProject> = discover_ws_projects(ws)
        .await?
        .into_iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();
    let workspace = read_workspace_manifest(ws).await?;
    shared_types::validate_workspace_startup(&workspace, &enabled)
        .map_err(|error| AppError::business(error.to_string()))?;

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
        let Some(argv) = proj.manifest.devbuild_argv().map(Vec::from) else {
            tracing::info!(
                service = proj.service_id(),
                "[DEV_BUILD] devrun is self-contained; skipping build"
            );
            if let Some(p) = &progress {
                p.emit(BuildProgressEvent::Log {
                    service: proj.service_id().to_string(),
                    line: "devrun does not consume build artifacts; skipping build".to_string(),
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

        let log_dir = ws.join("logs").join(proj.service_id());
        let proj_dir = file_server::path_safety::ensure_within(ws, &proj.dir).map_err(|_| {
            AppError::validation(format!(
                "project path escapes workspace: {} (service_id={})",
                proj.dir,
                proj.service_id()
            ))
        })?;
        if !tokio::fs::metadata(&proj_dir)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return Err(AppError::resource(format!(
                "project dir not found: service_id={} (path={})",
                proj.dir,
                proj.service_id()
            )));
        }
        tokio::fs::create_dir_all(&log_dir)
            .await
            .map_err(|e| AppError::system(format!("create dev build log dir: {e}")))?;

        // 服务名前缀错误在自愈层内包装（多服务串行时快照 error 自明是哪个服务挂的）
        let outcome = devbuild_with_no_lockfile_heal(
            &argv,
            &proj_dir,
            &log_dir,
            timeout_secs,
            &progress,
            proj.service_id(),
        )
        .await;
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

/// 单次 devbuild 执行：pid 回写（供 cancel kill 进程组）、逐行日志管道、
/// temp_log 落 `log_dir`；失败带 `<service_id> dev build failed` 前缀——
/// 缺 lockfile 自愈检测依赖错误尾段里的机器码。
async fn run_devbuild_once(
    argv: &[String],
    proj_dir: &Path,
    log_dir: &Path,
    timeout_secs: u64,
    progress: &Option<Arc<BuildTask>>,
    service_id: &str,
) -> AppResult<()> {
    let (program, args) = argv.split_first().ok_or_else(|| {
        AppError::validation("dev build command must have at least one argv item")
    })?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (line_cb, line_task) = spawn_build_log_pipe(progress, service_id);
    // on_pid 回调: spawn 子进程后回写 pid 到 task, 供 cancel kill 进程组。
    let pid_cb = progress.as_ref().map(|p| move |pid: u32| p.set_pid(pid));
    let pid_ref: Option<&(dyn Fn(u32) + Send + Sync)> =
        pid_cb.as_ref().map(|c| c as &(dyn Fn(u32) + Send + Sync));
    let result = run_command_to_log(
        program,
        &args,
        proj_dir,
        &log_dir.join(main_log_name()),
        &log_dir.join(temp_log_name(now_ms())),
        timeout_secs,
        CommandObservers {
            on_pid: pid_ref,
            on_line: Some(line_cb.clone()),
        },
    )
    .await;
    if let Some(p) = progress {
        p.clear_pid();
    }
    // 关通道（管道已 drain）→ 排空 join → 才 emit 终态（保序约定见管道 fn 文档）
    drop(line_cb);
    if let Err(e) = line_task.await {
        tracing::warn!(error = %e, "log line consumer task join failed");
    }
    result
        .map(|_| ())
        .map_err(|e| AppError::system(format!("{service_id} dev build failed: {e}")))
}

/// dev 构建 + 缺 lockfile 自愈（app-171 事故，与发布编译链
/// `super::build_service_with_no_lockfile_heal` 对称）：平台导出链过滤
/// pnpm-lock.yaml（既定设计），存量 `[devbuild]` 命令若 `--frozen-lockfile`
/// 假设 lockfile 存在必失败。失败码为 `ERR_PNPM_NO_LOCKFILE` 时先 `pnpm
/// install` 生成 lockfile 再重试一次；非该码失败路径不变；重试仍失败才
/// 上抛并附自愈上下文。
async fn devbuild_with_no_lockfile_heal(
    argv: &[String],
    proj_dir: &Path,
    log_dir: &Path,
    timeout_secs: u64,
    progress: &Option<Arc<BuildTask>>,
    service_id: &str,
) -> AppResult<()> {
    let first = match run_devbuild_once(argv, proj_dir, log_dir, timeout_secs, progress, service_id)
        .await
    {
        Ok(()) => return Ok(()),
        Err(error) if super::is_pnpm_no_lockfile_failure(&error) => error,
        Err(error) => return Err(error),
    };
    super::emit_no_lockfile_heal_log(progress, service_id, &first).await;
    let install_error = super::heal_pnpm_install(proj_dir, log_dir, timeout_secs).await;
    match run_devbuild_once(argv, proj_dir, log_dir, timeout_secs, progress, service_id).await {
        Ok(()) => Ok(()),
        Err(retry) => Err(super::self_heal_retry_error(retry, install_error)),
    }
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
    let discovered = discover_ws_projects(ws).await?;
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

async fn discover_ws_projects(ws: &Path) -> AppResult<Vec<DiscoveredProject>> {
    discover_projects_async(ws)
        .await
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
    #[tokio::test]
    async fn devrun_on_any_enabled_service_enables_source_mode() {
        let ws = temp_ws();
        write_manifest(
            &ws.join("frontend"),
            "frontend",
            "[devrun]\ncommand = ['vite']",
        );
        write_manifest(&ws.join("backend"), "backend", "");
        assert!(dev_mode_enabled(&ws).await.expect("dev mode"));
    }

    /// 全部未配 → 产物态（现状链路）。
    #[tokio::test]
    async fn no_devrun_keeps_artifact_mode() {
        let ws = temp_ws();
        write_manifest(&ws.join("frontend"), "frontend", "");
        write_manifest(&ws.join("backend"), "backend", "");
        assert!(!dev_mode_enabled(&ws).await.expect("dev mode"));
    }

    /// disabled 服务的 devrun 不触发（与 lock enabled 过滤一致）。严格 discover
    /// 对全 disabled workspace 本身报错（与产物态同语义），故用「disabled+devrun
    /// 与 enabled 无 devrun 并存」的合法形态验证。
    #[tokio::test]
    async fn disabled_service_devrun_does_not_enable_source_mode() {
        let ws = temp_ws();
        write_manifest(
            &ws.join("frontend"),
            "frontend",
            "enabled = false\n[devrun]\ncommand = ['vite']",
        );
        write_manifest(&ws.join("backend"), "backend", "");
        assert!(
            !dev_mode_enabled(&ws).await.expect("dev mode"),
            "disabled devrun must not flip the workspace to source mode"
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

    // ===== 真实 pnpm devbuild 链路回归（app 110 事故修复）=====
    //
    // 走生产路径 run_dev_builds（[devbuild] argv 原样执行，不经 pnpm 封装层）。
    // fixture 为本地 file: 依赖 + sentinel 检查命令，离线可重复。依赖宿主
    // PATH 上的真实 pnpm——缺失时跳过并打印原因；验收必须在有 pnpm 的环境
    // 实跑（见 specs/pnpm-dev-install-recovery/verification.md）。

    fn pnpm_version_on_path() -> Option<String> {
        std::process::Command::new("pnpm")
            .arg("--version")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn local_dep(proj: &Path, name: &str) {
        let dir = proj.join("vendor").join(name);
        fs::create_dir_all(&dir).expect("vendor dep dir");
        fs::write(
            dir.join("package.json"),
            format!(r#"{{ "name": "{name}", "version": "1.0.0" }}"#),
        )
        .expect("vendor dep package.json");
    }

    fn write_pkg_json(proj: &Path, deps: &[(&str, &str)]) {
        let deps = deps
            .iter()
            .map(|(name, spec)| format!(r#""{name}": "{spec}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let content = format!(
            r#"{{ "name": "devbuild-fixture", "version": "1.0.0", "dependencies": {{ {deps} }} }}"#
        );
        fs::write(proj.join("package.json"), content).expect("project package.json");
    }

    /// frontend 工程（vendor/dep-a 本地依赖 + [devbuild] 由 extra 注入）。
    /// 返回 (workspace, sentinel 路径)；sentinel 存在 = 安装后的检查确实执行。
    fn pnpm_devbuild_ws(devbuild_extra: &str) -> (PathBuf, PathBuf) {
        let ws = temp_ws();
        fs::write(
            ws.join("workspace.manifest.toml"),
            "schema_version=1\n[workspace]\nname='devbuild-test'\n",
        )
        .unwrap();
        let proj = ws.join("frontend");
        local_dep(&proj, "dep-a");
        write_pkg_json(&proj, &[("dep-a", "file:./vendor/dep-a")]);
        write_manifest(&proj, "frontend", devbuild_extra);
        write_manifest(&ws.join("backend"), "backend", "");
        let sentinel = proj.join("check-ran");
        (ws, sentinel)
    }

    async fn run_dev_builds_on(ws: &Path, app_id: &str) -> AppResult<()> {
        let manager = BuildManager::new(1);
        run_dev_builds(&manager, app_id, ws, 300, None).await
    }

    fn devbuild_log_text(ws: &Path) -> String {
        let mut text = String::new();
        if let Ok(entries) = fs::read_dir(ws.join("logs").join("frontend")) {
            for entry in entries.flatten() {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    text.push_str(&content);
                }
            }
        }
        text
    }

    const FROZEN_DEVBUILD: &str =
        "[devbuild]\ncommand = ['sh', '-c', 'pnpm install --frozen-lockfile && touch check-ran']";
    const NO_FROZEN_DEVBUILD: &str = "[devbuild]\ncommand = ['sh', '-c', 'pnpm install --no-frozen-lockfile && touch check-ran']";

    /// app-171 事故形态：旧模板命令 --frozen-lockfile + 无 lockfile（平台导出
    /// 链过滤 lockfile 为既定设计）→ 自愈先 pnpm install 生成 lockfile 再重试
    /// 一次，dev build 成功且 && 后的检查确实执行。
    #[tokio::test]
    async fn devbuild_frozen_without_lockfile_heals_and_retries() {
        let Some(version) = pnpm_version_on_path() else {
            eprintln!("SKIP: pnpm not on PATH (real-pnpm devbuild regression)");
            return;
        };
        eprintln!("using pnpm {version}");
        let (ws, sentinel) = pnpm_devbuild_ws(FROZEN_DEVBUILD);
        run_dev_builds_on(&ws, "frozen-171")
            .await
            .expect("frozen install without lockfile must self-heal and succeed");
        assert!(sentinel.exists(), "check must run on the healed retry");
        assert!(
            ws.join("frontend").join("pnpm-lock.yaml").exists(),
            "self-heal must generate the lockfile"
        );
        assert!(
            devbuild_log_text(&ws).contains("ERR_PNPM_NO_LOCKFILE"),
            "first failure must be ERR_PNPM_NO_LOCKFILE (incident signature)"
        );
    }

    /// 恒定该码失败：恰重试一次（两次执行），终错携带自愈上下文且保留
    /// `<service> dev build failed` 前缀。
    #[tokio::test]
    async fn devbuild_token_failure_heals_exactly_once() {
        let Some(version) = pnpm_version_on_path() else {
            eprintln!("SKIP: pnpm not on PATH (real-pnpm devbuild regression)");
            return;
        };
        eprintln!("using pnpm {version}");
        let extra = "[devbuild]\ncommand = ['sh', '-c', 'echo run >> runs.txt; echo ERR_PNPM_NO_LOCKFILE >&2; exit 1']";
        let (ws, _sentinel) = pnpm_devbuild_ws(extra);
        let error = run_dev_builds_on(&ws, "token-fail")
            .await
            .expect_err("constant token failure stays failed");
        assert!(error.to_string().contains("dev build failed"), "{error}");
        assert!(
            error.to_string().contains("self-heal attempted"),
            "final error must carry the heal context"
        );
        assert_eq!(
            fs::read_to_string(ws.join("frontend").join("runs.txt"))
                .expect("runs")
                .lines()
                .count(),
            2,
            "exactly one heal retry"
        );
    }

    /// 新命令 + 无 lockfile：安装成功、生成 lockfile、后续检查确实执行。
    #[tokio::test]
    async fn devbuild_no_frozen_without_lockfile_installs_and_checks() {
        let Some(version) = pnpm_version_on_path() else {
            eprintln!("SKIP: pnpm not on PATH (real-pnpm devbuild regression)");
            return;
        };
        eprintln!("using pnpm {version}");
        let (ws, sentinel) = pnpm_devbuild_ws(NO_FROZEN_DEVBUILD);
        run_dev_builds_on(&ws, "nofrozen-new")
            .await
            .expect("devbuild must succeed");
        assert!(sentinel.exists(), "subsequent check must run");
        assert!(
            ws.join("frontend").join("pnpm-lock.yaml").exists(),
            "lockfile must be generated"
        );
    }

    /// 过期 lockfile（依赖声明真实变更，非无关字段）：--no-frozen-lockfile
    /// 允许安装并同步更新 lockfile。
    #[tokio::test]
    async fn devbuild_no_frozen_updates_stale_lockfile() {
        let Some(version) = pnpm_version_on_path() else {
            eprintln!("SKIP: pnpm not on PATH (real-pnpm devbuild regression)");
            return;
        };
        eprintln!("using pnpm {version}");
        let (ws, sentinel) = pnpm_devbuild_ws(NO_FROZEN_DEVBUILD);
        let proj = ws.join("frontend");
        run_dev_builds_on(&ws, "stale-1")
            .await
            .expect("initial install");
        local_dep(&proj, "dep-b");
        write_pkg_json(
            &proj,
            &[
                ("dep-a", "file:./vendor/dep-a"),
                ("dep-b", "file:./vendor/dep-b"),
            ],
        );
        fs::remove_file(&sentinel).expect("reset sentinel");
        run_dev_builds_on(&ws, "stale-2")
            .await
            .expect("stale lockfile install must succeed");
        let lockfile = fs::read_to_string(proj.join("pnpm-lock.yaml")).expect("lockfile");
        assert!(lockfile.contains("dep-b"), "lockfile must be updated");
        assert!(sentinel.exists(), "subsequent check must run");
    }

    /// lockfile 已匹配：重复安装幂等成功，检查照常执行。
    #[tokio::test]
    async fn devbuild_repeat_install_with_matching_lockfile_succeeds() {
        let Some(version) = pnpm_version_on_path() else {
            eprintln!("SKIP: pnpm not on PATH (real-pnpm devbuild regression)");
            return;
        };
        eprintln!("using pnpm {version}");
        let (ws, sentinel) = pnpm_devbuild_ws(NO_FROZEN_DEVBUILD);
        run_dev_builds_on(&ws, "repeat-1").await.expect("first run");
        fs::remove_file(&sentinel).expect("reset sentinel");
        run_dev_builds_on(&ws, "repeat-2")
            .await
            .expect("second run");
        assert!(sentinel.exists(), "check must run on repeat install");
    }

    /// 安装失败（file: 指向不存在的路径）：任务失败且后续检查未执行。
    #[tokio::test]
    async fn devbuild_install_failure_propagates_and_skips_check() {
        let Some(version) = pnpm_version_on_path() else {
            eprintln!("SKIP: pnpm not on PATH (real-pnpm devbuild regression)");
            return;
        };
        eprintln!("using pnpm {version}");
        let (ws, sentinel) = pnpm_devbuild_ws(NO_FROZEN_DEVBUILD);
        write_pkg_json(
            &ws.join("frontend"),
            &[("missing-dep", "file:./vendor/missing")],
        );
        let error = run_dev_builds_on(&ws, "install-fail")
            .await
            .expect_err("unresolvable dep must fail devbuild");
        assert!(error.to_string().contains("dev build failed"), "{error}");
        assert!(
            !sentinel.exists(),
            "check must not run after failed install"
        );
    }

    /// React 形态后续检查失败：安装成功但检查脚本非零 → run_dev_builds 仍
    /// 失败，不返回假成功。
    #[tokio::test]
    async fn devbuild_check_failure_fails_the_task() {
        let Some(version) = pnpm_version_on_path() else {
            eprintln!("SKIP: pnpm not on PATH (real-pnpm devbuild regression)");
            return;
        };
        eprintln!("using pnpm {version}");
        let extra = "[devbuild]\ncommand = ['sh', '-c', 'pnpm install --no-frozen-lockfile && node -e \"process.exit(3)\"']";
        let (ws, _sentinel) = pnpm_devbuild_ws(extra);
        let error = run_dev_builds_on(&ws, "check-fail")
            .await
            .expect_err("check failure must fail devbuild");
        assert!(error.to_string().contains("dev build failed"), "{error}");
        assert!(
            !error.to_string().contains("self-heal attempted"),
            "non-token failure must not heal"
        );
    }
}
