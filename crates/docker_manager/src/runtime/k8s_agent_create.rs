//! agent-runner 容器创建 + PodSpec 构造（从 k8s_agent_pod.rs 拆出）。
//!
//! - `create_agent_container`：创建编排（PVC ensure → STS(replicas=1) → 等 Ready → ClusterIP svc）。
//! - `build_agent_pod_spec`：纯 PodSpec 构造（image/volumes/env/command/probes/lifecycle/sidecar），无副作用。
//!
//! 与 k8s_agent_query.rs（读）、k8s_agent_pod.rs（stop/restart/sync/cleanup 变更）正交。

use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus,
};
use k8s_openapi::api::core::v1::{
    Container as K8sContainer, ContainerPort, EnvVar, LocalObjectReference,
    PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, Probe, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use shared_types::{
    ContainerBasicInfo, K8sSidecarSpec, K8sVolumeMountSpec, K8sVolumeSpec, ServiceType,
};
use tracing::info;

use super::k8s_pod::K8sPodOps;
use super::k8s_pvc::K8sPvcOps;
use super::k8s_service::K8sServiceOps;
use super::kubernetes_runtime::KubernetesRuntime;

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
        // UserAppBuilder 天然 per-app PVC(不受灰度开关限制),总是 ensure。
        if pod_id.is_none()
            && (shared_types::per_agent_pvc_enabled()
                || matches!(service_type, ServiceType::UserAppBuilder))
        {
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
                    std::env::var("RCODER_COMPUTER_WORKSPACE_PVC_NAME").unwrap_or_else(|_| {
                        format!("{}-rcoder-computer-workspace", self.namespace)
                    }),
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
                // UserAppBuilder: per-app PVC 挂载点(file-server PROJECT_SOURCE_DIR 与之一致)
                ServiceType::UserAppBuilder => "/app/userapp-workspace".to_string(),
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
                        // ComputerAgentRunner / UserApp / UserAppBuilder 用镜像自带 ENTRYPOINT/CMD
                        // (UserApp 实际走 create_deployment,不经此路径;
                        //  UserAppBuilder 复用 dev-rcoder-agent-runner 镜像,走其 start-up.sh 启动 agent_runner + 内嵌 file-server)
                        ServiceType::ComputerAgentRunner
                        | ServiceType::UserApp
                        | ServiceType::UserAppBuilder => None,
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
                        // 透传 UserApp build 必需 env 给 agent-runner（build 在 agent-runner 执行）:
                        // release lock 三元组（rcoder 自身 env 已有，来自 helm runtime identity 注入）。
                        // 缺这些 agent-runner 无法生成 release.lock.toml。
                        for var in [
                            "RCODER_PINGAP_VERSION",
                            "RCODER_PINGAP_COMMIT",
                            "RCODER_RUNTIME_IMAGE_DIGEST",
                        ] {
                            if merged_env.contains_key(var) {
                                continue;
                            }
                            if let Ok(val) = std::env::var(var)
                                && !val.is_empty()
                            {
                                env_vars.push(EnvVar {
                                    name: var.to_string(),
                                    value: Some(val),
                                    ..Default::default()
                                });
                            }
                        }
                        // build timeout: rcoder env 透传，缺省 1800s（全量多语言 workspace build）
                        if !merged_env.contains_key("DEV_COMMAND_TIMEOUT_SECS") {
                            let timeout = std::env::var("DEV_COMMAND_TIMEOUT_SECS")
                                .ok()
                                .filter(|v| !v.is_empty())
                                .unwrap_or_else(|| "1800".to_string());
                            env_vars.push(EnvVar {
                                name: "DEV_COMMAND_TIMEOUT_SECS".to_string(),
                                value: Some(timeout),
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
                        // file-server port (embedded, UserApp workspace build / package download)
                        ContainerPort {
                            container_port: 60_000,
                            name: Some("file-server".to_string()),
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
                    // 启用 TTY + stdin: 让 k9s / kubectl exec -it 能进交互式 shell 排查。
                    // agent_runner 是服务进程(PID 1 监听 8086/50051, 不读 stdin), tty 不影响其运行;
                    // 与 Docker 模式(container_creator.rs tty:true)对齐。
                    // 副作用: PTY 下 agent 容器 stdout/stderr 合并成一条流(loki stream 标记失效),
                    // 但 agent_runner 同时写文件日志(/app/logs, 经 log-collector sidecar 进 loki, 完整),
                    // 故日志排查不受影响。
                    tty: Some(true),
                    stdin: Some(true),
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
}
