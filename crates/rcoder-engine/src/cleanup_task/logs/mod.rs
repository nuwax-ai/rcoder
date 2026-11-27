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
//! 因此删除判据是"已过期 **且** 当前没有被任何容器挂载"，无法确认时一律保留并告警。
//! 完整规则见 [`cleaner`] 的模块说明。
//!
//! # 两个容易踩的前提
//!
//! - **路径空间**：清理器手里的 `log_dir` 是 RCoder 进程自己命名空间里的路径，而 Docker
//!   报回的挂载源是宿主机路径，两者不能直接比对；同一个目录还可能有别名。换算与比对都
//!   由运行时负责（`mounted_log_directories`），compose 部署下两侧前缀本就不同。
//! - **日志目录一次性使用**：删除和"建目录 → Docker 登记挂载"之间不互斥，靠的是每个容器
//!   绑定的都是给它新建的目录（创建容器和 builder 重启都是，`docker_manager::log_directory`）。
//!   所以清理器手里的候选都是旧目录，还在用旧目录的容器都在清单里；新建好、还没登记进
//!   Docker 的目录有"正在准备挂载"的登记，清单把它们一并算作仍被挂载。创建结果不明的
//!   （应答丢了、被取消），登记继续保留到过期，不会因为"等得久"就失去保护。
//!
//! # 运行时接口的调用频率
//!
//! 日志清理有自己的周期（`cleanup_config.log_cleanup.cleanup_interval_seconds`，默认
//! 1 天）：任务每隔一个清扫间隔（默认 5 分钟）检查一次是否到期，到期才扫一遍目录——日志
//! 保留期以天计，没必要反复扫描。每次扫描只在**真的出现过期目录时**才会向运行时要一次
//! 挂载清单，且每次最多一次——没有过期目录就一次接口都不调。清单获取失败则这一轮不再删
//! 任何候选目录，下一次检查重试。删除目录有时间和批次预算，没做完的下一次检查继续，
//! 不等待完整周期。任务本身见 [`super::log_cleanup`]。
//!
//! K8s 部署下 rcoder 所在的 Pod 没有这个目录，清理在访问运行时之前就已跳过。
//!
//! `physical_docker` 是显式开启的真实 Docker 见证，运行条件见该模块的说明。

mod cleaner;
mod owner;
mod stats;

#[cfg(test)]
mod physical_docker;
#[cfg(test)]
pub(in crate::cleanup_task) mod testing;
#[cfg(test)]
mod tests;

pub use cleaner::LogCleaner;
pub(in crate::cleanup_task) use cleaner::LogCleanupError;
pub use owner::{MountedPathsProbe, RuntimeMountedPaths};
pub use stats::LogCleanupStats;
