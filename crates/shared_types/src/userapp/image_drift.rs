//! UserApp 工作负载镜像漂移治理的共享契约（spec sandbox-image-version-drift）。
//!
//! R3 只读台账的数据行与 R4 观察兜底的配置；判定与收敛分属引擎扫描器和
//! 各运行时实现，这里只承载跨 crate 的稳定形状。

use serde::{Deserialize, Serialize};

/// 一个受管 UserApp 工作负载的模板镜像快照（只读观察，不含判定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppWorkloadImageInfo {
    pub app_id: String,
    /// `dev`（builder StatefulSet/容器）或 `prod`（app Deployment/容器）。
    pub stage: String,
    /// 控制器/容器名（`rcoder-app-<id>` / `rcoder-app-builder-<id>`）。
    pub workload_name: String,
    /// 镜像引用原样（模板 `containers[].image` / Docker `Config.Image`）。
    pub image: String,
}

/// 镜像漂移观察兜底配置（R4；K8s 专有，Docker 豁免）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppImageDriftWatchConfig {
    /// 命名空间（观察范围）。
    pub namespace: String,
    /// 平台 prod 镜像引用（app-runtime；来自部署 env）。
    pub platform_image: String,
    /// 拉取失败持续判定窗口（秒）：Pod 处于 ImagePullBackOff/ErrImagePull
    /// 且跨越该窗口才触发收敛，过滤首启抖动。默认 120。
    pub pull_failure_window_seconds: u64,
    /// 同一 app 两次收敛尝试的最小间隔（秒；去重冷却）。默认 3600。
    pub recovery_cooldown_seconds: u64,
}

impl Default for UserAppImageDriftWatchConfig {
    fn default() -> Self {
        Self {
            namespace: String::new(),
            platform_image: String::new(),
            pull_failure_window_seconds: 120,
            recovery_cooldown_seconds: 3600,
        }
    }
}
