//! Userapp workspace 多项目打包：两级 manifest → 遍历子项目 build_generic → 组装整体包。
//!
//! - workspace 定位统一 [`file_server::workspace::resolve_userapp_dev`]（Userapp 开发卷, 容器无关）。
//!   workspace 根下有多个子项目（前端/后端/...）。
//! - file-server 严格读取 Manifest v1，并自动发现一级子项目。
//! - static 服务的产物在构建后做**路由一致性检查**（base/[proxy].path/strip_prefix/
//!   产物布局联动错配 → 构建失败并给修复指引，白屏前移到编译期拦截）。
//! - 组装成版本化整体包 `builds/workspace-package-<release_id>.zip`，内含 release lock。
//!
//! 子模块：
//! - `manifest`：两级 manifest 类型 + 解析
//! - `assemble`：整体包 zip 组装（raw copy 子产物 + 入口文件 + pingap 配置写入）
//! - `pingap`：pingap 反代配置（`pingap.toml`）+ `.service-ports` 生成（独立可扩展）

mod assemble;
pub mod dev_mode;
pub mod hygiene;
pub mod import;
mod manifest;
pub mod run_dir;
pub(crate) mod start_events;
pub mod tasks;

// 重导出 manifest 类型：保持 userapp 模块公开面。
pub use manifest::{
    BuildSection, ProjectManifest, ProjectMeta, ProxySection, RunSection, WorkspaceManifest,
    WorkspaceMeta,
};
// handlers 域同样要走阻塞池版扫描（manifest 是本模块私有子模块，够不到其内部项）
pub(crate) use manifest::discover_projects_async;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use file_server::error::{AppError, AppResult};
use file_server::service::build_generic::{GenericBuildRequest, build_generic};
use file_server::service::build_manager::{BuildGuard, BuildManager};
use shared_types::ProjectType;

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

/// pnpm "缺 lockfile" 机器码——构建自愈的精确触发信号。
const PNPM_NO_LOCKFILE_TOKEN: &str = "ERR_PNPM_NO_LOCKFILE";
/// `run_command_to_log` 错误 Display 的输出尾部标记：token 只在此后匹配，
/// 防止用户源码文本提前出现同词误报（对齐 classify.rs"机器码整 token 才参与
/// 分类"的哲学）。
const BUILD_OUTPUT_TAIL_MARKER: &str = "--- output tail (";

/// 构建失败是否为 pnpm 缺 lockfile：错误 Display 按尾部标记切分，只在
/// 输出尾段做整 token 等值匹配。
pub(super) fn is_pnpm_no_lockfile_failure(error: &AppError) -> bool {
    let text = error.to_string();
    let Some((_, tail)) = text.split_once(BUILD_OUTPUT_TAIL_MARKER) else {
        return false;
    };
    tail.split_whitespace().any(|token| {
        token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            == PNPM_NO_LOCKFILE_TOKEN
    })
}

/// 单服务构建 + 缺 lockfile 自愈（app-171 事故）：平台导出链过滤
/// pnpm-lock.yaml（既定设计），构建脚本若假设 lockfile 存在（`--frozen-
/// lockfile`）必失败。失败码为 `ERR_PNPM_NO_LOCKFILE` 时先 `pnpm install`
/// 生成 lockfile 再重试一次构建；安装失败不放弃重试（pnpm 先写 lockfile
/// 再跑 postinstall，部分安装后 lockfile 已可用）；重试仍失败才上抛并附
/// 自愈上下文。非该码失败路径不变。dev 编译链同款自愈见
/// `dev_mode::devbuild_with_no_lockfile_heal`（公告/安装/终错三段共用）。
async fn build_service_with_no_lockfile_heal(
    request: &GenericBuildRequest<'_>,
    guard: &BuildGuard<'_>,
    progress: &Option<Arc<BuildTask>>,
    service_id: &str,
) -> AppResult<PathBuf> {
    let first = match build_generic(request, guard).await {
        Ok(artifact) => return Ok(artifact),
        Err(error) if is_pnpm_no_lockfile_failure(&error) => error,
        Err(error) => return Err(error),
    };
    emit_no_lockfile_heal_log(progress, service_id, &first).await;
    let install_error = heal_pnpm_install(request.cwd, request.log_dir, request.timeout_secs).await;
    match build_generic(request, guard).await {
        Ok(artifact) => Ok(artifact),
        Err(retry) => Err(self_heal_retry_error(retry, install_error)),
    }
}

