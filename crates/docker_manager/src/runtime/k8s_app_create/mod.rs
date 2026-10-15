//! Userapp Deployment 创建路径(从 k8s_deployment.rs 拆出)。
//!
//! apply_app_configmap/secret/service/httproute/nodeport/deployment + build_app_deployment +
//! create_app_resources 编排。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    AppPortStatus, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    ExposeType, HttpExpose,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapEnvSource, Container as K8sContainer, ContainerPort, EnvFromSource, EnvVar,
    PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, SecretEnvSource, Volume,
    VolumeMount,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
#[cfg(feature = "kubernetes")]
#[cfg(feature = "kubernetes")]
use tracing::info;

#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;

use super::k8s_app_helpers::{
    build_app_resource_requirements, build_hostname_spread_constraint, build_probe,
    config_hash_annotations, merge_app_annotations,
};
use super::k8s_deployment::{APP_CONTAINER_NAME, APP_NAME_LABEL_VALUE};
#[cfg(feature = "kubernetes")]
use super::k8s_pvc::K8sPvcOps;
use super::kubernetes_runtime::KubernetesRuntime;

// 拆分（file-server 大文件范式）：`create` 应用工作负载创建 impl 块与
// 挂载助手 / `tests` 回归网。旧路径 `k8s_app_create::X` 经 glob 重导出
// 保持不变。

mod create;
#[cfg(test)]
mod tests;

#[cfg(test)]
use create::*;
