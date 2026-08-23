//! Docker 侧 UserApp Deployment 运行时（从 docker_runtime.rs 拆出）。
//!
//! `UserAppDeploymentRuntime` 完整实现——Docker 无 Deployment 概念，用容器
//! create/stop/start/port_bindings 做等价映射；与 K8s 侧 k8s_app_*.rs 文件群对称。
//! 工具函数（命名/ports label/资源换算/IP 提取）在 docker_runtime.rs（pub(crate) 共享）。

use async_trait::async_trait;
use container_runtime_api::{
    AppPortStatus, ContainerCreateParams, ContainerLogEntry, ContainerRuntimeError,
    ContainerRuntimeResult, ContainerSpecSnapshot, DeploymentStatus, ExposeType,
    UserAppDeploymentRuntime,
};
use shared_types::{ContainerBasicInfo, ServiceType};
use std::collections::HashMap;
use std::time::Duration;

use tracing::info;

use super::docker_runtime::DockerRuntime;

// 底部工具函数在本体（docker_runtime.rs）——pub(crate) 引入
use super::docker_runtime::{
    APP_COMMAND_LABEL, APP_PORTS_LABEL, RecyclePolicy, app_deployment_name,
    docker_cpus_to_quantity, docker_memory_to_quantity, encode_ports_label, extract_container_ip,
    extract_container_ports, parse_ports_label,
};