/// 缺 lockfile 自愈的 SSE 公告（发布与 dev 两条构建链共用）：命中机器码后
/// 先告知自愈动作与首败全文，日志流里可辨"自愈重试"段。
async fn emit_no_lockfile_heal_log(
    progress: &Option<Arc<BuildTask>>,
    service_id: &str,
    first: &AppError,
) {
    if let Some(task) = progress {
        task.emit(BuildProgressEvent::Log {
            service: service_id.to_string(),
            line: format!(
                "[self-heal] {PNPM_NO_LOCKFILE_TOKEN}: generating pnpm-lock.yaml via pnpm install, retrying build once (first failure: {first})"
            ),
        })
        .await;
    }
}

/// 缺 lockfile 自愈的安装段（两条构建链共用）：在 cwd 执行 `pnpm install`
/// 生成 lockfile（默认 --no-frozen-lockfile + 无 CI env；安装日志与构建日志
/// 同轴 logs/<service_id>：main + 新 dev-temp）；安装失败不放弃重试（pnpm
/// 先写 lockfile 再跑 postinstall，部分安装后 lockfile 已可用）。返回安装
/// 错误文本（None=安装成功）。
async fn heal_pnpm_install(cwd: &Path, log_dir: &Path, timeout_secs: u64) -> Option<String> {
    let install_logs = file_server::service::pnpm::LogFiles::new(
        log_dir.join(file_server::service::dev_server::log::main_log_name()),
        log_dir.join(file_server::service::dev_server::log::temp_log_name(
            file_server::service::dev_server::now_ms(),
        )),
    );
    match file_server::service::pnpm::install(
        cwd,
        &file_server::service::pnpm::InstallOptions::prefer_offline(),
        Some(&install_logs),
        timeout_secs,
    )
    .await
    {
        Ok(_) => None,
        Err(error) => Some(error.to_string()),
    }
}

