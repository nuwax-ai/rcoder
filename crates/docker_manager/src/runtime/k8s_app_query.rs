//! UserApp Deployment 查询/状态(从 k8s_deployment.rs 拆出)。
//!
//! get_app_status/read_app_container_spec/list_app_status + deployment_to_status +
//! container_error_message。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    AppPortStatus, ContainerRuntimeError,
    ContainerRuntimeResult, ContainerSpecSnapshot, DeploymentStatus, ExposeType,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::apps::v1::Deployment;
#[cfg(feature = "kubernetes")]
use kube::api::ListParams;


use super::kubernetes_runtime::KubernetesRuntime;
use super::k8s_app_helpers::{parse_port_expose, PORT_EXPOSE_ANNOTATION};
use super::k8s_deployment::{APP_CONTAINER_NAME, APP_LABEL_PREFIX, APP_MANAGED_BY, RCODER_LABEL_PREFIX};


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


    /// 读 app 当前容器的 `command`/`env` 快照（update 部分更新回退用）。
    ///
    /// - **command**：UserApp 存于 `container.args`（用镜像 ENTRYPOINT + 用户命令作 args），
    ///   兼顾 `container.command`（其他路径内联）→ `command.or(args)`。
    /// - **env**：UserApp env 走 ConfigMap（envFrom 引用 `{app}-config`），读 `.data` 还原字面值。
    ///
    /// 任一资源 404 → 对应字段 None（互不影响）。读失败上抛（update 调用方降级 warn）。
    /// 注：trait 方法 `ContainerRuntime::get_app_container_spec` 委派到这里（见 kubernetes_runtime.rs）。
    pub async fn read_app_container_spec(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
        // command：Deployment 主容器的 args（UserApp）/ command（其他）
        let command = match self
            .deployments_api()
            .get(&self.app_deployment_name(app_id))
            .await
        {
            Ok(deploy) => deploy
                .spec
                .and_then(|s| s.template.spec)
                .and_then(|ps| ps.containers.into_iter().next())
                .and_then(|c| c.command.or(c.args)),
            Err(kube::Error::Api(ae)) if ae.code == 404 => None,
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get deployment for container spec: {e}"
                )))
            }
        };
        // env：ConfigMap `{app}-config`.data（apply_app_configmap 写入 = params.env 原样）
        let env = match self.configmaps_api().get(&self.app_config_name(app_id)).await {
            Ok(cm) => cm
                .data
                .filter(|m| !m.is_empty())
                .map(|m| m.into_iter().collect()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => None,
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get configmap for container spec: {e}"
                )))
            }
        };
        Ok(ContainerSpecSnapshot { command, env })
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
    async fn collect_tcp_nodeports(
        &self,
        app_id: &str,
    ) -> std::collections::HashMap<String, u16> {
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
    tcp_nodeports: &std::collections::HashMap<String, u16>,
) -> Vec<AppPortStatus> {
    let port_expose: std::collections::HashMap<u16, ExposeType> = deploy
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
                            .map_or((ExposeType::Http, None), |np| {
                                (ExposeType::Tcp, Some(*np))
                            }),
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
fn container_error_message(cs: &k8s_openapi::api::core::v1::ContainerStatus) -> Option<String> {
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