#[async_trait]
impl UserAppDeploymentRuntime for DockerRuntime {
    // ===== Deployment 生命周期（UserApp 专用，Docker 语义映射）=====
    // Docker 无 Deployment 概念，用容器 create/stop/start 做等价映射。
    // app 容器加入主网络（与 rcoder 同网络），HTTP 端口由 app_manager 通过
    // Pingora backend 注册（container_ip:port），TCP 端口做 port_bindings（自动分配 host port）。
    async fn create_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType, PortBinding};
        use bollard::query_parameters::{CreateContainerOptions, StartContainerOptions};

        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "create_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        let image = params.image_override.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "create_deployment requires image_override".to_string(),
            )
        })?;
        let container_name = app_deployment_name(&app_id);

        // env（env + secrets 合并；Docker 模式无 Secret 概念）
        let mut env_map: HashMap<String, String> = HashMap::new();
        if let Some(e) = &params.env {
            env_map.extend(e.clone());
        }
        if let Some(s) = &params.secrets {
            env_map.extend(s.clone());
        }
        let env_vec: Vec<String> = env_map.iter().map(|(k, v)| format!("{k}={v}")).collect();

        // labels（供对账/list 过滤）
        let mut labels: HashMap<String, String> = HashMap::new();
        labels.insert("managed-by".to_string(), "rcoder-app-manager".to_string());
        labels.insert("app-id".to_string(), app_id.clone());
        labels.insert("service-type".to_string(), ServiceType::UserApp.to_string());
        if let Some(t) = &params.tenant_id {
            labels.insert("tenant".to_string(), t.clone());
        }
        if let Some(s) = &params.space_id {
            labels.insert("space".to_string(), s.clone());
        }
        // ports/command 元数据 label（update live 回退数据源）：Docker 侧 Http 走
        // Pingora 注册、Tcp 走 port_bindings，ExposedPorts 无法完整还原（Http 读不
        // 回、Tcp 被隐式 expose 后类型丢失、镜像 EXPOSE 幽灵端口混入）；command 无法
        // 区分用户显式设置与镜像 CMD 固化（inspect 的 Config.cmd 是合并结果）。两者
        // 用 label 显式持久化（与 K8s port-expose 注解同构），update 回退读回。
        if let Some(ports) = &params.ports
            && !ports.is_empty()
        {
            labels.insert(APP_PORTS_LABEL.to_string(), encode_ports_label(ports));
        }
        if let Some(command) = &params.command
            && !command.is_empty()
            && let Ok(encoded) = serde_json::to_string(command)
        {
            labels.insert(APP_COMMAND_LABEL.to_string(), encoded);
        }

        // TCP port_bindings（host_port=None 让 Docker 自动分配）
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        if let Some(ports) = &params.ports {
            for p in ports.iter().filter(|p| p.expose_type == ExposeType::Tcp) {
                port_bindings.insert(
                    format!("{}/tcp", p.port),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: None,
                    }]),
                );
            }
        }

        // workspace bind mount（host_workspace_path → /app）
        let mounts = if !params.host_workspace_path.is_empty() {
            Some(vec![Mount {
                target: Some("/app".to_string()),
                source: Some(params.host_workspace_path.clone()),
                typ: Some(MountType::BIND),
                ..Default::default()
            }])
        } else {
            None
        };

        // 加入主网络（与 rcoder 同网络，Pingora 才能通过 container_ip 访问）
        // 同时保留网络名，供 start 后按网卡定位 container_ip（多网卡时避免 values().next() 取错）
        let main_network = self.inner.detect_main_network_name().await.ok();
        let network_mode = main_network.clone();

        let host_config = HostConfig {
            mounts,
            port_bindings: if port_bindings.is_empty() {
                None
            } else {
                Some(port_bindings)
            },
            network_mode,
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(image.clone()),
            cmd: params.command.clone(),
            env: if env_vec.is_empty() {
                None
            } else {
                Some(env_vec)
            },
            labels: Some(labels),
            host_config: Some(host_config),
            ..Default::default()
        };

        let client = self.inner.get_docker_client();
        let created = client
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.clone()),
                    platform: String::new(),
                }),
                config,
            )
            .await
            .map_err(|e| {
                // Fail Fast：打印 bollard 原始错误（含 daemon status_code/message），
                // 避免 service 层 context 吞掉根因（见 service.rs create_app 错误链）
                tracing::error!(
                    "[APP-DOCKER] create_container 失败 name={}, image={}: {e:?}",
                    container_name,
                    image
                );
                ContainerRuntimeError::ContainerCreationError(e.to_string())
            })?;
        if let Err(e) = client
            .start_container(&created.id, None::<StartContainerOptions>)
            .await
        {
            tracing::error!(
                "[APP-DOCKER] start_container 失败 name={}, id={}: {e:?}",
                container_name,
                created.id
            );
            // best-effort 强删已 created 的孤儿容器，避免残留导致下次同名创建冲突
            // （对齐 delete_deployment 的 force-remove 范式）
            use bollard::query_parameters::RemoveContainerOptions;
            if let Err(rm_e) = client
                .remove_container(
                    &created.id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                tracing::debug!(
                    "[APP-DOCKER] best-effort remove orphan container {} failed: {}",
                    created.id,
                    rm_e
                );
            }
            return Err(ContainerRuntimeError::ContainerStartError(e.to_string()));
        }

        // 短轮询等待 container_ip 就绪（容器刚 start，IP 可能尚未分配）。
        // 优先取主网络网卡的 IP，回退任意网卡；最多重试 6 次 × 200ms。
        let preferred = main_network.as_deref();
        let ip = {
            let mut ip = String::new();
            for attempt in 0..6u32 {
                match client.inspect_container(&created.id, None).await {
                    Ok(inspect) => {
                        ip = extract_container_ip(&inspect, preferred);
                        if !ip.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[APP-DOCKER] inspect container {} for ip failed (attempt {attempt}): {}",
                            created.id,
                            e
                        );
                    }
                }
                if attempt < 5 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            ip
        };

        if ip.is_empty() {
            tracing::warn!(
                "[APP-DOCKER] container {} started but IP not ready after polling; \
                 Pingora/gRPC 注册前应确认可达，否则会掩盖启动故障",
                created.id
            );
        }
        Ok(ContainerBasicInfo {
            container_id: created.id.clone(),
            container_name,
            container_ip: ip,
            internal_port: 0,
            external_port: 0,
            project_id: app_id,
            status: "Running".to_string(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
        })
    }

    /// 更新 UserApp 容器：Docker 不支持 in-place 改 image/env/command，必须重建。
    /// force-remove 旧容器（best-effort，不存在则忽略）后用新 params 走 create_deployment。
    /// 工作空间目录不在 runtime 层（由 service 层管理），重建不丢数据。
    async fn patch_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        use bollard::query_parameters::RemoveContainerOptions;
        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "patch_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        let name = app_deployment_name(&app_id);
        let client = self.inner.get_docker_client();
        // 旧容器 best-effort 强删（image/env/command 变了必须重建；不存在则忽略错误）
        if let Err(e) = client
            .remove_container(
                &name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            tracing::debug!(
                "[DOCKER] Best-effort remove old container {} failed (may not exist): {}",
                name,
                e
            );
        }
        // 用新 params 重建（复用 create_deployment 全套逻辑：mount/env/labels/ports/start）
        self.create_deployment(params).await
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::{StartContainerOptions, StopContainerOptions};
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        if replicas == 0 {
            client
                .stop_container(
                    &name,
                    Some(StopContainerOptions {
                        t: Some(10),
                        signal: Some(String::new()),
                    }),
                )
                .await
                .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?;
        } else {
            client
                .start_container(&name, None::<StartContainerOptions>)
                .await
                .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;
        }
        Ok(())
    }

    /// Docker 无 K8s 注解,改用内存态存储回收策略(merge 语义:None=不改该字段)。
    async fn patch_recycle_policy(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        // merge 语义统一:Occupied 合并旧值,Vacant 以 default(全 None) 为基底 merge。
        // and_modify/or_insert_with 把两分支的 merge 语义收敛为一处表达。
        self.recycle_policy
            .entry(app_id.to_string())
            .and_modify(|p| *p = p.merge(recycle_enabled, idle_timeout_seconds))
            .or_insert_with(|| {
                RecyclePolicy::default().merge(recycle_enabled, idle_timeout_seconds)
            });
        info!(
            "[DOCKER-APP] recycle policy patched: {app_id} (enabled={:?}, idle_timeout={:?})",
            recycle_enabled, idle_timeout_seconds
        );
        Ok(())
    }

    async fn restart_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::{StartContainerOptions, StopContainerOptions};
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        // best-effort: 容器可能已停止，忽略 stop 失败
        if let Err(e) = client
            .stop_container(
                &name,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: Some(String::new()),
                }),
            )
            .await
        {
            tracing::debug!(
                "[DOCKER] Best-effort stop container {} before restart failed: {}",
                name,
                e
            );
        }
        client
            .start_container(&name, None::<StartContainerOptions>)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;
        Ok(())
    }

    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::RemoveContainerOptions;
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        // 404 容忍 + 真实失败透传（对齐 K8s 侧 k8s_app_lifecycle 契约）：
        // 调用方 delete_app(purge=true) 依赖本步成功才继续 destroy_app_pvc
        //（Docker 语义=删 workspace 目录）——全量吞错会让容器还在运行而
        // bind mount 源目录被删，写入进入孤儿 inode，数据丢失
        match client
            .remove_container(
                &name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(()) => {}
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                tracing::debug!("[DOCKER] delete container {} not found, skip", name);
            }
            Err(e) => {
                // daemon 短暂不可达等瞬态：先查存在性区分（存在但删失败=透传）
                let exists = client
                    .inspect_container(&name, None)
                    .await
                    .map(|_| true)
                    .unwrap_or(false);
                if exists {
                    return Err(ContainerRuntimeError::DockerError(format!(
                        "delete container {name}: {e}"
                    )));
                }
                tracing::debug!(
                    "[DOCKER] delete container {} vanished concurrently ({}), skip",
                    name,
                    e
                );
            }
        }
        // 清理内存态回收策略（K8s 靠注解随 Deployment 自动消失；Docker 需显式清，防孤儿堆积）
        drop(self.recycle_policy.remove(app_id));
        Ok(())
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