/// 缺 lockfile 自愈的重试终错（两条构建链共用）：附自愈上下文（含安装失败
/// 原因，若有）。
fn self_heal_retry_error(retry: AppError, install_error: Option<String>) -> AppError {
    AppError::system(format!(
        "{retry}\n(self-heal attempted: pnpm install + one build retry{})",
        install_error
            .map(|error| format!("; install failed: {error}"))
            .unwrap_or_default()
    ))
}

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
/// 1. `resolve_userapp_dev(app_id)` → workspace 根（Userapp 开发卷）
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
    // 1. workspace 根（Userapp 开发卷, 容器无关）
    let ws = file_server::workspace::resolve_userapp_dev(app_id, None, config)?;
    if !tokio::fs::metadata(&ws)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(AppError::resource(format!(
            "Userapp workspace not found: {} (app_id={app_id})",
            ws.display()
        )));
    }

    // 2. 严格解析 workspace Manifest v1。
    let manifest = read_workspace_manifest(&ws).await?;

    // 3. 自动发现子项目（扫描含 project.manifest.toml 的一级子目录）
    let discovered = discover_projects_async(&ws)
        .await
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
        // 构建输出逐行转 Log 事件（共用管道，保序约定见 fn 文档）
        let (line_cb, line_task) = spawn_build_log_pipe(&progress, proj.service_id());
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
        // on_pid 回调: spawn build 子进程后回写 pid 到 task, 供 cancel kill 进程组。
        let pid_cb = progress.as_ref().map(|p| move |pid: u32| p.set_pid(pid));
        let pid_ref: Option<&(dyn Fn(u32) + Send + Sync)> =
            pid_cb.as_ref().map(|c| c as &(dyn Fn(u32) + Send + Sync));
        let build_result = build_service_with_no_lockfile_heal(
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
            &progress,
            proj.service_id(),
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
        // static+proxy 产物路由一致性检查（zip 产物不查）：错配即构建失败，
        // 走与构建失败完全相同的失败链（BuildFail 事件 + 任务终态 Failed、不启动）。
        // 生产 /api/v1/userapp/build 与 dev/start 产物态共用本函数——一处接线两链都拦。
        if proj.manifest.project.r#type == ProjectType::Static
            && let Some(proxy) = &proj.manifest.proxy
            && tokio::fs::metadata(&artifact)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false)
        {
            // frontend-detector 是纯同步 crate（内部同步读 index.html + 逐引用 stat），
            // 移进阻塞池执行，不占 tokio worker
            let dist = artifact.clone();
            let proxy_path = proxy.path.clone();
            let strip_prefix = proxy.strip_prefix;
            let aligned = tokio::task::spawn_blocking(move || {
                frontend_detector::check_static_proxy_alignment(&dist, &proxy_path, strip_prefix)
            })
            .await
            .map_err(|e| AppError::system(format!("proxy alignment task: {e}")))?;
            if let Err(msg) = aligned {
                let wrapped = AppError::business(format!("{}: {msg}", proj.service_id()));
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
            minimum_app_cli_version: shared_types::MINIMUM_APP_CLI_VERSION,
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

/// dev/build 受理前置校验结果：workspace 绝对路径 + 源码态判定。
#[derive(Debug)]
pub struct DevWorkspacePrecheck {
    /// workspace 根（受理期已 resolve，任务快照与编译链共用）。
    pub ws: PathBuf,
    /// 是否源码态（任一 enabled 服务配 `[devrun]`；dev 编译形态分派用，
    /// `/build` 产物态打包不消费此值）。
    pub dev_source_mode: bool,
}

/// 受理前置校验失败——同步 4xx 拒绝（不创建任务），由壳层映射专用错误码。
#[derive(Debug)]
pub enum DevPrecheckError {
    /// workspace 路径解析失败（appId 非法等）——沿用通用校验错误。
    Resolve(AppError),
    /// workspace 不存在或没有任何文件——`ERR_WORKSPACE_EMPTY`。
    WorkspaceEmpty(String),
    /// discover 失败（manifest 缺失/损坏、无 enabled 服务）——
    /// `ERR_WORKSPACE_NO_SERVICES`（message 保留 discover 的 fix 指引文案）。
    NoServices(String),
}

/// dev/build 任务受理前置校验：workspace 就绪性快速失败。
///
/// 空目录 / manifest 无可用服务直接拒绝受理，不再创建"受理成功 → 秒级
/// failed"的任务——失败对调用方即刻可见（HTTP 4xx + 专用错误码），也免去
/// 任务注册与上游轮询开销。`/build`（产物态打包）、`/dev/start`、
/// `/dev/restart` 三链共用：discover 失败对三条链路同样致命。
pub async fn precheck_dev_workspace(
    app_id: &str,
    config: &Arc<file_server::Config>,
) -> Result<DevWorkspacePrecheck, DevPrecheckError> {
    let ws = file_server::workspace::resolve_userapp_dev(app_id, None, config)
        .map_err(DevPrecheckError::Resolve)?;
    // 廉价空检查：目录不存在或无任何条目 = 尚未创建/导入项目。空目录走
    // discover 只会报 "no enabled services"，与 manifest 损坏无法区分——
    // 提前分流才能给出 ERR_WORKSPACE_EMPTY 的精确指引。
    let has_entries = match tokio::fs::read_dir(&ws).await {
        Ok(mut entries) => matches!(entries.next_entry().await, Ok(Some(_))),
        Err(_) => false,
    };
    if !has_entries {
        return Err(DevPrecheckError::WorkspaceEmpty(format!(
            "userapp workspace is empty: create or import a project first ({})",
            ws.display()
        )));
    }
    // 非空但没有任何服务目录：与"manifest 全 disabled/语法错"分流——前者
    // 的修复动作是先放项目（初始化模板/上传），后者才是查 manifest 配置。
    // 混在一个文案会把排障方向带偏（线上 app 35：空 workspace 被误读成
    // 接口/manifest 问题）。message 英文（项目惯例：错误码经 i18n 表
    // error.workspace_* 三语翻译，本地化由调用方按 code 驱动）。
    if !any_project_manifest(&ws).await {
        return Err(DevPrecheckError::NoServices(format!(
            "workspace has no service directories: no project.manifest.toml \
             found under {} — initialize a project template first, or place \
             a project directory containing project.manifest.toml",
            ws.display()
        )));
    }
    let dev_source_mode = dev_mode::dev_mode_enabled(&ws).await.map_err(|e| {
        // e 内部已含 "discover projects in {ws}: " 前缀（dev_mode.rs 包装），
        // 此处只加链路标识——再拼路径会双前缀重复
        DevPrecheckError::NoServices(format!("dev mode detection: {e}"))
    })?;
    Ok(DevWorkspacePrecheck {
        ws,
        dev_source_mode,
    })
}

/// workspace 一级子目录中是否存在任何 `project.manifest.toml`。
///
/// 只做文件存在性探测，不解析内容——用于 [`precheck_dev_workspace`] 的
/// 错误文案分流（"先放项目" vs "查 manifest 配置"）。
async fn any_project_manifest(ws: &Path) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(ws).await else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if tokio::fs::metadata(entry.path().join("project.manifest.toml"))
            .await
            .is_ok()
        {
            return true;
        }
    }
    false
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
    ws: PathBuf,
    app_id: String,
    timeout_secs: u64,
) -> Result<(BuildTaskId, String), AppError> {
    let workspace_activity = store.workspace_activity(&app_id).await.read_owned().await;
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
    // workspace 根由受理前置校验（precheck_dev_workspace）resolve 后传入：
    // 空目录/manifest 无可用服务已在受理期同步拒绝，此处不再有 resolve 失败分支。
    task.set_workspace_root(ws).await;
    let task_spawn = task.clone();
    let config = Arc::clone(config);
    let context = task.command_context();
    store
        .workers
        .spawn_identified(
            context.identity.clone(),
            context.scope(async move {
                let _workspace_activity = workspace_activity;
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
            }),
        )
        .map_err(AppError::system)?;

    Ok((task.id.clone(), artifact_path))
}

