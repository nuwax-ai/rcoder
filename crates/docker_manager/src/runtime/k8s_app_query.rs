//! Userapp Deployment 查询/状态(从 k8s_deployment.rs 拆出)。
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
    /// - **command**：Userapp 存于 `container.args`（用镜像 ENTRYPOINT + 用户命令作 args），
    ///   兼顾 `container.command`（其他路径内联）→ `command.or(args)`。
    /// - **env**：Userapp env 走 ConfigMap（envFrom 引用 `{app}-config`），读 `.data` 还原字面值。
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
        let ports = container
            .and_then(|c| c.ports.as_ref())
            .filter(|ps| !ps.is_empty())
            .map(|ps| {
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
        let lp = ListParams {
            label_selector: Some(format!("{}/managed-by={APP_MANAGED_BY}", APP_LABEL_PREFIX)),
            // resourceVersion=0 走 apiserver watch cache（内存读）——不带 rv 的
            // list 是 quorum read（等 etcd 多副本确认），查询面高频轮询会把
            // 压力传导到 etcd；列表展示容忍 cache 的短暂陈旧
            resource_version: Some("0".to_string()),
            ..Default::default()
        };
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
        let lp = ListParams {
            label_selector: Some(format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX)),
            // 查询面同样走 watch cache（与 list_app_status 一致）
            resource_version: Some("0".to_string()),
            ..Default::default()
        };
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

// 纯推导函数（derive_phase/derive_port_statuses/container_error_message/
// probe_to_health_check）与测试已拆至 k8s_app_status_derive.rs；
// re-export 保持 k8s_agent_query 的既有引用路径不变
pub(crate) use super::k8s_app_status_derive::container_error_message;
use super::k8s_app_status_derive::{derive_phase, derive_port_statuses, probe_to_health_check};
