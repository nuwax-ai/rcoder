//! 日志目录的挂载判定从哪里来。
//!
//! 清理器在删除一个过期的日志目录之前，必须先确认没有容器还在把它当 bind mount 源。
//! 判定由调用方注入：生产环境走运行时（[`RuntimeMountedPaths`]，一次列表调用），测试用
//! 假实现。

use async_trait::async_trait;
use container_runtime_api::LogDirectoryMounts;
use std::path::PathBuf;
use std::sync::Arc;

/// 日志目录的挂载判定。
#[async_trait]
pub trait MountedPathsProbe: Send + Sync {
    /// 逐个候选给出判定：仍被容器挂载、无法确认（必须保留），或两者都不含——确认当前
    /// 没有容器挂它。
    ///
    /// `Err` 只用于整份清单拿不到的情况，调用方同样不删除任何目录。
    async fn mounted_log_directories(
        &self,
        candidates: &[PathBuf],
    ) -> Result<LogDirectoryMounts, String>;
}

/// 生产实现：经运行时的权威清单判断哪些候选目录还被容器挂着。
pub struct RuntimeMountedPaths {
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
}

impl RuntimeMountedPaths {
    pub fn new(runtime: Arc<dyn container_runtime_api::ContainerRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl MountedPathsProbe for RuntimeMountedPaths {
    async fn mounted_log_directories(
        &self,
        candidates: &[PathBuf],
    ) -> Result<LogDirectoryMounts, String> {
        self.runtime
            .mounted_log_directories(candidates)
            .await
            .map_err(|error| error.to_string())
    }
}