/// 构建输出行回调（同步 send 进管道通道；Arc 式跨入 spawn task）。
pub(super) type BuildLineCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// 构建输出逐行转 Log 事件的管道：unbounded 通道 + 独立消费 task（emit 为
/// async，管道回调同步 send 行）。返回 (行回调, 消费 task join 句柄)。
///
/// 调用方保序约定（building → log\* → build_ok/build_fail 严格有序）：
/// run_command_to_log 返回前已 drain 管道——返回后 drop 行回调关通道 →
/// await join 排空 → 才 emit 本服务的 BuildOk/BuildFail。
pub(super) fn spawn_build_log_pipe(
    progress: &Option<Arc<BuildTask>>,
    service_id: &str,
) -> (BuildLineCallback, tokio::task::JoinHandle<()>) {
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let line_task = {
        let task = progress.clone();
        let service = service_id.to_string();
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
    (line_cb, line_task)
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

async fn hash_file(path: &Path) -> AppResult<(String, u64)> {
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

#[cfg(test)]
mod truncate_tests {
    use super::*;

    /// UTF-8 边界安全截断：多字节字符中间回退到合法边界并以省略号标记；
    /// 未超限原样返回。
    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate_at_char("abc", 16), "abc");
        let long = "行".repeat(6000); // 18000 字节 > 16KB 上限
        let out = truncate_at_char(&long, MAX_LOG_EVENT_LINE_BYTES);
        assert!(out.len() <= MAX_LOG_EVENT_LINE_BYTES + 3);
        assert!(out.ends_with('…'));
        // 边界回退：max 落在多字节字符中间（'中' 占字节 1-3，max=2 回退到 1）
        let mixed = "a中b";
        assert_eq!(truncate_at_char(mixed, 2), "a…");
        // max 恰在合法边界（'中' 结束于字节 4）：不回退、不丢字符
        assert_eq!(truncate_at_char(mixed, 4), "a中…");
    }
}

#[cfg(test)]
mod precheck_tests {
    use super::*;

    fn config_with_root(root: &Path) -> Arc<file_server::Config> {
        Arc::new(file_server::Config {
            userapp_workspace_dir: root.to_path_buf(),
            ..Default::default()
        })
    }

    /// 与 dev_mode 测试同款 manifest fixture（enabled 服务，无 [devrun]）。
    fn write_manifest(dir: &Path, service_id: &str) {
        let content = format!(
            "schema_version = 1\n\
             [project]\nservice_id = '{service_id}'\nname = '{service_id}'\ntype = 'node'\n\
             [build]\ncommand = ['true']\nartifact = 'artifact.zip'\n\
             [run]\ncommand = ['true']\n"
        );
        std::fs::write(dir.join("project.manifest.toml"), content).expect("write manifest");
    }

    /// workspace 不存在 / 存在但空 → WorkspaceEmpty（message 带路径指引）。
    /// 这两个场景不再走任务链报笼统的 "no enabled services"。
    #[tokio::test]
    async fn precheck_rejects_missing_or_empty_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = config_with_root(tmp.path());

        // 目录不存在
        match precheck_dev_workspace("app-7", &cfg).await {
            Err(DevPrecheckError::WorkspaceEmpty(msg)) => {
                assert!(msg.contains("app-7"), "message should mention path: {msg}");
            }
            other => panic!("expected WorkspaceEmpty, got {other:?}"),
        }

        // 目录存在但没有任何条目
        tokio::fs::create_dir_all(tmp.path().join("app-7"))
            .await
            .expect("mkdir");
        assert!(matches!(
            precheck_dev_workspace("app-7", &cfg).await,
            Err(DevPrecheckError::WorkspaceEmpty(_))
        ));
    }

    /// 非空但没有任何 project.manifest.toml → NoServices（保留 discover 的
    /// fix 指引文案，与旧任务 error 文本同源）。
    #[tokio::test]
    async fn precheck_rejects_workspace_without_services() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = config_with_root(tmp.path());
        let ws = tmp.path().join("app-7");
        tokio::fs::create_dir_all(ws.join("some-dir"))
            .await
            .expect("mkdir");

        match precheck_dev_workspace("app-7", &cfg).await {
            Err(DevPrecheckError::NoServices(msg)) => {
                // 非空但无任何服务目录 → "先放项目"指引（不走 discover 文案）
                assert!(msg.contains("no service directories"), "msg={msg}");
                assert!(msg.contains("project.manifest.toml"), "msg={msg}");
                assert_eq!(
                    msg.matches("app-7").count(),
                    1,
                    "workspace path must not duplicate: {msg}"
                );
            }
            _ => panic!("expected NoServices"),
        }
    }

    /// 有服务目录但全部 disabled → discover 路径原文案（查 manifest 配置的
    /// fix 指引）——与"没有服务目录"分流的回归锁。
    #[tokio::test]
    async fn precheck_all_disabled_keeps_manifest_fix_hint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = config_with_root(tmp.path());
        let ws = tmp.path().join("app-7");
        let svc = ws.join("backend-go");
        tokio::fs::create_dir_all(&svc).await.expect("mkdir");
        let manifest = "schema_version = 1\n\n[project]\nservice_id = \"backend-go\"\n\
             name = \"Go\"\ntype = \"go\"\nenabled = false\n\n\
             [build]\ncommand = [\"true\"]\nartifact = \"a.zip\"\n\n\
             [run]\ncommand = [\"true\"]\n";
        tokio::fs::write(svc.join("project.manifest.toml"), manifest)
            .await
            .expect("write manifest");

        match precheck_dev_workspace("app-7", &cfg).await {
            Err(DevPrecheckError::NoServices(msg)) => {
                assert!(msg.contains("no enabled services"), "msg={msg}");
                assert!(msg.contains("fix"), "fix hint expected: {msg}");
                assert_eq!(
                    msg.matches("discover projects").count(),
                    1,
                    "prefix must not duplicate: {msg}"
                );
            }
            _ => panic!("expected NoServices"),
        }
    }

    /// 正常 workspace（enabled 服务）→ 放行；无 [devrun] 时产物态（false），
    /// 配 [devrun] 时源码态（true）——判定与 dev_mode 单一事实源一致。
    #[tokio::test]
    async fn precheck_passes_valid_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = config_with_root(tmp.path());
        let ws = tmp.path().join("app-7");
        let frontend = ws.join("frontend");
        tokio::fs::create_dir_all(&frontend).await.expect("mkdir");
        write_manifest(&frontend, "frontend");

        let precheck = precheck_dev_workspace("app-7", &cfg)
            .await
            .expect("valid workspace passes");
        assert!(!precheck.dev_source_mode, "artifact mode without [devrun]");
        assert!(precheck.ws.ends_with("app-7"));

        // 追加 [devrun] → 源码态（build/run 段为 manifest 必填，dev_mode 测试同款）
        let devrun = "schema_version = 1\n\
             [project]\nservice_id = 'hot'\nname = 'hot'\ntype = 'node'\n\
             [devrun]\ncommand = ['true']\n\
             [build]\ncommand = ['true']\nartifact = 'artifact.zip'\n\
             [run]\ncommand = ['true']\n";
        let hot = ws.join("hot");
        tokio::fs::create_dir_all(&hot).await.expect("mkdir");
        tokio::fs::write(hot.join("project.manifest.toml"), devrun)
            .await
            .expect("write devrun manifest");
        let precheck = precheck_dev_workspace("app-7", &cfg)
            .await
            .expect("devrun workspace passes");
        assert!(precheck.dev_source_mode, "source mode with [devrun]");
    }

    /// appId 非法 → Resolve（沿用通用校验错误，非专用码）。
    #[tokio::test]
    async fn precheck_maps_illegal_app_id_to_resolve_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = config_with_root(tmp.path());
        assert!(matches!(
            precheck_dev_workspace("../evil", &cfg).await,
            Err(DevPrecheckError::Resolve(_))
        ));
    }
}

