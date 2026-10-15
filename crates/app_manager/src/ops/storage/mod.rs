//! 持久存储管理（query/clear/destroy storage + orphan 检测，`app_stage` 显式分派 dev/prod）
//!
//! RBD 卷形态（rcoder 零挂载）：`exists` = PVC 归属（K8s label 集）/目录存在
//! （Docker）；`path` = PVC 名（K8s，非可挂载路径）/bind 源目录（Docker）；
//! `modified_at` 仅 Docker 可得（K8s 需容器运行，降级 None）。
//! - `prod`：`clear` K8s = 删 PVC（数据清空语义等价——RBD 不可挂载无法逐文件清，
//!   卷在下次 create 自动重建）；Docker = 清目录内容。
//! - `dev`：`clear` = 经容器 file-server 清空 workspace 内容（留容器留卷——
//!   开发容器常驻，"重置开发工作区"语义）；`destroy` = UserappDevCleanup 四步
//!   回收整个开发环境（容器+PVC+目录+注册），不动 metadata。

/// deploy-host：userapp 根经容器名键映射出口（与引擎 workspace_root_path 同语义，
/// app_manager 不依赖 rcoder-engine，就地实现）。
#[cfg(feature = "deploy-host")]
fn app_manager_utils_host_root() -> std::path::PathBuf {
    // 直接读 host_map 同源 env/默认表（避免跨 crate 依赖）：默认 ~/.rcoder
    let home = std::env::var("HOME").unwrap_or_default();
    std::env::var("RCODER_OPERATION_LOCK_ROOT")
        .unwrap_or_else(|_| format!("{home}/.rcoder/workspace/userapp"))
        .into()
}

use tracing::{info, warn};

use shared_types::ServiceType;
use shared_types::UserappStage;

use crate::models::*;
use crate::utils::*;

// 拆分（file-server 大文件范式）：`inspect` 查询/孤儿判定与目录锚点 /
// `clear` 清空存储（StorageClearLeases）/ `destroy` 销毁 + purge + dev 回收
// 捕获；测试独立 `tests`。`crate::ops::StorageClearLeases` 旧路径经重导出
// 保持不变；跨块互访的 impl 私有方法按原语义升 pub(super)。

mod clear;
mod destroy;
mod inspect;
#[cfg(test)]
mod tests;

pub(crate) use clear::*;
