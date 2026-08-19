//! UserApp Deployment 查询/状态(从 k8s_deployment.rs 拆出)。
//!
//! get_app_status/read_app_container_spec/list_app_status + deployment_to_status +
//! container_error_message。

#[cfg(feature = "kubernetes")]
use std::collections::HashMap;

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    AppPortSpec, AppPortStatus, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerSpecSnapshot, DeploymentStatus, ExposeType,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::apps::v1::Deployment;
#[cfg(feature = "kubernetes")]
use kube::api::ListParams;

use super::k8s_app_helpers::{
    IDLE_TIMEOUT_ANNOTATION, PORT_EXPOSE_ANNOTATION, RECYCLE_ENABLED_ANNOTATION,
    WAKE_ON_TRAFFIC_ANNOTATION, parse_port_expose,
};
use super::k8s_deployment::{
    APP_CONTAINER_NAME, APP_LABEL_PREFIX, APP_MANAGED_BY, RCODER_LABEL_PREFIX,
};
use super::kubernetes_runtime::KubernetesRuntime;

impl KubernetesRuntime {
    /// 查询单个 app 的运行时状态（实时查 Deployment + Pod）
    pub async fn get_app_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let name = self.app_deployment_name(app_id);
        let deploy = match self.deployments_api().get(&name).await {
            Ok(d) => d,
            Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(None),
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get deployment: {e}"
                )));
            }
        };
        Ok(Some(self.deployment_to_status(app_id, &deploy).await))
    }

    /// 读 app 当前容器的 desired 快照（update 部分更新回退用）。
    ///
    /// - **command**：UserApp 存于 `container.args`（用镜像 ENTRYPOINT + 用户命令作 args），
    ///   兼顾 `container.command`（其他路径内联）→ `command.or(args)`。
    /// - **env**：UserApp env 走 ConfigMap（envFrom 引用 `{app}-config`），读 `.data` 还原字面值。
    /// - **secrets**：Secret `{app}-secret`（写入用 string_data，API 返回在 `.data` 并 base64）。
    /// - **resources**：pod template `resources.limits` 原样还原 Quantity 字符串
    ///   （写入与读出同格式，无换算损耗；storage 是 per-app PVC 配额不在容器层，不读）。
    /// - **health_check**：liveness/readiness probe 反推（`build_probe` 的逆映射）。
    ///
    /// 任一资源 404 → 对应字段 None（互不影响）。读失败上抛（update 调用方降级 warn）。
    /// 注：trait 方法 `ContainerRuntime::get_app_container_spec` 委派到这里（见 kubernetes_runtime.rs）。
    pub async fn read_app_container_spec(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
        use container_runtime_api::AppResourceRequirements;

        // deployment GET 一次：command + resources + probes 同源提取。
        let deploy = match self
            .deployments_api()
            .get(&self.app_deployment_name(app_id))
            .await
        {
            Ok(deploy) => Some(deploy),
            Err(kube::Error::Api(ae)) if ae.code == 404 => None,
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get deployment for container spec: {e}"
                )));
            }
        };
        let container = deploy
            .as_ref()
            .and_then(|d| d.spec.as_ref())
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|ps| ps.containers.first());
        let command = container.and_then(|c| c.command.clone().or_else(|| c.args.clone()));
        let resources = container
            .and_then(|c| c.resources.as_ref())
            .and_then(|r| r.limits.as_ref())
            .map(|limits| AppResourceRequirements {
                // Quantity 是 newtype String，取 .0 原样还原（写入读出同格式无换算）
                cpu: limits.get("cpu").map(|q| q.0.clone()),
                memory: limits.get("memory").map(|q| q.0.clone()),
                storage: None,
                ephemeral_storage: limits.get("ephemeral-storage").map(|q| q.0.clone()),
            })
            .filter(|r| r.cpu.is_some() || r.memory.is_some() || r.ephemeral_storage.is_some());
        let health_check = container.and_then(probe_to_health_check);

        // env：ConfigMap `{app}-config`.data（apply_app_configmap 写入 = params.env 原样）
        let env = match self
            .configmaps_api()
            .get(&self.app_config_name(app_id))
            .await
        {
            Ok(cm) => cm
                .data
                .filter(|m| !m.is_empty())
                .map(|m| m.into_iter().collect()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => None,
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get configmap for container spec: {e}"
                )));
            }
        };

        // secrets：Secret `.data` base64 解码还原（写入走 string_data，API 侧自动转 data；
        // 值类型是 ByteString，直接取内部字节，无需再按文本 base64 解码字符串）
        let secrets = match self.secrets_api().get(&self.app_secret_name(app_id)).await {
            Ok(secret) => secret.data.and_then(|data| {
                data.into_iter()
                    .map(|(key, value)| {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .decode(&value.0)
                            .map(|decoded| (key, String::from_utf8_lossy(&decoded).into_owned()))
                            .map_err(|e| e.to_string())
                    })
                    .collect::<Result<HashMap<String, String>, String>>()
                    .ok()
                    .filter(|m| !m.is_empty())
            }),
            Err(kube::Error::Api(ae)) if ae.code == 404 => None,
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get secret for container spec: {e}"
                )));
            }
        };

        // ports：container.ports（name/port）join port-expose 注解（expose_type；缺省
        // Http）。strip_prefix 未持久化（仅体现在 HTTPRoute URLRewrite filter），读回
        // None——显式 false 的用户部分更新后回默认剥前缀行为（罕见配置，取舍见
        // ContainerSpecSnapshot.ports 文档）。空 ports = 未设置（None）。
        let ports = container.and_then(|c| c.ports.as_ref()).map(|ps| {
            let expose_map = deploy
                .as_ref()
                .and_then(|d| d.metadata.annotations.as_ref())
                .and_then(|ann| ann.get(PORT_EXPOSE_ANNOTATION))
                .map(|value| parse_port_expose(value))
                .unwrap_or_default();
            ps.iter()
                .map(|p| AppPortSpec {
                    name: p.name.clone().unwrap_or_default(),
                    port: p.container_port as u16,
                    expose_type: expose_map
                        .get(&(p.container_port as u16))
                        .cloned()
                        .unwrap_or(ExposeType::Http),
                    strip_prefix: None,
                })
                .collect::<Vec<_>>()
        });

        Ok(ContainerSpecSnapshot {
            command,
            env,
            secrets,
            resources,
            health_check,
            ports,
        })
    }

    /// 列出所有 rcoder-app-manager 托管的 app 状态（对账用）
    pub async fn list_app_status(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        let lp = ListParams::default()
            .labels(&format!("{}/managed-by={APP_MANAGED_BY}", APP_LABEL_PREFIX));
        let deploys = self
            .deployments_api()
            .list(&lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("list deployments: {e}")))?;
        let mut out = vec![];
        for d in deploys {
            // app_id 从 label rcoder.io/app-id 取
            let app_id = d
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(&format!("{}/app-id", RCODER_LABEL_PREFIX)))
                .cloned()
                .unwrap_or_default();
            if app_id.is_empty() {
                continue;
            }
            out.push(self.deployment_to_status(&app_id, &d).await);
        }
        Ok(out)
    }

    /// Deployment 对象 → DeploymentStatus（含关联 Pod 的实时信息）。
    ///
    /// 编排 4 个单一职责子步骤：
    /// - `fetch_app_pod_info`：拉关联 Pod（pod_ip/node/restart/started_at/error）—— IO
    /// - `derive_phase`：replicas+ready+error → phase —— 纯函数
    /// - `collect_tcp_nodeports`：查 NodePort Service 的 TCP node_port —— IO
    /// - `derive_port_statuses`：container ports + annotation + nodeports → 端口状态 —— 纯函数
    async fn deployment_to_status(&self, app_id: &str, deploy: &Deployment) -> DeploymentStatus {
        let replicas = deploy.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
        let ready_replicas = deploy
            .status
            .as_ref()
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0);

        // 关联 Pod 信息先取：phase 判定需要容器状态（CrashLoop/ImagePull/异常退出 → Error）。
        let (pod_ip, node, restart_count, started_at, error_message) =
            self.fetch_app_pod_info(app_id).await;
        let phase = derive_phase(replicas, ready_replicas, &error_message);
        let tcp_nodeports = self.collect_tcp_nodeports(app_id).await;
        let ports = derive_port_statuses(deploy, &tcp_nodeports);

        // recycle 配置从 Deployment metadata.annotations 读回（absent=旧 app=默认可回收）；
        // created_at 来自 creationTimestamp，供回收扫描器做 protection 龄期判断。
        let ann = deploy.metadata.annotations.as_ref();
        let recycle_enabled = ann
            .and_then(|a| a.get(RECYCLE_ENABLED_ANNOTATION))
            .map(|s| !s.eq_ignore_ascii_case("false"));
        let idle_timeout_seconds = ann
            .and_then(|a| a.get(IDLE_TIMEOUT_ANNOTATION))
            .and_then(|s| s.parse::<u64>().ok());
        let wake_on_traffic = ann
            .and_then(|a| a.get(WAKE_ON_TRAFFIC_ANNOTATION))
            .and_then(|value| value.parse::<bool>().ok());
        let created_at = deploy
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.to_string());

        DeploymentStatus {
            app_id: app_id.to_string(),
            replicas,
            ready_replicas,
            phase,
            message: error_message,
            pod_ip: if pod_ip.is_empty() {
                None
            } else {
                Some(pod_ip)
            },
            node: if node.is_empty() { None } else { Some(node) },
            restart_count,
            started_at,
            ports,
            resource_version: deploy.metadata.resource_version.clone(),
            recycle_enabled,
            idle_timeout_seconds,
            wake_on_traffic,
            created_at,
        }
    }

    /// 拉取 app 关联 Pod 的实时信息（取一个；app 当前为单副本）。
    /// 返回 (pod_ip, node, restart_count, started_at, error_message)；无 Pod 或 list 失败返默认空值。
    async fn fetch_app_pod_info(
        &self,
        app_id: &str,
    ) -> (String, String, u32, Option<String>, Option<String>) {
        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
        match self.pods_api().list(&lp).await {
            Ok(pods) => pods
                .items
                .into_iter()
                .next()
                .and_then(|p| {
                    let st = p.status?;
                    // 按容器名取 "app" 容器状态（防御 sidecar 注入后 pop() 取错容器）
                    let cs = st
                        .container_statuses
                        .and_then(|v| v.into_iter().find(|c| c.name == APP_CONTAINER_NAME))?;
                    // started_at：从 container state.running 提取实际启动时间
                    let started_at = cs
                        .state
                        .as_ref()
                        .and_then(|s| s.running.as_ref())
                        .and_then(|r| r.started_at.as_ref())
                        .map(|t| t.0.to_string());
                    // 启动失败原因（CrashLoop / 镜像拉取失败 / 异常退出）；正常拉起的中间态
                    // （ContainerCreating）不在此列，不会被误判为 Error。
                    let error_message = container_error_message(&cs);
                    Some((
                        st.pod_ip.unwrap_or_default(),
                        p.spec.and_then(|s| s.node_name).unwrap_or_default(),
                        cs.restart_count as u32,
                        started_at,
                        error_message,
                    ))
                })
                .unwrap_or_default(),
            Err(_) => (String::new(), String::new(), 0, None, None),
        }
    }

    /// TCP 端口的 node_port：查 NodePort Service，按 port name 关联（TCP 对外时用）。
    /// Service 不存在（无 TCP 端口）返空 map。
    async fn collect_tcp_nodeports(&self, app_id: &str) -> HashMap<String, u16> {
        self.services_api()
            .get(&self.app_nodeport_name(app_id))
            .await
            .ok()
            .and_then(|svc| svc.spec)
            .and_then(|s| s.ports)
            .map(|ports| {
                ports
                    .into_iter()
                    .filter_map(|p| {
                        let name = p.name.unwrap_or_default();
                        let np = p.node_port.map(|n| n as u16)?;
                        Some((name, np))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// phase 推导（纯函数）。
///
/// replicas=0 → Stopped；容器启动失败 → Error（优先于 ready 判定，避免 CrashLoop 期间
/// 偶发 ready_replicas>0 被误报 Running）；就绪副本达标 → Running；否则 Starting。
fn derive_phase(replicas: i32, ready_replicas: i32, error_message: &Option<String>) -> String {
    if replicas == 0 {
        "Stopped".to_string()
    } else if error_message.is_some() {
        "Error".to_string()
    } else if ready_replicas >= replicas && ready_replicas > 0 {
        "Running".to_string()
    } else {
        "Starting".to_string()
    }
}

/// 端口状态推导（纯函数）：从 Deployment spec container ports + annotation port-expose +
/// TCP nodeports 推导每端口 expose_type/external_port。
///
/// expose_type 优先用 annotation（create 时写入，TCP 不对外也能准确区分）；
/// 缺失（旧 app）回退 NodePort 推导：在 NodePort Service 里 = Tcp，否则 Http。
fn derive_port_statuses(
    deploy: &Deployment,
    tcp_nodeports: &HashMap<String, u16>,
) -> Vec<AppPortStatus> {
    let port_expose: HashMap<u16, ExposeType> = deploy
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(PORT_EXPOSE_ANNOTATION))
        .map(|s| parse_port_expose(s))
        .unwrap_or_default();
    deploy
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|s| s.containers.first())
        .and_then(|c| c.ports.as_ref())
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    let name = p.name.clone().unwrap_or_default();
                    let port = p.container_port as u16;
                    let (expose_type, external_port) = match port_expose.get(&port) {
                        Some(ExposeType::Tcp) => {
                            (ExposeType::Tcp, tcp_nodeports.get(&name).copied())
                        }
                        Some(ExposeType::Http) => (ExposeType::Http, None),
                        // 回退：无 annotation（旧 app）—— 在 NodePort Service 里 = Tcp，否则 Http
                        None => tcp_nodeports
                            .get(&name)
                            .map_or((ExposeType::Http, None), |np| (ExposeType::Tcp, Some(*np))),
                    };
                    AppPortStatus {
                        name,
                        port,
                        expose_type,
                        external_port,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// 从容器状态提取"启动失败"原因（供 phase=Error 的 message）。
///
/// 命中条件（任一）：
/// - `state.waiting.reason` ∈ {CrashLoopBackOff, ImagePullBackOff, ErrImagePull,
///   CreateContainerConfigError, CreateContainerError, InvalidImageName, RunContainerError,
///   StartError}（`ContainerCreating` 是正常拉起中间态，不在此列，不会被误判）
/// - `state.terminated.exit_code != 0`（容器异常退出）
///
/// CrashLoop 时当前 `state=waiting`，真实退出码在 `last_state.terminated`，一并附带，
/// 便于定位"挂在哪一次退出、退出码多少"。
pub(crate) fn container_error_message(
    cs: &k8s_openapi::api::core::v1::ContainerStatus,
) -> Option<String> {
    let state = cs.state.as_ref()?;
    const BAD_WAITING: &[&str] = &[
        "CrashLoopBackOff",
        "ImagePullBackOff",
        "ErrImagePull",
        "CreateContainerConfigError",
        "CreateContainerError",
        "InvalidImageName",
        "RunContainerError",
        "StartError",
    ];
    if let Some(w) = state.waiting.as_ref()
        && let Some(reason) = w.reason.as_ref()
        && BAD_WAITING.contains(&reason.as_str())
    {
        let detail = w
            .message
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        let term = cs
            .last_state
            .as_ref()
            .and_then(|ls| ls.terminated.as_ref())
            .map(|t| {
                format!(
                    " (last exit={}, reason={})",
                    t.exit_code,
                    t.reason.as_deref().unwrap_or("")
                )
            })
            .unwrap_or_default();
        return Some(format!("{reason}{detail}{term}"));
    }
    if let Some(t) = state.terminated.as_ref()
        && t.exit_code != 0
    {
        let reason = t.reason.as_deref().unwrap_or("");
        let msg = t
            .message
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        return Some(format!(
            "terminated: exit code={exit}, reason={reason}{msg}",
            exit = t.exit_code
        ));
    }
    None
}

/// probes → [`container_runtime_api::AppHealthCheck`] 反推（`build_probe` 的逆映射）。
///
/// readiness http_get → `Http{path, port}`；tcp_socket → `Tcp{port}`；两者皆无 → None。
/// 细节：`build_probe` 对缺省 path 写 "/"，反推 "/" → None；liveness http_get.path 与
/// readiness path 不同时还原 `liveness_path`（相同则 None=复用 path）；delay/period 原样还原。
#[cfg(feature = "kubernetes")]
fn probe_to_health_check(
    container: &k8s_openapi::api::core::v1::Container,
) -> Option<container_runtime_api::AppHealthCheck> {
    use container_runtime_api::{AppHealthCheck, HealthCheckType};
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

    fn probe_port(port: &IntOrString) -> Option<u16> {
        match port {
            IntOrString::Int(i) => u16::try_from(*i).ok(),
            IntOrString::String(s) => s.parse().ok(),
        }
    }

    let readiness = container.readiness_probe.as_ref()?;
    let (check_type, port, path) = if let Some(http) = readiness.http_get.as_ref() {
        let path = http.path.clone().filter(|p| p != "/");
        (HealthCheckType::Http, probe_port(&http.port), path)
    } else {
        let tcp = readiness.tcp_socket.as_ref()?;
        (HealthCheckType::Tcp, probe_port(&tcp.port), None)
    };
    let liveness_path = container
        .liveness_probe
        .as_ref()
        .and_then(|lp| lp.http_get.as_ref())
        .and_then(|lh| lh.path.clone())
        .filter(|lp_path| lp_path != "/" && Some(lp_path) != path.as_ref());
    Some(AppHealthCheck {
        check_type,
        path,
        liveness_path,
        port,
        initial_delay_seconds: readiness
            .initial_delay_seconds
            .and_then(|s| u32::try_from(s).ok()),
        period_seconds: readiness.period_seconds.and_then(|s| u32::try_from(s).ok()),
    })
}

#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus,
    };

    // ---- derive_phase：四态 + 边界优先级 ----

    #[test]
    fn phase_zero_replicas_is_stopped_regardless_of_error() {
        // replicas=0 最先判断：已缩容，即便容器报 error 也归 Stopped
        assert_eq!(derive_phase(0, 0, &None), "Stopped");
        assert_eq!(derive_phase(0, 0, &Some("CrashLoop".into())), "Stopped");
    }

    #[test]
    fn phase_error_preferred_over_ready() {
        // CrashLoop 期间偶发 ready_replicas>0，error 必须优先 → Error（防误报 Running）
        assert_eq!(derive_phase(1, 1, &Some("err".into())), "Error");
        assert_eq!(derive_phase(2, 2, &Some("err".into())), "Error");
    }

    #[test]
    fn phase_running_when_ready_meets_replicas() {
        assert_eq!(derive_phase(1, 1, &None), "Running");
        assert_eq!(derive_phase(3, 3, &None), "Running");
    }

    #[test]
    fn phase_starting_when_ready_below_replicas() {
        assert_eq!(derive_phase(1, 0, &None), "Starting");
        assert_eq!(derive_phase(3, 1, &None), "Starting");
    }

    #[test]
    fn phase_running_requires_ready_positive() {
        // ready_replicas > 0 是 Running 硬条件：replicas>0 但 ready=0 → Starting
        assert_eq!(derive_phase(2, 0, &None), "Starting");
    }

    // ---- container_error_message：启动失败原因提取 ----

    fn cs_waiting(reason: &str, message: Option<&str>) -> ContainerStatus {
        ContainerStatus {
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some(reason.to_string()),
                    message: message.map(|m| m.to_string()),
                }),
                ..Default::default()
            }),
            last_state: None,
            ..Default::default()
        }
    }

    fn cs_terminated(exit: i32, reason: Option<&str>) -> ContainerStatus {
        ContainerStatus {
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: exit,
                    reason: reason.map(|r| r.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            last_state: None,
            ..Default::default()
        }
    }

    #[test]
    fn error_message_crashloop_with_detail() {
        let cs = cs_waiting("CrashLoopBackOff", Some("back-off 5s"));
        assert_eq!(
            container_error_message(&cs).as_deref(),
            Some("CrashLoopBackOff: back-off 5s")
        );
    }

    #[test]
    fn error_message_image_pull_no_detail() {
        let cs = cs_waiting("ImagePullBackOff", None);
        assert_eq!(
            container_error_message(&cs).as_deref(),
            Some("ImagePullBackOff")
        );
    }

    #[test]
    fn error_message_includes_last_terminated_exit_code() {
        // CrashLoop 时当前 state=waiting，真实退出码在 last_state.terminated，应附带
        let mut cs = cs_waiting("CrashLoopBackOff", None);
        cs.last_state = Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: 137,
                reason: Some("OOMKilled".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            container_error_message(&cs).as_deref(),
            Some("CrashLoopBackOff (last exit=137, reason=OOMKilled)")
        );
    }

    #[test]
    fn error_message_container_creating_is_none() {
        // ContainerCreating 是正常拉起中间态，不在 BAD_WAITING → None（不误判 Error）
        let cs = cs_waiting("ContainerCreating", None);
        assert!(container_error_message(&cs).is_none());
    }

    #[test]
    fn error_message_nonzero_exit() {
        let cs = cs_terminated(137, Some("OOMKilled"));
        assert_eq!(
            container_error_message(&cs).as_deref(),
            Some("terminated: exit code=137, reason=OOMKilled")
        );
    }

    #[test]
    fn error_message_zero_exit_is_none() {
        let cs = cs_terminated(0, None);
        assert!(container_error_message(&cs).is_none());
    }

    #[test]
    fn error_message_empty_state_is_none() {
        let cs = ContainerStatus {
            state: None,
            ..Default::default()
        };
        assert!(container_error_message(&cs).is_none());
    }

    // ---- derive_port_statuses：annotation 优先 + NodePort 回退 ----

    fn deploy_from_json(json: &str) -> Deployment {
        serde_json::from_str(json).expect("解析测试 Deployment 失败")
    }

    #[test]
    fn port_status_annotation_tcp_with_nodeport() {
        // annotation 标 5432=tcp + NodePort 有该 name → Tcp + external_port
        let deploy = deploy_from_json(
            r#"{"metadata":{"annotations":{"rcoder.io/port-expose":"80:http,5432:tcp"}},
               "spec":{"template":{"spec":{"containers":[{"name":"app","ports":[
                   {"name":"http","containerPort":80},
                   {"name":"pg","containerPort":5432}
               ]}]}}}}"#,
        );
        let mut nodeports = HashMap::new();
        nodeports.insert("pg".to_string(), 30001u16);
        let ports = derive_port_statuses(&deploy, &nodeports);
        assert_eq!(ports.len(), 2);
        let pg = ports.iter().find(|p| p.port == 5432).unwrap();
        assert_eq!(pg.expose_type, ExposeType::Tcp);
        assert_eq!(pg.external_port, Some(30001));
        let http = ports.iter().find(|p| p.port == 80).unwrap();
        assert_eq!(http.expose_type, ExposeType::Http);
        assert_eq!(http.external_port, None);
    }

    #[test]
    fn port_status_no_annotation_falls_back_to_nodeport() {
        // 旧 app 无 annotation：端口在 NodePort Service 里 = Tcp，否则 Http
        let deploy = deploy_from_json(
            r#"{"metadata":{},"spec":{"template":{"spec":{"containers":[{"name":"app","ports":[
                {"name":"http","containerPort":80},
                {"name":"pg","containerPort":5432}
            ]}]}}}}"#,
        );
        let mut nodeports = HashMap::new();
        nodeports.insert("pg".to_string(), 30002u16);
        let ports = derive_port_statuses(&deploy, &nodeports);
        let pg = ports.iter().find(|p| p.port == 5432).unwrap();
        assert_eq!(pg.expose_type, ExposeType::Tcp); // nodeport 里有 → Tcp
        assert_eq!(pg.external_port, Some(30002));
        let http = ports.iter().find(|p| p.port == 80).unwrap();
        assert_eq!(http.expose_type, ExposeType::Http); // nodeport 里无 → Http
    }

    #[test]
    fn port_status_no_container_ports_empty() {
        let deploy = deploy_from_json(
            r#"{"metadata":{"annotations":{"rcoder.io/port-expose":"80:http"}},
               "spec":{"template":{"spec":{"containers":[{"name":"app"}]}}}}"#,
        );
        let ports = derive_port_statuses(&deploy, &HashMap::new());
        assert!(ports.is_empty());
    }
}
