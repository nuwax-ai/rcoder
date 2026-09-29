//! Builder-only deletion by immutable Docker container ID.
use super::docker_runtime::DockerRuntime;
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::{AppResourceIdentity, AppResourceKind, BuilderDeletionSnapshot, ServiceType};

pub(super) fn builder_identity(
    info: bollard::models::ContainerInspectResponse,
    name: &str,
    app_id: &str,
) -> Result<AppResourceIdentity> {
    let labels = info
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or_else(|| Error::Conflict("builder container has no ownership labels".into()))?;
    if labels.get("service-type").map(String::as_str)
        != Some(ServiceType::UserappBuilder.to_string().as_str())
        || labels.get("identifier").map(String::as_str) != Some(app_id)
    {
        return Err(Error::Conflict(
            "builder container ownership mismatch".into(),
        ));
    }
    let uid = info
        .id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::DockerError("builder container has no physical ID".into()))?;
    Ok(AppResourceIdentity {
        kind: AppResourceKind::Container,
        name: name.to_owned(),
        uid,
        resource_version: None,
    })
}

// 拆分（file-server 大文件范式）：`ops` application_lease_root + DockerRuntime
// 的 builder 删除/文件租约 impl 块（pub(super)→pub(crate) 供 runtime 兄弟
// 模块经类型调用）；`endpoint` workspace endpoint 提取自由函数（cfg(test)
// 垫片随块）；`file_lease` flock 收据原语（unix/not(unix) 成对迁移，pub(super)
// 供 ops 与测试互见）。`builder_identity` 留在 mod.rs：外部
// docker_builder_control 经 `docker_builder_deletion::builder_identity` 路径
// 引用，pub(super) 可见性与路径均不变。

mod compute_lease;
mod endpoint;
mod file_lease;
mod ops;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod workspace_endpoint_tests;

use endpoint::*;
use file_lease::*;
