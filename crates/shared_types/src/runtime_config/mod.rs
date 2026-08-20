//! 部署配置域 —— Docker / K8s 双运行时配置族 + K8s Quantity 解析
//!
//! - `service`：服务镜像配置（镜像 / 环境变量 / 挂载点 / 资源限制，Docker 侧）
//! - `multi_image`：多镜像配置（全局默认 / 服务覆盖 / 选择策略 / 缓存）
//! - `k8s`：K8s 运行时专用配置（PVC / emptyDir 卷、sidecar，与 docker_config 分家）
//! - `quantity`：K8s Quantity 解析工具（内存/存储/CPU，winnow 实现）
//!
//! 消费方：docker_manager（运行时构建）与 rcoder config 加载；
//! K8s 构建器只读 `k8s`，docker 运行时只读 `service`/`multi_image`，互不污染。
//!
//! 对外统一经 crate 根部 re-export 暴露（如 `shared_types::KubernetesConfig`），
//! 下游不应依赖 `shared_types::runtime_config::` 路径。

pub mod k8s;
pub mod multi_image;
pub mod quantity;
pub mod service;
