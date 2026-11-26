//! 可回收的日志根：哪些 bind 源落在它下面。
//!
//! 日志清理器回收**可回收的日志根**（`DockerManagerConfig::log_reclaim_root`，和清理器的
//! `cleanup_config.log_cleanup.log_dir` 是同一个目录）下面过期且没有被挂载的目录。所以落在它
//! 下面的 bind 源，不管容器内的挂载点叫什么，都必须是为本容器独占新建的目录：绑定一个已有的
//! 目录，就可能和回收撞上。创建容器和 builder 重启都按这条规则判定，看的是源路径。

use std::path::{Path, PathBuf};

/// 可回收的日志根在宿主机上的位置
pub(crate) struct LogDomain(Option<PathBuf>);

impl LogDomain {
    pub(crate) fn new(host_root: PathBuf) -> Self {
        Self(Some(crate::normalize_path(&host_root)))
    }

    /// 换不出日志根的宿主机路径：没有 bind 能指向它，也就没有要管的
    pub(crate) fn none() -> Self {
        Self(None)
    }

    /// 宿主机路径是否落在日志根**之下**。日志根本身不算：清理器回收的是它里面的目录
    pub(crate) fn contains(&self, host_path: &str) -> bool {
        self.0.as_ref().is_some_and(|root| {
            Path::new(host_path)
                .strip_prefix(root)
                .is_ok_and(|under| !under.as_os_str().is_empty())
        })
    }
}
