//! Docker 侧 Userapp Deployment 运行时（从 docker_runtime.rs 拆出）。
//!
//! `UserAppDeploymentRuntime` 的 trait 壳：**变更组**（create/patch/scale/
//! recycle/restart/delete）一行委托 docker_app_create.rs 的自有 impl；
//! **观测组**（status/spec/list/logs/exec/stream）在本文件。与 K8s 侧
//! k8s_app_*.rs 文件群对称；工具函数在 docker_runtime.rs（pub(crate) 共享）。

use async_trait::async_trait;
use container_runtime_api::{
    AppPortStatus, ContainerCreateParams, ContainerLogEntry, ContainerRuntimeError,
    ContainerRuntimeResult, ContainerSpecSnapshot, DeploymentStatus, ExposeType,
    UserAppDeploymentRuntime,
};
use shared_types::ContainerBasicInfo;
use std::collections::HashMap;

use super::docker_runtime::DockerRuntime;
use super::docker_runtime::{
    APP_COMMAND_LABEL, APP_PORTS_LABEL, app_deployment_name, docker_cpus_to_quantity,
    docker_memory_to_quantity, extract_container_ip, extract_container_ports, parse_ports_label,
};

/// Docker reports published ports as TCP regardless of their application
/// protocol. The persisted app port label determines which ports use Pingora.
fn merge_http_port_labels(ports: &mut Vec<AppPortStatus>, raw: &str) {
    for port in parse_ports_label(raw) {
        if port.expose_type != ExposeType::Http {
            continue;
        }
        if let Some(existing) = ports.iter_mut().find(|existing| existing.port == port.port) {
            existing.expose_type = ExposeType::Http;
        } else {
            ports.push(AppPortStatus {
                name: format!("http-{}", port.port),
                port: port.port,
                expose_type: ExposeType::Http,
                external_port: None,
            });
        }
    }
}

pub(crate) async fn execute_container_command(
    client: &bollard::Docker,
    name: &str,
    command: Vec<String>,
) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
    use bollard::container::LogOutput;
    use bollard::exec::{CreateExecOptions, StartExecResults};
    use futures_util::StreamExt;

    // 1. create exec(容器不存在 → ContainerNotFound,与 get_deployment_status 404 处理一致)
    let exec = client
        .create_exec(
            name,
            CreateExecOptions {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(command),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => ContainerRuntimeError::ContainerNotFound(name.to_owned()),
            _ => ContainerRuntimeError::ContainerExecError(format!("create_exec: {e}")),
        })?;

    // 2. start exec + 读输出流(LogOutput 分桶 stdout/stderr,同 get_app_logs)
    let mut stdout = String::new();
    let mut stderr = String::new();
    match client
        .start_exec(&exec.id, None)
        .await
        .map_err(|e| ContainerRuntimeError::ContainerExecError(format!("start_exec: {e}")))?
    {
        StartExecResults::Attached { mut output, .. } => {
            while let Some(item) = output.next().await {
                match item {
                    Ok(LogOutput::StdOut { message }) | Ok(LogOutput::Console { message }) => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    Ok(LogOutput::StdErr { message }) => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        return Err(ContainerRuntimeError::ContainerExecError(format!(
                            "stream: {e}"
                        )));
                    }
                }
            }
        }
        StartExecResults::Detached => {
            return Err(ContainerRuntimeError::ContainerExecError(
                "unexpected Detached".into(),
            ));
        }
    }

    // 3. exit code(stream 结束后 inspect 单独取)
    let inspect = client
        .inspect_exec(&exec.id)
        .await
        .map_err(|e| ContainerRuntimeError::ContainerExecError(format!("inspect_exec: {e}")))?;
    if inspect.running != Some(false) {
        return Err(ContainerRuntimeError::ContainerExecError(
            "Exec has no confirmed stopped state; outcome is unknown".into(),
        ));
    }
    let exit_code = inspect.exit_code.filter(|code| *code >= 0).ok_or_else(|| {
        ContainerRuntimeError::ContainerExecError(
            "Exec has no exit code; outcome is unknown".into(),
        )
    })?;

    Ok(container_runtime_api::ExecResult {
        stdout,
        stderr,
        exit_code,
    })
}

fn validate_configuration_exec_target(
    context: &shared_types::UserAppExecutionContext,
    target: &shared_types::RuntimeConfigurationTarget,
    container: &bollard::models::ContainerInspectResponse,
) -> ContainerRuntimeResult<()> {
    let mismatch =
        || ContainerRuntimeError::Conflict("Configuration exec target identity changed".into());
    if container.id.as_deref() != Some(target.physical_uid.as_str()) {
        return Err(mismatch());
    }
    let config = container.config.as_ref().ok_or_else(mismatch)?;
    let labels = config.labels.as_ref().ok_or_else(mismatch)?;
    if labels.get("managed-by").map(String::as_str) != Some("rcoder-app-manager")
        || labels.get("service-type").map(String::as_str)
            != Some(shared_types::ServiceType::Userapp.to_string().as_str())
        || labels.get(shared_types::USERAPP_DOCKER_APP_ID_LABEL) != Some(&context.app_id)
    {
        return Err(mismatch());
    }
    context
        .validate_application_metadata(
            &labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        )
        .map_err(ContainerRuntimeError::Conflict)?;
    let expected = format!(
        "{}={}",
        shared_types::APP_DEPLOY_GENERATION_ID,
        target.deployment_generation
    );
    let generations: Vec<_> = config
        .env
        .iter()
        .flatten()
        .filter(|entry| {
            entry
                .split_once('=')
                .is_some_and(|(key, _)| key == shared_types::APP_DEPLOY_GENERATION_ID)
        })
        .collect();
    if generations.len() != 1 || generations[0] != &expected {
        return Err(mismatch());
    }
    Ok(())
}

// 拆分（file-server 大文件范式）：`deployment` `UserAppDeploymentRuntime`
// trait impl（trait impl 不可跨文件拆分，整块迁移；方法无可见性修饰符）。
// 三个共享自由函数留在 mod.rs 保持原私有/pub(crate) 可见性——
// `execute_container_command` 被 native_domain/docker_readiness/
// docker_builder_control/docker_runtime 经 `docker_app_runtime::` 路径引用，
// 留在 mod.rs 使路径不变。deployment 块内 super:: 兄弟路径已改写为
// crate::runtime::。

#[cfg(test)]
mod configuration_exec_tests;
mod deployment;
