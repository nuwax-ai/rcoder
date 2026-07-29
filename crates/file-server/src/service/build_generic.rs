//! 通用 build：跑任意 manifest.build.cmd 产 artifact（给 UserApp workspace 打包调）。
//!
//! 与 build_project（网页 pnpm/vite，写死）并存，**不动旧逻辑**。
//! 工具链依赖 agent-runner 容器（Node/Python/Java-Maven/Rust 齐全；Go/Gradle 需镜像补装）。

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::service::build_manager::BuildManager;
use crate::service::dev_server::log::{main_log_name, temp_log_name};
use crate::service::dev_server::process::{now_ms, run_command_to_log};

/// 通用 build：在 workspace 内跑 shell `cmd`，产 `artifact_rel`。
///
/// 复用 `run_command_to_log`（日志管道 + 超时 + 进程组 kill）+ `BuildManager`（全局并发 +
/// 项目级互斥）。给 [`crate::service::userapp`] workspace 打包调用（project.manifest [build] → artifact）。
///
/// - `cmd` 经 `sh -c` 执行（支持管道/&&/env 等 shell 语法，如 `npm run build:standalone`）。
/// - 产物路径 = `cwd/artifact_rel`，跑完校验存在，不存在返 business 错。
/// - `log_dir` 存 build 日志（main + temp），调用方管理（如 agent workspace 的 logs/）。
///
/// # 工具链盲区
/// agent-runner 镜像当前缺 **Go** 和 **Gradle**（`Dockerfile.base` 未装）。这两类项目
/// 需先在镜像补装，或在 `cmd` 里自行 `curl` 拉取 —— 否则 build 会失败。
pub async fn build_generic(
    build_manager: &BuildManager,
    project_id: &str,
    cmd: &str,
    cwd: &Path,
    artifact_rel: &str,
    log_dir: &Path,
    timeout_secs: u64,
) -> AppResult<PathBuf> {
    tokio::fs::create_dir_all(log_dir)
        .await
        .map_err(|e| AppError::system(format!("create build log dir: {e}")))?;
    let now = now_ms();
    let main_log = log_dir.join(main_log_name());
    let temp_log = log_dir.join(temp_log_name(now));

    // 并发控制：全局信号量 + 项目级互斥（与 build_project 同机制）
    let _guard = build_manager.try_start(project_id)?;

    // 跑 cmd（sh -c 让 shell 解析管道/&& 等）
    run_command_to_log("sh", &["-c", cmd], cwd, &main_log, &temp_log, timeout_secs).await?;

    // 校验产物
    let artifact = cwd.join(artifact_rel);
    if !artifact.exists() {
        return Err(AppError::business(format!(
            "build produced no artifact: {artifact_rel} (cwd={})",
            cwd.display()
        )));
    }
    Ok(artifact)
}
