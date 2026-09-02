//! 容器状态检查器
//!
//! 定期查询 Agent Runner 的容器状态，如果容器有活跃任务则更新活动时间。
//! 这样可以防止正在执行长时间任务的容器被清理任务误判为闲置而销毁。
//!
//! 注意：本模块由 binary (main.rs) 使用，lib 内部不直接调用，因此整体
//! 抑制 dead_code 警告。
//!
//! 拆分（对齐文件尺寸惯例）：
//! - [`checker`]: ContainerStatusChecker 主体（周期检查流程/容器存在性分派/启动入口）
//! - [`state`]: 健康状态机（ContainerHealthState/Config + 失败计数升降级/skip 窗口/过期清理）

#![allow(dead_code)]

mod checker;
mod state;

pub use checker::start_container_status_checker;
pub use state::ContainerStatusCheckerConfig;

#[cfg(test)]
mod tests;
