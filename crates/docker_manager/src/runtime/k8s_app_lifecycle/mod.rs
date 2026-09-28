//! Userapp Deployment 生命周期(从 k8s_deployment.rs 拆出)。
//!
//! Scale/restart and removal of obsolete port resources. Identity-bound deletion
//! lives in k8s_app_deletion.

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ExposeType,
};
#[cfg(feature = "kubernetes")]
use kube::api::{Patch, PatchParams};
#[cfg(feature = "kubernetes")]
use tracing::info;

#[cfg(feature = "kubernetes")]
use super::k8s_app_helpers::{
    IDLE_TIMEOUT_ANNOTATION, RECYCLE_ENABLED_ANNOTATION, WAKE_ON_TRAFFIC_ANNOTATION,
};
#[cfg(feature = "kubernetes")]
use super::k8s_deployment::APP_CONTAINER_NAME;

use super::kubernetes_runtime::KubernetesRuntime;

// 拆分（file-server 大文件范式）：原单一 impl 按功能域拆两块——`captured`
// captured 生命周期方法组（capture/start/stop/reconcile，pub(super)→pub(crate)
// 供 runtime 兄弟模块经类型调用）；`app_ops` 公开 app 方法组（scale/restart/
// patch/cleanup，patch_captured_app 被兄弟块复用升 pub(crate)）。`patches`
// 变更 patch 构造自由函数（pub(super) 供两 impl 块与测试互见）。

mod app_ops;
mod captured;
#[cfg(test)]
mod mutation_identity_tests;
mod patches;

use patches::*;
