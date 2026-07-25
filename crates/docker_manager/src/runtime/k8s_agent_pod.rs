//! agent-runner 生命周期管理（从 kubernetes_runtime.rs 拆出）。
//!
//! 这组方法是 `impl ContainerRuntime` 里 agent-runner（WebAgentRunner / ComputerAgentRunner）
//! 的容器生命周期实现：创建（走 StatefulSet）、查询、停止、列举、状态同步、全量回收。
//! 与 `k8s_deployment.rs` 的 UserApp（Deployment）路径正交。
//!
//! 模式沿用 k8s_statefulset.rs：本文件用 inherent 方法（`create_agent_container` /
//! `find_container_inner` 等），kubernetes_runtime.rs 的 `impl ContainerRuntime` 对应方法
//! 改为一行薄委派。命名 `_inner` 后缀沿用 k8s_app_observation.rs 的 `stream_app_logs_inner`
//! 约定（避免 trait 同名方法依赖方法解析优先级）。

use chrono::Utc;
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus,
    RemovedContainerInfo, RuntimeContainerInfo,
};
use k8s_openapi::api::core::v1::{
    Container as K8sContainer, ContainerPort, EnvVar, LocalObjectReference,
    PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec, Probe, Service, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, DeleteParams, ListParams};
use shared_types::{
    ContainerBasicInfo, K8sSidecarSpec, K8sVolumeMountSpec, K8sVolumeSpec, ServiceType,
};
use tracing::{debug, info, warn};

use super::k8s_pod::K8sPodOps;
use super::k8s_pvc::K8sPvcOps;
use super::k8s_service::K8sServiceOps;
use super::kubernetes_runtime::{KubernetesRuntime, RUNTIME_MANAGED_LABEL};

impl KubernetesRuntime {
    /// 创建 agent-runner 容器（走 StatefulSet，K8s 原生 pod 级自愈）。
    ///
    /// 编排顺序：identifier 解析 → per-agent PVC ensure（副作用）→ cache-hit 早返回 →
    /// `build_agent_pod_spec` 纯构造 → headless svc + STS(replicas=1) → 等 Ready →
    /// ClusterIP svc → 取 info。UserApp 不走此路径（用 create_deployment）。
    pub(crate) async fn create_agent_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // 确定容器标识符（复用 ServiceType::container_identifier 单一事实源，
        // 与 docker 模式 / handler 层保持一致）。identifier 借自 pod_id/user_id/project_id 之一。
        // ⚠️ 不要在此重写优先级逻辑，否则会与 handler 层不一致 → ensure/chat 造出不同名 pod+PVC。
        let service_type = params.service_type.clone();
        let project_id = params.project_id.clone();
        let user_id = params.user_id.clone();
        let pod_id = params.pod_id.clone();

        let identifier: &str = service_type
            .container_identifier(pod_id.as_deref(), user_id.as_deref(), project_id.as_deref())
            .map_err(|e| ContainerRuntimeError::ConfigurationError(e.to_string()))?;

        // Pod 名称：统一使用 pod_name() helper（含 RFC 1123 下划线清理）
        let pod_name = self.pod_name(identifier, &service_type)?;

        // 阶段2 per-agent PVC (CephFS subvolume, ceph-csi 服务端配额, 绕开 client setfattr):
        // 仅隔离容器 (pod_id=None, project/user 级) 且 per_agent_pvc_enabled=true 走 per-agent PVC。
        // 共享容器 (pod_id=Some) 或回滚开关 false → 共享 PVC (选项A 行为)。
        if pod_id.is_none() && shared_types::per_agent_pvc_enabled() {
            self.ensure_workspace_pvc(identifier, &service_type, params.storage_size.as_deref())
                .await?;
        }

        // Check if pod already exists and is running
        if let Some(cached) = self.pod_cache.read().await.get(identifier)
            && cached.status == ContainerRuntimeStatus::Running
        {
            info!("[K8S] Pod {} already exists and is running", pod_name);
            return self
                .get_container_info_by_identifier_inner(identifier, &service_type)
                .await?
                .ok_or_else(|| ContainerRuntimeError::ContainerNotFound(identifier.to_string()));
        }

        // 构造 PodSpec（纯计算，无 K8s API 副作用）。
        // Pod 的 ObjectMeta（name/labels）不传给 STS——STS 模板的 labels 由
        // build_agent_statefulset 经 build_standard_labels 自行设置，故此处只返回 PodSpec。
        let pod_spec = self.build_agent_pod_spec(identifier, &service_type, &params)?;

        // agent-runner 走 StatefulSet（K8s 原生 pod 级自愈）：把 pod spec
        // wrap 进 STS（而非裸 Pod）。STS replicas=1 时 pod 被 evict/删除 → 控制器自动重建
        // 同名 pod（挂回同 PVC，数据不丢）；容器级 OOM 仍由 restartPolicy=Always 原地重启。
        // service_type 重名/不匹配由 ensure_agent_statefulset 内部删旧重建处理。
        self.ensure_agent_headless_service(identifier, &service_type)
            .await?;
        self.ensure_agent_statefulset(identifier, &service_type, pod_spec, 1)
            .await?;

