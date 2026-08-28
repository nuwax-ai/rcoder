//! Docker 侧 UserApp Deployment 运行时（从 docker_runtime.rs 拆出）。
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

#[async_trait]
impl UserAppDeploymentRuntime for DockerRuntime {
    async fn create_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.create_deployment_impl(params).await
    }

    async fn patch_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.patch_deployment_impl(params).await
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        self.scale_deployment_impl(app_id, replicas).await
    }

    async fn patch_recycle_policy(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        self.patch_recycle_policy_impl(app_id, recycle_enabled, idle_timeout_seconds)
            .await
    }

    async fn restart_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        self.restart_deployment_impl(app_id).await
    }

    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        self.delete_deployment_impl(app_id).await
    }

    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        let inspect = match client.inspect_container(&name, None).await {
            Ok(i) => i,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                return Ok(None);
            }
            Err(e) => {
                return Err(ContainerRuntimeError::ConnectionError(format!(
                    "inspect: {e}"
                )));
            }
        };
        let running = inspect
            .state
            .as_ref()
            .and_then(|s| s.running)
            .unwrap_or(false);
        let ip = extract_container_ip(&inspect, None);
        // 提前借用 inspect 提取 ports（避免下方 inspect.state 消费后借用冲突）
        let ports = extract_container_ports(&inspect);
        let rp = self.recycle_policy_of(app_id);
        Ok(Some(DeploymentStatus {
            app_id: app_id.to_string(),
            replicas: if running { 1 } else { 0 },
            ready_replicas: if running { 1 } else { 0 },
            phase: if running { "Running" } else { "Stopped" }.to_string(),
            message: None,
            pod_ip: if ip.is_empty() { None } else { Some(ip) },
            node: None,
            restart_count: inspect.restart_count.unwrap_or(0) as u32,
            started_at: inspect.state.as_ref().and_then(|s| s.started_at.clone()),
            ports,
            resource_version: None,
            recycle_enabled: rp.recycle_enabled,
            idle_timeout_seconds: rp.idle_timeout_seconds,
            ..Default::default()
        }))
    }

    /// 读 app 当前容器的 desired 快照（update 部分更新回退用，见 trait 注释）。
    /// Docker：command = `Config.cmd`，env = `Config.env`（`K=V` 数组）；
    /// resources 从 inspect HostConfig 换算（NanoCpus→核数、字节→Quantity）。
    /// secrets/health_check 恒 None：Docker create 时 env+secrets **合并**进容器 env
    /// （不可分），而 Docker 无探针概念——env 回退已含 secrets 值，容器行为不丢。
    /// 容器不存在 → 空快照。
    async fn get_app_container_spec(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        let inspect = match client.inspect_container(&name, None).await {
            Ok(i) => i,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(ContainerSpecSnapshot::default()),
            Err(e) => {
                return Err(ContainerRuntimeError::ConnectionError(format!(
                    "inspect for container spec: {e}"
                )));
            }
        };
        let cfg = inspect.config.as_ref();
        let labels = cfg.and_then(|c| c.labels.as_ref());
        // command/ports：从元数据 label 读回（create 时写入，见 create_deployment 内
        // 注释）。label 缺失 = 本版本之前创建的存量容器 → None（部分更新缺省会清空
        // 对应字段，过渡态；重建容器后 label 补齐）。command 不从 Config.cmd 读回：
        // 它无法区分"用户显式设置"与"镜像 CMD 固化"（create 未指定时 Docker 把镜像
        // CMD 写进容器 Config），读回会把旧镜像 CMD 钉死到换镜像后的新容器。
        let command = labels
            .and_then(|l| l.get(APP_COMMAND_LABEL))
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .filter(|c| !c.is_empty());
        let env = cfg
            .and_then(|c| c.env.clone())
            .map(|envs| {
                envs.into_iter()
                    .filter_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        Some((k.to_string(), v.to_string()))
                    })
                    .collect::<HashMap<String, String>>()
            })
            .filter(|m| !m.is_empty());
        // ports：label 编码还原（name 空串、strip_prefix None——Docker 单机模式这两项
        // 无运行时语义；expose_type 精确保留 Http/Tcp 区分）。
        let ports = labels
            .and_then(|l| l.get(APP_PORTS_LABEL).map(String::as_str))
            .map(parse_ports_label)
            .filter(|ps| !ps.is_empty());
        let resources = inspect
            .host_config
            .as_ref()
            .map(|hc| container_runtime_api::AppResourceRequirements {
                cpu: hc.nano_cpus.map(docker_cpus_to_quantity),
                memory: hc.memory.map(docker_memory_to_quantity),
                storage: None,
                ephemeral_storage: None,
            })
            .filter(|r| r.cpu.is_some() || r.memory.is_some());
        Ok(ContainerSpecSnapshot {
            command,
            env,
            secrets: None,
            resources,
            health_check: None,
            ports,
        })
    }

    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        // Docker 模式对账：按 label managed-by=rcoder-app-manager list 容器（含 stopped），
        // 从 ContainerSummary 组装 DeploymentStatus。供 /apps/runtime 与 query_storage 的
        // is_orphan 判定（无此实现则 Docker 模式所有 app 被误判 orphan）。
        use bollard::models::ContainerSummaryStateEnum;
        use bollard::query_parameters::ListContainersOptionsBuilder;
        let client = self.inner.get_docker_client();
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec!["managed-by=rcoder-app-manager".to_string()],
        );
        let opts = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let summaries = client
            .list_containers(Some(opts))
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(format!("list containers: {e}")))?;
        let mut out = Vec::with_capacity(summaries.len());
        for s in summaries {
            let Some(labels) = &s.labels else { continue };
            let Some(app_id) = labels.get("app-id").cloned() else {
                continue;
            };
            let running = s.state == Some(ContainerSummaryStateEnum::RUNNING);
            let ports: Vec<AppPortStatus> = s
                .ports
                .as_ref()
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| {
                            let ext = p.public_port?;
                            Some(AppPortStatus {
                                name: String::new(),
                                port: p.private_port,
                                expose_type: ExposeType::Tcp,
                                external_port: Some(ext),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let rp = self.recycle_policy_of(&app_id);
            out.push(DeploymentStatus {
                app_id,
                replicas: if running { 1 } else { 0 },
                ready_replicas: if running { 1 } else { 0 },
                phase: if running { "Running" } else { "Stopped" }.to_string(),
                message: None,
                pod_ip: None,
                node: None,
                restart_count: 0,
                started_at: None,
                ports,
                resource_version: None,
                recycle_enabled: rp.recycle_enabled,
                idle_timeout_seconds: rp.idle_timeout_seconds,
                ..Default::default()
            });
        }
        Ok(out)
    }

    async fn get_app_logs(
        &self,
        app_id: &str,
        tail: u32,
        timestamps: bool,
    ) -> ContainerRuntimeResult<Vec<ContainerLogEntry>> {
        use bollard::container::LogOutput;
        use bollard::query_parameters::LogsOptions;
        use futures_util::StreamExt;

        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        let opts = LogsOptions {
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            timestamps,
            ..Default::default()
        };
        let mut stream = client.logs(&name, Some(opts));
        let mut out: Vec<ContainerLogEntry> = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(log) => {
                    // 按 bollard LogOutput 变体区分 stdout/stderr（StdIn/Console 归 stdout）
                    let stream_name = match &log {
                        LogOutput::StdErr { .. } => "stderr",
                        _ => "stdout",
                    };
                    let bytes = log.into_bytes();
                    let text = String::from_utf8_lossy(&bytes);
                    for line in text.lines() {
                        let (ts, msg) =
                            container_runtime_api::split_log_timestamp(line, timestamps);
                        out.push(ContainerLogEntry {
                            timestamp: ts,
                            stream: stream_name.to_string(),
                            message: msg,
                        });
                    }
                }
                // 容器不存在（已删）→ 空日志，与 get_deployment_status 的 Ok(None) 语义对齐
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(vec![]),
                Err(e) => {
                    return Err(ContainerRuntimeError::DockerError(format!("logs: {e}")));
                }
            }
        }
        Ok(out)
    }

    /// 在 app 容器内执行命令(docker exec):create_exec → start_exec(读 LogOutput)→ inspect_exec(exit code)。
    /// 用于数据库管理(reset-password / create-database 跑 psql)等场景。
    async fn exec(
        &self,
        app_id: &str,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        use bollard::container::LogOutput;
        use bollard::exec::{CreateExecOptions, StartExecResults};
        use futures_util::StreamExt;

        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();

        // 1. create exec(容器不存在 → ContainerNotFound,与 get_deployment_status 404 处理一致)
        let exec = client
            .create_exec(
                &name,
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
                } => ContainerRuntimeError::ContainerNotFound(name.clone()),
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
        let exit_code = inspect.exit_code.unwrap_or(-1);

        Ok(container_runtime_api::ExecResult {
            stdout,
            stderr,
            exit_code,
        })
    }

    async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> ContainerRuntimeResult<container_runtime_api::mpsc::Receiver<ContainerLogEntry>> {
        use bollard::container::LogOutput;
        use bollard::query_parameters::LogsOptions;
        use futures_util::StreamExt;

        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        let app_id = app_id.to_string();
        let timestamps = true;
        let opts = LogsOptions {
            stdout: true,
            stderr: true,
            tail: if tail > 0 {
                tail.to_string()
            } else {
                "all".to_string()
            },
            follow: true,
            timestamps,
            ..Default::default()
        };
        let mut stream = client.logs(&name, Some(opts));
        let (tx, rx) = container_runtime_api::mpsc::channel::<ContainerLogEntry>(64);
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(log) => {
                        let stream_name = match &log {
                            LogOutput::StdErr { .. } => "stderr",
                            _ => "stdout",
                        };
                        let bytes = log.into_bytes();
                        let text = String::from_utf8_lossy(&bytes);
                        for line in text.lines() {
                            let (ts, msg) =
                                container_runtime_api::split_log_timestamp(line, timestamps);
                            let entry = ContainerLogEntry {
                                timestamp: ts,
                                stream: stream_name.to_string(),
                                message: msg,
                            };
                            if tx.send(entry).await.is_err() {
                                return; // 客户端断开，receiver 已 drop
                            }
                        }
                    }
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => {
                        tracing::warn!("[DOCKER-APP] log stream 容器不存在: {app_id}");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("[DOCKER-APP] log stream 读失败 (终止): {e}");
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }
}
