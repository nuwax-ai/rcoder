//! 通用 build：以 argv 启动 manifest.build.command 并产出 artifact。
//!
//! 与 build_project（网页 pnpm/vite，写死）并存，**不动旧逻辑**。
//! 工具链依赖 agent-runner 容器（Node/Python/Java-Maven/Rust 齐全；Go/Gradle 需镜像补装）。

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::service::build_manager::BuildGuard;
use crate::service::dev_server::log::{main_log_name, temp_log_name};
use crate::service::dev_server::process::{now_ms, run_command_to_log};

/// 通用 build 请求参数（命令 + 产物 + 日志 + 超时 + pid 回调）。
///
/// 把 `build_generic` 的执行参数打包为参数对象（SOLID：相关参数聚合），避免函数签名过长。
/// 并发控制(项目互斥 + 全局 permit)由调用方持 [`BuildGuard`] 保证,不在此聚合。
pub struct GenericBuildRequest<'a> {
    /// build 命令 argv（不经过 shell；需 shell 语法时显式写成 `["sh", "-c", "..."]`）。
    pub argv: &'a [String],
    /// 工作目录（命令的 cwd）。
    pub cwd: &'a Path,
    /// 产物相对 cwd 的路径，跑完校验存在，不存在返 business 错。
    pub artifact_rel: &'a str,
    /// 日志目录（main + temp），调用方管理（如 agent workspace 的 logs/）。
    pub log_dir: &'a Path,
    /// 单命令超时秒数。
    pub timeout_secs: u64,
    /// spawn 后回调 child pid（供外部 cancel kill 进程组）；None 则不回调。
    pub on_pid: Option<&'a (dyn Fn(u32) + Send + Sync)>,
    /// 每行输出回调（原始行，时间戳前缀之前；供上层实时推送 SSE `log` 事件）；
    /// None 则不回调。与文件写入同源（vite 噪音行同样被过滤）。
    /// Arc 所有权式（区别于 on_pid 的借用式）：回调需跨入日志管道的 spawn task（'static）。
    pub on_line: Option<crate::service::dev_server::process::OnLineCallback>,
}

/// 通用 build：在给定工作目录执行 `argv`，产 `artifact_rel`。
///
/// 复用 `run_command_to_log`（日志管道 + 超时 + 进程组 kill）。**并发控制（项目互斥 + 全局
/// permit）由调用方持 `BuildGuard` 保证** —— 本函数只负责跑命令 + 校验产物,不再自取锁,
/// 保持为通用原语:既可作 workspace 打包的子项目构建(workspace 最外层持【一个】guard 跨
/// 整个构建周期,避免子项目间隙释放锁导致同 app_id 构建穿插、首个构建中途 409 失败,#13),
/// 也可作任意单项目构建(调用方自己 `try_start` 取一个 guard 传入)。
///
/// `guard` 以引用传入:仅用于在类型层面证明调用方已持有并发 guard(其借用在本次调用期间
/// 有效,阻止 guard 被提前释放),函数体内不直接使用。
///
/// # 工具链盲区
/// agent-runner 镜像当前缺 **Go** 和 **Gradle**（`Dockerfile.base` 未装）。这两类项目
/// 需先在镜像补装，或在 `cmd` 里自行 `curl` 拉取 —— 否则 build 会失败。
pub async fn build_generic(
    req: &GenericBuildRequest<'_>,
    _guard: &BuildGuard<'_>,
) -> AppResult<PathBuf> {
    let (program, args) = req
        .argv
        .split_first()
        .ok_or_else(|| AppError::validation("build command must contain at least one argv item"))?;
    tokio::fs::create_dir_all(req.log_dir)
        .await
        .map_err(|e| AppError::system(format!("create build log dir: {e}")))?;
    let now = now_ms();
    let main_log = req.log_dir.join(main_log_name());
    let temp_log = req.log_dir.join(temp_log_name(now));

    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_command_to_log(
        program,
        &args,
        req.cwd,
        &main_log,
        &temp_log,
        req.timeout_secs,
        crate::service::dev_server::process::CommandObservers {
            on_pid: req.on_pid,
            on_line: req.on_line.clone(),
        },
    )
    .await?;

    // 校验产物
    let artifact = req.cwd.join(req.artifact_rel);
    if !artifact.exists() {
        return Err(AppError::business(format!(
            "build produced no artifact: {} (cwd={})",
            req.artifact_rel,
            req.cwd.display()
        )));
    }
    Ok(artifact)
}
