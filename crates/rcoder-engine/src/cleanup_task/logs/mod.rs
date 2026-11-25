//! 日志清理模块
//!
//! 负责清理 `/app/logs/container` 目录下的过期日志文件和过期目录。
//!
//! # 为什么不能只看修改时间
//!
//! 该目录下的子目录是 RCoder 为每个容器创建的日志目录，同时也是该容器内
//! `/app/container-logs` 的 bind mount 源。容器还在时删掉它，容器里的这个路径就失效了
//! （`ls` 为空、新建文件报 `ENOENT`），依赖它的 agent 会启动失败。而目录的修改时间只
//! 反映顶层最后一次增删条目——一个长期运行、顶层不再变化的容器，其日志目录会一直
//! "看起来已过期"。
//!
//! 因此删除判据是"已过期 **且** 当前没有被任何容器挂载"，拿不到挂载清单时一律保留并
//! 告警。完整规则见 [`cleaner`] 的模块说明。
//!
//! # 运行时接口的调用频率
//!
//! 清理任务每轮（`cleanup_interval_seconds`，默认 5 分钟）都会扫一遍目录，但只有在
//! **真的出现过期目录时**才会向运行要一次挂载清单，且每轮最多一次——没有过期目录就
//! 一次接口都不调。清单获取失败则本轮不再删任何目录，也不重试。
//!
//! K8s 部署下 rcoder 所在的 Pod 没有这个目录，清理在访问运行时之前就已跳过。

mod cleaner;
mod owner;

#[cfg(test)]
mod physical_docker;
#[cfg(test)]
mod tests;

pub use cleaner::{LogCleaner, LogCleanupStats};
pub use owner::{MountedPathsProbe, RuntimeMountedPaths};
