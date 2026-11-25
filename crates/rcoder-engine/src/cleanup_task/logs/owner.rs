//! 挂载中的宿主机路径清单。
//!
//! 清理器在删除一个过期的日志目录之前，必须先确认没有容器还在把它当 bind mount 源。
//! 检查由调用方注入：生产环境走运行时的清单（[`RuntimeMountedPaths`]，一次列表调用），
//! 测试用假实现。

use async_trait::async_trait;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

/// 当前被容器挂载的宿主机路径清单。
#[async_trait]
pub trait MountedPathsProbe: Send + Sync {
    /// 当前被任意容器挂载为 bind 源的宿主机路径（含已停止但尚未删除的容器）。
    ///
    /// `Err` 表示无法确认，调用方按"仍在用"处理，不删除任何目录。
    async fn mounted_host_paths(&self) -> Result<HashSet<PathBuf>, String>;
}

/// 生产实现：经运行时的权威清单判断哪些宿主机路径还被容器挂着。
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
    async fn mounted_host_paths(&self) -> Result<HashSet<PathBuf>, String> {
        self.runtime
            .mounted_host_paths()
            .await
            .map_err(|error| error.to_string())
    }
}