#[cfg(test)]
mod no_lockfile_heal_tests {
    use super::*;

    fn app_error(message: impl Into<String>) -> AppError {
        AppError::system(message.into())
    }

    /// 检测矩阵:token 只在输出尾段计;用户源码文本提前出现同词不算;
    /// 其他码/无尾段不算。
    #[test]
    fn no_lockfile_detection_is_tail_scoped_and_token_exact() {
        let tail = |body: &str| {
            app_error(format!(
                "command exited non-zero: exit status: 1\n--- output tail (dev-temp-1.log, last 3 lines) ---\n{body}"
            ))
        };
        assert!(is_pnpm_no_lockfile_failure(&tail(
            "ERR_PNPM_NO_LOCKFILE Cannot install with frozen-lockfile"
        )));
        assert!(is_pnpm_no_lockfile_failure(&tail(
            "Note: `ERR_PNPM_NO_LOCKFILE` is the code."
        )));
        assert!(
            !is_pnpm_no_lockfile_failure(&tail("ERR_PNPM_OUTDATED_LOCKFILE")),
            "相邻码不得命中"
        );
        assert!(
            !is_pnpm_no_lockfile_failure(&tail("ERR_PNPM_NO_LOCKFILE_X")),
            "仅前缀相同的长 token 不得命中"
        );
        assert!(
            !is_pnpm_no_lockfile_failure(&app_error(
                "echo ERR_PNPM_NO_LOCKFILE && exit 1\n--- output tail (x.log, last 1 lines) ---\nplain failure"
            )),
            "token 只出现在尾段标记之前(用户输出正文)不得命中"
        );
        assert!(!is_pnpm_no_lockfile_failure(&app_error(
            "command timed out after 30s"
        )));
    }