        // Wait for pod to be ready
        self.wait_for_pod_ready(identifier, &service_type).await?;

        // Create K8s Service for Envoy Gateway routing
        self.create_agent_service(identifier, &service_type).await?;

        // Get pod info
        self.get_container_info_by_identifier_inner(identifier, &service_type)
            .await?
            .ok_or_else(|| {
                ContainerRuntimeError::ContainerCreationError(
                    "Pod created but info not found".to_string(),
                )
            })
    }

    /// 纯构造 agent-runner 的 PodSpec（image 选择、PVC 名、volumes/mounts 翻译、env 合并、
    /// command、probe、resources、sidecar）。
    ///
    /// 只读 `self.config` + `&params`，无 K8s API 副作用，自包含可单测。
    /// PodSpec 字面量与原 create_container 内联构造逐字一致。
    pub(crate) fn build_agent_pod_spec(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<PodSpec> {
        let project_id_val = params.project_id.clone().unwrap_or_default();
        let user_id_val = params.user_id.clone().unwrap_or_default();
        let service_type_str = service_type.to_string();
        let image = self.select_image(service_type);

        // Build resource requirements if limits are provided
        let resources = params
            .resource_limits
            .as_ref()
            .and_then(Self::build_resource_requirements);

        // workspace PVC:
        // - per-agent (pod_id=None + per_agent_pvc_enabled=true): per-agent PVC (subPath=None)
        // - 共享 (pod_id=Some 或 per_agent_pvc_enabled=false): 共享 PVC + subPath (选项A / 回滚)
        let per_agent = params.pod_id.is_none() && shared_types::per_agent_pvc_enabled();
        let (workspace_pvc, workspace_sub_path): (String, Option<String>) = if per_agent {
            (self.workspace_pvc_name(identifier, service_type)?, None)
        } else {
            match service_type {
                ServiceType::WebAgentRunner => (
                    std::env::var("RCODER_WORKSPACE_PVC_NAME")
                        .unwrap_or_else(|_| format!("{}-rcoder-workspace", self.namespace)),
                    Some(
                        std::env::var("RCODER_WORKSPACE_SUBPATH")
                            .ok()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "workspace".to_string()),
                    ),
                ),
                ServiceType::ComputerAgentRunner => (
                    std::env::var("RCODER_COMPUTER_WORKSPACE_PVC_NAME")
                        .unwrap_or_else(|_| format!("{}-rcoder-computer-workspace", self.namespace)),
                    Some(user_id_val.clone()),
                ),
                _ => (self.workspace_pvc_name(identifier, service_type)?, None),
            }
        };

        // (阶段2: xattr 目录配额已退役 —— 改用 per-agent subvolume PVC + CSI 服务端配额。
        //  parse_quantity_to_bytes / agent_workspace_quota_dir / xattr crate 已删, 见 Task 2.3)

        // 取 service 配置(完全分家):K8s 优先读 kubernetes_config;docker_config.multi_image_config
        // 仅作过渡期安全兜底(旧 chart 未带 kubernetes_config 段时,保留 workspace 路径/command/env 行为)。
        // volumes / volume_mounts / sidecars 只来自 kubernetes_config(docker_config 无此概念)。
        let k8s_service = self
            .config
            .kubernetes_config
            .get_service_config(service_type);
        let docker_service = self
            .config
            .docker_manager_config
            .multi_image_config
            .get_service_config(service_type);

        // workspace 挂载路径(K8s 模式 computer→/home/user, web→/app/project_workspace)
        let workspace_mount_path = k8s_service
            .map(|sc| sc.workspace_container_path())
            .or_else(|| docker_service.map(|sc| sc.workspace_container_path()))
            .unwrap_or_else(|| match service_type {
                ServiceType::ComputerAgentRunner => "/home/user".to_string(),
                _ => "/app/project_workspace".to_string(),
            });

        // 构建 volumes: 硬编码 workspace PVC(保留) + 翻译 kubernetes_config 额外卷
        let mut volumes_vec: Vec<Volume> = vec![Volume {
            name: "workspace".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: workspace_pvc.clone(),
                read_only: Some(false),
            }),
            ..Default::default()
        }];
        let extra_volumes: Vec<K8sVolumeSpec> =
            k8s_service.map(|s| s.volumes.clone()).unwrap_or_default();
        for v in extra_volumes.iter().flat_map(Self::translate_k8s_volume) {
            volumes_vec.push(v);
        }

        // 构建 volume_mounts: workspace 挂载 + 翻译 kubernetes_config 额外挂载(挂到 agent 容器)
        let mut volume_mounts_vec: Vec<VolumeMount> = vec![VolumeMount {
            name: "workspace".to_string(),
            mount_path: workspace_mount_path,
            sub_path: workspace_sub_path, // computer→Some(user_id)，web→None
            read_only: Some(false),
            ..Default::default()
        }];
        let extra_mounts: Vec<K8sVolumeMountSpec> = k8s_service
            .map(|s| s.volume_mounts.clone())
            .unwrap_or_default();
        for m in extra_mounts.iter().map(Self::translate_k8s_volume_mount) {
            volume_mounts_vec.push(m);
        }

        let volumes = Some(volumes_vec);
        let volume_mounts = Some(volume_mounts_vec);

        // sidecar 容器(只来自 kubernetes_config):如 log-collector tail 容器内日志到 stdout
        let sidecars: Vec<K8sSidecarSpec> =
            k8s_service.map(|s| s.sidecars.clone()).unwrap_or_default();

        // Build image pull secrets if configured
        let image_pull_secrets = self.config.image_pull_secret.as_ref().map(|secret| {
            vec![LocalObjectReference {
                name: secret.clone(),
            }]
        });

        Ok(PodSpec {
            volumes,
            image_pull_secrets,
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(false),
                ..Default::default()
            }),
            termination_grace_period_seconds: Some(15),
            containers: {
                // 主 agent 容器 + 翻译自 kubernetes_config 的 sidecar(如 log-collector)
                let mut containers_vec = vec![K8sContainer {
                    name: "agent".to_string(),
                    image: Some(image),
                    // IfNotPresent: 动态 pod 频繁创建（每 chat/computer-chat 一个），
                    // 节点已缓存就直接用，避免每次都去 registry 验 token/manifest。
                    // image 更新由主 Deployment 触发拉取（用户做 rollout restart 时），
                    // 主服务用新 image 启动后，动态 pod 跟着用同样的 image 引用。
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    // 启动命令：
                    //   - WebAgentRunner：从 config.yml 的 web-agent-runner.command 读取
                    //     （与 docker-compose 一致）。配置里的 /app/agent-runner-start.sh wrapper
                    //     会先 nohup 拉起 ttyd(7681)，再 exec agent_runner；agent_runner 的
                    //     ws_terminal 中间层(17681)依赖 ttyd 就绪后才会 bind。若不读配置而裸跑
                    //     agent_runner，ttyd 不启动 -> ws_terminal 等 7681 超时 abort ->
                    //     /computer/terminal 终端 WS 连不上。配置缺失时回退裸 agent_runner
                    //     （保留旧行为，至少 pod 能起；rcoder-master 镜像本身没有 CMD/ENTRYPOINT）。
                    //   - ComputerAgentRunner：刻意用 None 走镜像自带 ENTRYPOINT(start-up.sh)。
                    //     注意 config.yml 里 computer-agent-runner.command 写的是裸 agent_runner，
                    //     那是给 docker 运行时用的；K8s 下若改读它会绕过 start-up.sh，丢失 ttyd/VNC，
                    //     因此这里不复用 config.command。
                    command: match service_type {
                        ServiceType::WebAgentRunner => {
                            // 优先 kubernetes_config.command;过渡期回退 docker_config.command;
                            // 都缺则裸跑 agent_runner(保留旧行为,至少 pod 能起)。
                            let cmd = k8s_service
                                .and_then(|sc| {
                                    if sc.command.is_empty() {
                                        None
                                    } else {
                                        Some(sc.command.clone())
                                    }
                                })
                                .or_else(|| {
                                    docker_service.and_then(|sc| {
                                        if sc.command.is_empty() {
                                            None
                                        } else {
                                            Some(sc.command.clone())
                                        }
                                    })
                                })
                                .unwrap_or_else(|| vec!["/app/bin/agent_runner".to_string()]);
                            Some(cmd)
                        }
                        // ComputerAgentRunner / UserApp 用镜像自带 ENTRYPOINT/CMD
                        // （UserApp 实际走 create_deployment，不经此路径）
                        ServiceType::ComputerAgentRunner | ServiceType::UserApp => None,
                    },
                    env: {
                        let mut env_vars = vec![
                            EnvVar {
                                name: "PROJECT_ID".to_string(),
                                value: Some(project_id_val.to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "USER_ID".to_string(),
                                value: Some(user_id_val.to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "SERVICE_TYPE".to_string(),
                                value: Some(service_type_str.clone()),
                                ..Default::default()
                            },
                            // 部署模式标识: start-up.sh 据此 source extra (K8s 下 /home/user 是 PVC, 跳过 bind mount 权限修复)
                            EnvVar {
                                name: "DEPLOY_MODE".to_string(),
                                value: Some("k8s".to_string()),
                                ..Default::default()
                            },
                        ];
                        // 多租户环境变量（agent_runner 用于构建工作目录路径）
                        if let Some(ref tid) = params.tenant_id {
                            env_vars.push(EnvVar {
                                name: "TENANT_ID".to_string(),
                                value: Some(tid.clone()),
                                ..Default::default()
                            });
                        }
                        if let Some(ref sid) = params.space_id {
                            env_vars.push(EnvVar {
                                name: "SPACE_ID".to_string(),
                                value: Some(sid.clone()),
                                ..Default::default()
                            });
                        }
                        if let Some(ref it) = params.isolation_type {
                            env_vars.push(EnvVar {
                                name: "ISOLATION_TYPE".to_string(),
                                value: Some(it.clone()),
                                ..Default::default()
                            });
                        }
                        // 透传 service environment
                        // (PROJECT_WORKSPACE_BASE/RUST_LOG/SERVICE_MODE/AGENT_PORT 等,
                        //  让 sub-container 行为与 Docker 模式一致)。跳过已硬编码的同名 env。
                        // 合并顺序:docker_config 兜底 → kubernetes_config 覆盖(K8s 主)。
                        const RESERVED: [&str; 6] = [
                            "PROJECT_ID",
                            "USER_ID",
                            "SERVICE_TYPE",
                            "TENANT_ID",
                            "SPACE_ID",
                            "ISOLATION_TYPE",
                        ];
                        let mut merged_env: std::collections::HashMap<String, String> =
                            std::collections::HashMap::new();
                        if let Some(sc) = docker_service {
                            for (k, v) in &sc.environment {
                                merged_env.insert(k.clone(), v.clone());
                            }
                        }
                        if let Some(sc) = k8s_service {
                            for (k, v) in &sc.environment {
                                merged_env.insert(k.clone(), v.clone());
                            }
                        }
                        for (k, v) in &merged_env {
                            if RESERVED.contains(&k.as_str()) {
                                continue;
                            }
                            env_vars.push(EnvVar {
                                name: k.clone(),
                                value: Some(v.clone()),
                                ..Default::default()
                            });
                        }
                        Some(env_vars)
                    },
                    ports: Some(vec![
                        ContainerPort {
                            container_port: shared_types::GRPC_DEFAULT_PORT as i32,
                            name: Some("grpc".to_string()),
                            ..Default::default()
                        },
                        // HTTP health check port for agent_runner
                        ContainerPort {
                            container_port: 8086,
                            name: Some("http".to_string()),
                            ..Default::default()
                        },
                    ]),
                    resources,
                    volume_mounts,
                    liveness_probe: Some(Probe {
                        http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                            path: Some("/health".to_string()),
                            port: IntOrString::Int(8086),
                            ..Default::default()
                        }),
                        initial_delay_seconds: Some(30),
                        period_seconds: Some(10),
                        timeout_seconds: Some(3),
                        failure_threshold: Some(3),
                        success_threshold: Some(1),
                        ..Default::default()
                    }),
                    // readiness_probe: 探 /ready (检查 gRPC 50051 就绪), 与 liveness 的 /health 分离。
                    // gRPC 没起 → /ready 返 503 → pod NotReady → Service 摘流量; gRPC 起了 → 200 → 放流量。
                    // initialDelay/period 用 1s: 每 1s 探一次, /ready=200 后 ~1s 内 Ready。
                    // failure_threshold=20 容忍启动期 503 (gRPC 还没起), 不被误判 NotReady 太久。
                    readiness_probe: Some(Probe {
                        http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                            path: Some("/ready".to_string()),
                            port: IntOrString::Int(8086),
                            ..Default::default()
                        }),
                        initial_delay_seconds: Some(1),
                        period_seconds: Some(1),
                        timeout_seconds: Some(3),
                        failure_threshold: Some(20),
                        success_threshold: Some(1),
                        ..Default::default()
                    }),
                    // 注: 此处不配 startup_probe。
                    // 历史: 曾配 startup_probe(initialDelay=5/period=10/failure=12, ~2min 宽限) 防
                    // initdb 慢启动被 liveness 误杀; 但 bebba86 已把 PG initdb 异步化(supervisor 托管),
                    // /health 2s 内 ready, 慢启动根因消除。保留 startup_probe 反成拖累: initialDelay=5 +
                    // period=10 粒度让 Ready 卡在 ~11s(应用早 ready 却要等首探 pod+5s)。去掉后
                    // readiness(period=1) 直接接管, ~3s 内 Ready; 启动期保护由 liveness 兜底
                    // (initialDelay=30 + failure=3×period=10 = 50s 宽限, 远大于 2s ready, 不会被误杀)。
                    // 若日后 agent-runner 首启又变慢(>50s), 再考虑重新引入激进配置的 startup probe。
                    // preStop lifecycle hook: 在 kubelet 发送 SIGTERM 之前执行，
                    // 确保 JuiceFS FUSE 卷上的写入 buffer flush 到磁盘，
                    // 减少 FUSE unmount 卡住的概率
                    lifecycle: Some(k8s_openapi::api::core::v1::Lifecycle {
                        pre_stop: Some(k8s_openapi::api::core::v1::LifecycleHandler {
                            exec: Some(k8s_openapi::api::core::v1::ExecAction {
                                command: Some(vec![
                                    "sh".to_string(),
                                    "-c".to_string(),
                                    "sync && sleep 2".to_string(),
                                ]),
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }];
                // sidecar(只来自 kubernetes_config.services[].sidecars)。
                // 无配置时 pod = 仅 agent(干净基线)。log-collector 等采集器在 configmap 声明。
                containers_vec.extend(sidecars.iter().map(Self::translate_k8s_sidecar));
                containers_vec
            },
            // Always(非 Never): agent 容器 OOM/崩溃时由 kubelet 原地重启自愈。
            // Never 下 agent 一死(sidecar 还活着 → pod 仍 Running)rcoder 既不重启也不重建 → 用户中断。
            // rcoder 的 stop/restart/destroy 均走 pods().delete() 整 pod 删, 不依赖 Never;
            // /computer/agent/stop 是 gRPC 取消会话(进程继续), 故 Always 只补崩盘自愈、不冲突。
            restart_policy: Some("Always".to_string()),
            service_account_name: Some(self.config.service_account_name.clone()),
            ..Default::default()
        })
    }

    pub(crate) async fn get_container_info_inner(
        &self,
        identifier: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // Try cache first
        if let Some(cached) = self.pod_cache.read().await.get(identifier)
            && cached.status == ContainerRuntimeStatus::Running
        {
            return Ok(Some(
                self.build_container_basic_info(identifier, cached).await?,
            ));
        }

        // Query K8s API - 使用标准 K8s 标签查询（与 build_standard_labels 一致）
        let search_queries = vec![
            format!("app.kubernetes.io/instance={}", identifier),
            format!("rcoder.io/identifier={}", identifier),
        ];

        for query in search_queries {
            let lp = ListParams::default().labels(&query);
            if let Ok(pods) = self.pods().list(&lp).await
                && let Some(pod) = pods.items.into_iter().next()
            {
                let status = Self::extract_pod_status(&pod);
                let metadata = &pod.metadata;
                let uid = metadata.uid.clone().unwrap_or_default();
                let name = metadata.name.clone().unwrap_or_default();
                let pod_ip = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.pod_ip.clone())
                    .unwrap_or_default();
                let created_at = metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|ts| {
                        chrono::DateTime::from_timestamp(
                            ts.0.as_second(),
                            ts.0.subsec_nanosecond() as u32,
                        )
                        .unwrap_or_else(Utc::now)
                    })
                    .unwrap_or_else(Utc::now);

                let pod_info = RuntimeContainerInfo {
                    container_id: uid,
                    // agent-runner 走 STS：pod 名 = {sts_name}-0，但 container_name 用作寻址基名
                    // （Service FQDN/grpc_addr/backend_addr 都从它派生 `{name}-svc`），故剥 -0 还原
                    // sts_name，否则所有 gRPC/VNC 地址会指向不存在的 {...}-0-svc。bare-pod 残留无
                    // -0 后缀，strip 安全（identity）。实际 pod 名由 agent_pod_name() 按需取。
                    container_name: Self::sts_name_from_pod_name(&name).to_string(),
                    container_ip: pod_ip,
                    status,
                    created_at,
                    env_vars: None,
                };

                // Update cache if running
                if pod_info.status == ContainerRuntimeStatus::Running {
                    self.pod_cache
                        .write()
                        .await
                        .insert(identifier.to_string(), pod_info.clone());
                }

                return Ok(Some(
                    self.build_container_basic_info(identifier, &pod_info)
                        .await?,
                ));
            }
        }

        Ok(None)
    }

    pub(crate) async fn get_container_info_by_identifier_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let info = self.get_container_info_inner(identifier).await?;
        if info.is_some() {
            // Self-heal：异常创建（如 OrbStack sandbox 超时）可能留下"pod 在、svc 丢"
            // 的不一致状态——pod 重试后起来了，但 create_agent_service 那步没跑完。
            // 后续 Chat 走 svc FQDN `{pod}-svc:50051` 会 transport error → GRPC_ERROR。
            // create_agent_service 幂等（先 get，存在即返回，缺失才建），此处补建，避免人工删 pod 介入。
            // 失败仅 warn（get 是读操作，自愈失败不应阻塞读）。
            if let Err(e) = self.create_agent_service(identifier, service_type).await {
                warn!(
                    "[K8S] self-heal: 补建 agent service 失败 identifier={}, service_type={:?} (non-fatal): {}",
                    identifier, service_type, e
                );
            }
        }
        Ok(info)
    }

    pub(crate) async fn find_container_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        // Check cache first
        if let Some(cached) = self.pod_cache.read().await.get(identifier) {
            return Ok(Some(cached.clone()));
        }

        // 1) Query by concrete pod name
        let pod_name = self.pod_name(identifier, service_type)?;
        match self.pods().get(&pod_name).await {
            Ok(pod) => return Ok(Some(Self::runtime_info_from_pod(&pod))),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to get pod by name '{}': {}",
                    pod_name, e
                )));
            }
        }

        // 2) Query by labels (使用新的标准标签)
        let selector = format!("app.kubernetes.io/instance={}", identifier);
        let pods = self
            .pods()
            .list(&ListParams::default().labels(&selector).limit(1))
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!(
                    "Failed to list pods with selector '{}': {}",
                    selector, e
                ))
            })?;

        if let Some(pod) = pods.items.into_iter().next() {
            return Ok(Some(Self::runtime_info_from_pod(&pod)));
        }

        // 3) 兼容旧标签查询（平滑迁移）
        for old_selector in [
            format!("pod_id={}", identifier),
            format!("user_id={}", identifier),
            format!("project_id={}", identifier),
        ] {
            let pods = self
                .pods()
                .list(&ListParams::default().labels(&old_selector).limit(1))
                .await
                .map_err(|e| {
                    ContainerRuntimeError::K8sError(format!(
                        "Failed to list pods with selector '{}': {}",
                        old_selector, e
                    ))
                })?;

            if let Some(pod) = pods.items.into_iter().next() {
                return Ok(Some(Self::runtime_info_from_pod(&pod)));
            }
        }

        Ok(None)
    }

    pub(crate) async fn stop_container_by_identifier_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let total_start = std::time::Instant::now();
        let pod_name = self.pod_name(identifier, service_type)?;

        // STS 实际 pod 名（等待终止用）；pod_name 即 sts_name。
        let agent_pod = self.agent_pod_name(identifier, service_type)?;

        info!(
            "[K8S] Destroying agent StatefulSet {} (identifier={}, service_type={})",
            pod_name, identifier, service_type
        );

        // Step 0: 删除 ClusterIP Service（先摘流量 / 移除 DNS，再销毁 pod）
        if let Err(e) = self.delete_agent_service(identifier, service_type).await {
            warn!("[K8S] Failed to delete ClusterIP Service for {}: {} (continuing)", identifier, e);
        }

        // Step 1: 删除 StatefulSet（Foreground cascade → pod 随之终止）。回收 = 彻底销毁 STS
        // （非 scale 0；scale 0 会留 STS 永不清理）。PVC 保留（数据复用，下次 ensure 重建挂回）。
        if let Err(e) = self.delete_agent_statefulset(identifier, service_type).await {
            warn!("[K8S] Failed to delete StatefulSet {}: {} (continuing)", pod_name, e);
        }

        // Step 2: 等 pod {sts}-0 完全终止（Foreground cascade 异步；等其 404 再继续，
        // 避免与立即重建的新 pod 抢 RWO PVC）。
        if let Err(e) = self.wait_for_pod_terminated(&agent_pod).await {
            warn!("[K8S] wait_for_pod_terminated for {} failed: {} (continuing)", agent_pod, e);
        }

        // Step 3: 删除 headless Service（与 STS/ClusterIP 一并彻底回收）
        if let Err(e) = self.delete_agent_headless_service(identifier, service_type).await {
            warn!("[K8S] Failed to delete headless Service for {}: {} (continuing)", identifier, e);
        }

        self.pod_cache.write().await.remove(identifier);

        info!(
            "[K8S] agent {} destroyed (STS + ClusterIP/headless svc deleted; PVC preserved for reuse), total time: {:.1}s",
            pod_name,
            total_start.elapsed().as_secs_f64()
        );

        Ok(())
    }

    pub(crate) async fn list_containers_inner(
        &self,
    ) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);
        let pods =
            self.pods().list(&lp).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Failed to list pods: {}", e))
            })?;

        let mut result = Vec::new();
        for p in pods.items {
            let pod: Pod = p;
            let status = Self::extract_pod_status(&pod);
            let metadata = &pod.metadata;

            // 从 Pod 的 labels 中提取环境变量信息
            let mut env_vars = std::collections::HashMap::new();
            if let Some(labels) = &metadata.labels {
                if let Some(project_id) = labels.get("project_id") {
                    env_vars.insert("PROJECT_ID".to_string(), project_id.clone());
                }
                if let Some(user_id) = labels.get("user_id") {
                    env_vars.insert("USER_ID".to_string(), user_id.clone());
                }
            }

            let pod_info = RuntimeContainerInfo {
                container_id: metadata.uid.clone().unwrap_or_default(),
                // 同 get 路径：剥 STS ordinal -0，container_name 作寻址基名（见上方注释）。
                container_name: Self::sts_name_from_pod_name(
                    &metadata.name.clone().unwrap_or_default(),
                )
                .to_string(),
                container_ip: pod
                    .status
                    .as_ref()
                    .and_then(|s| s.pod_ip.clone())
                    .unwrap_or_default(),
                status,
                created_at: metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|ts| {
                        chrono::DateTime::from_timestamp(
                            ts.0.as_second(),
                            ts.0.subsec_nanosecond() as u32,
                        )
                        .unwrap_or_else(Utc::now)
                    })
                    .unwrap_or_else(Utc::now),
                env_vars: Some(env_vars),
            };
            result.push(pod_info);
        }

        Ok(result)
    }

    pub(crate) async fn sync_states_inner(
        &self,
    ) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        let mut removed = Vec::new();

        // 获取缓存快照 (identifier, RuntimeContainerInfo)
        let cache_snapshot: Vec<(String, RuntimeContainerInfo)> = self
            .pod_cache
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let checked_count = cache_snapshot.len() as u32;

        for (identifier, container_info) in cache_snapshot {
            // container_name 已是 sts_name（get_container_info 源头剥过 -0）。判"真没了"看 STS
            // 是否存在，不看 pod 404（STS replicas>0 时 pod 被 evict/重建会瞬时空缺，误判清缓存
            // 会中断重建）。
            let sts_name = &container_info.container_name;
            match self.statefulsets().get(sts_name).await {
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    // STS 已删 → 真没了；从缓存移除 + 收集（消费方 container_sync 只用 container_ip 清 gRPC 池）
                    self.pod_cache.write().await.remove(&identifier);
                    removed.push(RemovedContainerInfo {
                        container_name: container_info.container_name.clone(),
                        container_ip: container_info.container_ip.clone(),
                        identifier: identifier.clone(),
                        // FIXME: RuntimeContainerInfo 不带 service_type；消费方未用，暂占位。
                        service_type: ServiceType::WebAgentRunner,
                    });
                    info!(
                        "[K8S_SYNC] StatefulSet gone, removed from cache: {} (identifier={})",
                        sts_name, identifier
                    );
                }
                Ok(_) => {
                    // STS 存在（replicas>0 pod 运行/重建中；replicas=0 已停）→ 不动缓存
                }
                Err(e) => {
                    warn!("[K8S_SYNC] Failed to check StatefulSet {}: {}", sts_name, e);
                }
            }
        }

        Ok((checked_count, removed))
    }

    pub(crate) async fn cleanup_all_inner(&self) -> ContainerRuntimeResult<()> {
        let total_start = std::time::Instant::now();
        info!("[K8S_CLEANUP] Starting cleanup_all — sequential Service → Pod → PVC deletion");

        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);

        // ── Step 0: 批量删除 K8s Service ──
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        match services
            .delete_collection(&DeleteParams::default(), &lp)
            .await
        {
            Ok(_) => info!("[K8S_CLEANUP] Service delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S_CLEANUP] Service delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // ── Step 1: 获取所有 managed Pod 名称（用于后续等待终止）──
        let pods_to_wait: Vec<String> = self
            .pods()
            .list(&lp)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ConnectionError(format!(
                    "Failed to list pods for cleanup: {}",
                    e
                ))
            })?
            .items
            .iter()
            .filter_map(|pod| pod.metadata.name.clone())
            .collect();

        info!(
            "[K8S_CLEANUP] Found {} managed pods to clean",
            pods_to_wait.len()
        );

        // ── Step 2: 批量删除 Pod（graceful, Foreground 传播）──
        let dp = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
            grace_period_seconds: Some(15),
            ..Default::default()
        };

        // 先删 StatefulSet（cascade 删其 pod，且阻止 STS 控制器重建 pod）。
        // agent-runner 现走 STS，若直接删 pod 而 STS 仍在，控制器会立即重建 pod → 永远删不掉。
        match self.statefulsets().delete_collection(&dp, &lp).await {
            Ok(_) => info!("[K8S CLEANUP] StatefulSet delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S CLEANUP] StatefulSet delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // 再删 Pod（兜底：清理历史遗留的游离裸 pod，或 STS cascade 未覆盖的残留）
        match self.pods().delete_collection(&dp, &lp).await {
            Ok(_) => info!("[K8S_CLEANUP] Pod delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S_CLEANUP] Pod delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // ── Step 3: 等待所有 Pod 完全终止 ──
        // 关键：必须在删除 PVC 之前完成，确保 FUSE 卷已卸载
        let wait_futures: Vec<_> = pods_to_wait
            .iter()
            .map(|pod_name| self.wait_for_pod_terminated(pod_name))
            .collect();

        let wait_results = futures_util::future::join_all(wait_futures).await;
        for (pod_name, result) in pods_to_wait.iter().zip(wait_results.iter()) {
            if let Err(e) = result {
                tracing::warn!(
                    "[K8S_CLEANUP] Pod {} termination wait failed: {}",
                    pod_name,
                    e
                );
            }
        }

        // ── Step 4: PVC 清理策略 ──
        //
        // 不在 cleanup_all 中主动删除 PVC，原因：
        // 1. Pod 删除时 K8s PropagationPolicy::Foreground 会级联清理关联的 PVC
        // 2. 主动删除正在被 pod 使用的 PVC 会导致 PVC 卡在 Terminating 状态（pvc-protection finalizer）
        // 3. 多副本部署时，cleanup_all 会误删其他 rcoder 实例正在使用的 PVC
        // 4. Terminating PVC 会导致后续 create_container 失败（409 重试循环）
        info!(
            "[K8S_CLEANUP] PVC cleanup skipped — PVCs are cleaned up via K8s cascading deletion when pods are removed"
        );

        // 清理缓存 (含 subvolume_path_cache — 跨重启 PVC 可能被运维删除重建,
        // 陈旧 cache 导致 resolve 命中旧 subvolPath → rcoder 读老 subvol 而 pod 挂新 PVC → 数据面分裂)
        self.pod_cache.write().await.clear();
        self.subvolume_path_cache.write().await.clear();

        info!(
            "[K8S_CLEANUP] cleanup_all completed in {:.1}s",
            total_start.elapsed().as_secs_f64()
        );
        Ok(())
    }

    /// 原地重启 agent 容器：exec 进 agent 容器 `kill -TERM 1` → agent_runner SIGTERM handler
    /// 优雅退出 → kubelet `restartPolicy=Always` **原地重启容器**（卷不 unstage，避免 CephFS
    /// `NodeStageVolume` re-stage ~60s）。对比 destroy+recreate（删 STS+等 pod 404+重建，慢）。
    ///
    /// 轮询 agent 容器 `restartCount` 自增确认 kubelet 已原地重启；30s 超时 → Err（调用方
    /// `pod_restart` 回落 destroy+recreate，处理 agent 卡死/PID 1 不接 SIGTERM 等异常）。
    /// agent 容器名固定 "agent"；PID 1 = agent_runner（实测 ComputerAgentRunner
    /// `/usr/local/bin/agent_runner -p 8086`），SIGTERM 直达其 shutdown handler。
    pub(crate) async fn restart_agent_container_inplace(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        use kube::api::AttachParams;
        use tokio::io::AsyncReadExt;

        const AGENT_CONTAINER: &str = "agent";
        let pod_name = self.agent_pod_name(identifier, service_type)?;

        // 1. 基线 restartCount（agent 容器；缺失视作 0）
        let baseline = self
            .pods()
            .get(&pod_name)
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get pod for restart baseline: {e}"))
            })?
            .status
            .and_then(|s| s.container_statuses)
            .and_then(|cs| cs.into_iter().find(|c| c.name == AGENT_CONTAINER))
            .map(|c| c.restart_count)
            .unwrap_or(0);

        // 2. exec kill -TERM 1（agent 容器 PID 1 = agent_runner → SIGTERM → 优雅退出 → kubelet 原地重启）
        let ap = AttachParams::default()
            .container(AGENT_CONTAINER)
            .stdout(true)
            .stderr(true)
            .stdin(false)
            .tty(false);
        let mut attached = self
            .pods()
            .exec(
                &pod_name,
                vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "kill -TERM 1".to_string(),
                ],
                &ap,
            )
            .await
            .map_err(|e| ContainerRuntimeError::ContainerExecError(format!("exec kill: {e}")))?;
        // drain stdout/stderr（kill 通常无输出；读空释放 buffer，避免 join 死锁；reader 出作用域 drop 后再 join）
        if let Some(mut r) = attached.stdout() {
            let mut buf = String::new();
            let _ = r.read_to_string(&mut buf).await;
        }
        if let Some(mut r) = attached.stderr() {
            let mut buf = String::new();
            let _ = r.read_to_string(&mut buf).await;
        }
        if let Err(e) = attached.join().await {
            debug!("[K8S] restart exec join (kill -TERM 1): {e} (non-fatal, SIGTERM 已发)");
        }

        // 3. 轮询原地重启完成：restartCount 自增（kubelet 原地重启）+ ready。30s 超时 → Err（回落）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if std::time::Instant::now() > deadline {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "in-place restart timeout: agent restartCount did not increment within 30s (pod={pod_name})"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let restarted = self
                .pods()
                .get(&pod_name)
                .await
                .ok()
                .and_then(|p| p.status)
                .and_then(|s| s.container_statuses)
                .and_then(|cs| cs.into_iter().find(|c| c.name == AGENT_CONTAINER))
                .map(|c| (c.restart_count, c.ready));
            if let Some((rc, ready)) = restarted
                && rc > baseline
                && ready
            {
                info!(
                    "[K8S] agent restarted in-place: {} (restartCount {}→{}, ready, volume 未 unstage)",
                    pod_name, baseline, rc
                );
                return Ok(());
            }
            // restartCount 未自增 / ready 未就绪 / pod get 短暂失败（重启中）→ 继续轮询
        }
    }
}
