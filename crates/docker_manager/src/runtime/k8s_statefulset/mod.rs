//! agent-runner StatefulSet 操作（K8s 原生 pod 级自愈）。
//!
//! agent-runner 由裸 Pod 改为 per-identifier StatefulSet（replicas 1）：
//! - pod 被 evict/删除/节点挂 → StatefulSet 控制器自动重建同名 pod（挂回同 PVC，数据不丢）；
//! - 容器级 OOM 仍由 restartPolicy=Always 原地重启（pod 模板继承）；
//! - stop/destroy = 删 STS + ClusterIP/headless svc（保留 PVC；下次 ensure 重建 STS 挂回同 PVC）。
//!
//! 仅 ComputerAgentRunner / WebAgentRunner 走此路径；Userapp 仍用 Deployment（create_deployment）。

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    EnvVar, PodSpec, PodTemplateSpec, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use tracing::{debug, info, warn};

use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use shared_types::ServiceType;

use crate::runtime::k8s_pod::K8sPodOps;
use crate::runtime::k8s_service::build_standard_labels;

use super::KubernetesRuntime;

/// rcoder.io/service-type label key（与 build_standard_labels 写入的一致，用于 STS 重名时类型校验）
const SERVICE_TYPE_LABEL: &str = "rcoder.io/service-type";

/// rcoder.io/template-hash 注解 key：创建时记录期望 PodSpec 的指纹，
/// ensure 时对比感知模板漂移（镜像/env/command/sidecar/资源等全部内容）。
pub(crate) const TEMPLATE_HASH_ANNOTATION: &str = "rcoder.io/template-hash";

// 拆分（file-server 大文件范式）：`builder_sts` builder STS ensure/heal impl
// 块；`agent_sts` agent STS/命名/无头服务 impl 块（两块由原单一 impl 拆开，
// 跨块调用的 build_agent_statefulset/scale_captured_statefulset 升
// pub(super)）；`helpers` 模板指纹/校验自由函数（validate_agent_statefulset
// 自原 1268-1328 区并入）。helpers 经 pub(super) + 私有 glob 供子模块互见。

#[cfg(all(test, feature = "kubernetes"))]
mod agent_statefulset_winner_tests;
mod agent_sts;
mod builder_sts;
pub(crate) mod helpers;
#[cfg(all(test, feature = "kubernetes"))]
mod tests;

use helpers::*;