    fn pnpm_on_path() -> bool {
        std::process::Command::new("pnpm")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn write_local_dep_project(dir: &Path) {
        std::fs::create_dir_all(dir.join("vendor").join("vend")).expect("vendor dir");
        std::fs::write(
            dir.join("vendor").join("vend").join("package.json"),
            r#"{ "name": "vend", "version": "1.0.0" }"#,
        )
        .expect("vendor pkg");
        std::fs::write(
            dir.join("package.json"),
            r#"{ "name": "heal-fixture", "version": "1.0.0", "dependencies": { "vend": "file:./vendor/vend" } }"#,
        )
        .expect("package.json");
    }

    async fn run_heal(dir: &Path, argv: &[&str], artifact_rel: &str) -> AppResult<PathBuf> {
        let manager = BuildManager::new(1);
        let guard = manager.try_start("heal-test").expect("guard");
        let command: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let log_dir = dir.join("logs");
        let request = GenericBuildRequest {
            argv: &command,
            cwd: dir,
            artifact_rel,
            log_dir: &log_dir,
            timeout_secs: 180,
            on_pid: None,
            on_line: None,
        };
        build_service_with_no_lockfile_heal(&request, &guard, &None, "fixture").await
    }

    /// app-171 事故形态:构建脚本 --frozen-lockfile + 无 lockfile → 自愈生成
    /// lockfile 后重试成功。
    #[tokio::test]
    async fn frozen_build_without_lockfile_heals_and_retries() {
        if !pnpm_on_path() {
            eprintln!("skip: pnpm not on PATH");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        write_local_dep_project(dir.path());
        let result = run_heal(
            dir.path(),
            &[
                "sh",
                "-c",
                "pnpm install --frozen-lockfile && mkdir -p dist && echo ok > dist/index.html",
            ],
            "dist",
        )
        .await;
        let artifact = result.expect("healed build succeeds");
        assert!(artifact.ends_with("dist"));
        assert!(
            dir.path().join("pnpm-lock.yaml").is_file(),
            "self-heal must generate the lockfile"
        );
    }

    /// 反例:非该码失败不自愈不重试(恰一次执行)。
    #[tokio::test]
    async fn non_token_failure_does_not_heal_or_retry() {
        let dir = tempfile::tempdir().expect("dir");
        let runs = dir.path().join("runs.txt");
        let error = run_heal(
            dir.path(),
            &["sh", "-c", "echo run >> runs.txt; exit 7"],
            "dist",
        )
        .await
        .expect_err("plain failure stays failed");
        assert_eq!(
            std::fs::read_to_string(&runs)
                .expect("runs")
                .lines()
                .count(),
            1,
            "non-token failure must not retry"
        );
        assert!(
            !error.to_string().contains("self-heal attempted"),
            "no heal context on plain failures"
        );
    }

    /// 恒定该码失败:恰重试一次(两次执行),终错携带自愈上下文。
    #[tokio::test]
    async fn token_failure_heals_exactly_once() {
        if !pnpm_on_path() {
            eprintln!("skip: pnpm not on PATH");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "name": "always-fail", "version": "1.0.0" }"#,
        )
        .expect("package.json");
        let error = run_heal(
            dir.path(),
            &[
                "sh",
                "-c",
                "echo run >> runs.txt; echo 'ERR_PNPM_NO_LOCKFILE' >&2; exit 1",
            ],
            "dist",
        )
        .await
        .expect_err("retry failure surfaces");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("runs.txt"))
                .expect("runs")
                .lines()
                .count(),
            2,
            "exactly one heal retry"
        );
        assert!(
            error.to_string().contains("self-heal attempted"),
            "final error must carry the heal context"
        );
    }
}
