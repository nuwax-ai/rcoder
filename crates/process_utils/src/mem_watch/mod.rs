//! Agent 命令树内存看门狗。
//!
//! 目标：cgroup 内存到达阈值时，杀掉 agent 命令树里最大的子进程，
//! 避免整个容器被 OOM 杀掉（会话消失）。设计见 `docs/agent-mem-watch.md`。
//!
//! 模块划分：
//! - [`proc_tree`]：`/proc` 解析、后代遍历、候选选择（纯函数）
//! - [`cgroup`]：working set 计算（纯函数）
//! - [`config`]：参数与模式
//! - [`switch`]：文件开关（热更新）
//! - [`registry`]：会话根进程与"启动窗口"保护集
//! - [`kill`]：pidfd 终止
//! - [`sampler`]：采样循环

pub mod cgroup;
pub mod config;
pub mod kill;
pub mod proc_tree;
pub mod registry;
pub mod sampler;
mod switch;
mod sys;

pub use config::{Config, Mode};
pub use proc_tree::ProcInfo;

/// 当前进程表快照（`pid -> ProcInfo`）。非 Linux 返回空表。
///
/// 供其它模块复用，避免各自再写一份 `/proc` 解析。
pub fn snapshot() -> std::collections::HashMap<u32, ProcInfo> {
    sys::read_all()
}
pub use registry::{
    AGENT_RUNTIME_MARKERS, is_agent_runtime, mark_session_active, try_register, try_unregister,
};
pub use sampler::{evictable_pids, run, start};

/// 安装全局注册表并启动采样任务。
///
/// 即使 `mode=off` 也要安装：文件开关可以在运行期把模式打开，
/// 而注册表必须在那之前就存在。关闭时采样循环只做一次 `mode` 判断。
pub fn install_and_start() {
    registry::install();
    let handle = start(Config::from_env());
    // 监督任务：panic 不能静默——否则"看门狗失效"没有任何信号。
    tokio::spawn(async move {
        if let Err(err) = handle.await {
            tracing::error!(event = "mem_watch.task_panicked", error = %err);
        }
    });
}
