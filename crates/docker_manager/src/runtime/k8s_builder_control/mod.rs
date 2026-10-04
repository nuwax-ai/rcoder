//! UID/version-fenced builder compute controls; PVC is untouched.
use super::{
    k8s_pod::K8sPodOps, k8s_service::K8sServiceOps, kubernetes_runtime::KubernetesRuntime,
};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use k8s_openapi::api::{apps::v1::StatefulSet, core::v1::Pod};
use kube::{
    Api,
    api::{DeleteParams, Patch, PatchParams, Preconditions},
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderPodIdentity,
    ContainerBasicInfo, ServiceType, UserAppExecutionContext,
};
use std::time::Duration;

// 拆分（file-server 大文件范式）：`ops` KubernetesRuntime 的 builder 控制
// impl 块 / `helpers` 判定与补丁构造自由函数。impl 方法经类型本身可达，
// 无跨模块路径引用——helpers 私有 glob 仅供 mod.rs 作用域（子模块经
// `use super::*` 互见）。

mod helpers;
#[cfg(test)]
mod live_tests;
mod ops;
#[cfg(test)]
mod tests;

use helpers::*;
