//! K8s app 网络暴露（从 k8s_app_create.rs 拆出）——ClusterIP Service /
//! HTTPRoute（Gateway 模式）/ NodePort 三条暴露路径，与 Deployment 组装
//! （k8s_app_create.rs）职责分离。

use container_runtime_api::{
    AppPortSpec, AppPortStatus, ContainerCreateParams, ContainerRuntimeError,
    ContainerRuntimeResult, ExposeType,
};
use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DynamicObject, Patch, PatchParams, PostParams};
use tracing::warn;

use super::kubernetes_runtime::KubernetesRuntime;

#[cfg(feature = "kubernetes")]
impl KubernetesRuntime {
    /// apply ClusterIP Service（暴露 app 端口）—— SSA create-or-update
    pub(super) async fn apply_app_service(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<()> {
        let ports: Vec<ServicePort> = params
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| ServicePort {
                        name: Some(p.name.clone()),
                        port: p.port as i32,
                        target_port: Some(IntOrString::Int(p.port as i32)),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut ports = ports;
        // 运行容器固定端口随 Service 一并暴露——
        // `/api/v1/userapp/proxy/{ttyd,dbx}/prod/{app_id}` 代理上游为 Service FQDN，
        // 若 7681/4224 不在 Service ports 内，
        // 代理连接将超时（app-runtime 镜像 supervisor 恒起 ttyd/dbx-web，
        // targetPort 恒可达）。
        // 60000 = 容器内 file-server-proxy（rcoder 转发层 prod 文件操作的上游，
        // AllRust → 8086 Rust file-server；单 app 模式）。
        // 用户 ports 为空时同样需要本 Service（仅含固定端口也建）——
        // 平台内定四要素下 ports 恒非空，此分支防御直接 REST create 的调用方。
        // （原 pgweb(8081) 成员已随 pgweb 退役删除；Service 全量构造+声明式 apply，
        // 存量 Service 的 8081 端口随下次 reconcile 自动消失。）
        for (name, port) in [
            ("ttyd", shared_types::TTYD_PORT),
            ("dbx", shared_types::DBX_PORT),
            ("file-server", shared_types::AGENT_FILE_SERVER_PORT),
        ] {
            // 撞名即跳过（不论端口值）：再 push 同名端口会撞 K8s 校验
            // "port names must be unique"，整个 Service 被拒绝。用户占用保留名
            // 属自担行为（7681/4224 未暴露，对应 runtime 代理将不可达）。
            if let Some(conflict) = ports.iter().find(|p| p.name.as_deref() == Some(name)) {
                warn!(
                    "[K8S] app {} Service port name '{}' occupied by user port {}, skipping builtin exposure",
                    app_id, name, conflict.port
                );
                continue;
            }
            ports.push(ServicePort {
                name: Some(name.to_string()),
                port: port as i32,
                target_port: Some(IntOrString::Int(port as i32)),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            });
        }
        let tenant_id = params.tenant_id.as_deref();
        let space_id = params.space_id.as_deref();
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(self.app_service_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id, tenant_id, space_id)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                // selector 用稳定 core（创建后不可变）
                selector: Some(self.build_app_labels(app_id, None, None)),
                ports: Some(ports),
                ..Default::default()
            }),
            ..Default::default()
        };
        let body = serde_json::to_value(&svc)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize service: {e}")))?;
        // 条件化 upsert（uid+RV 前置）：接管后的迟到写 409 而非静默收敛。
        conditioned_upsert_service(&self.services_api(), &self.app_service_name(app_id), body)
            .await?;
        Ok(())
    }

    /// apply HTTPRoute（HTTP 端口 → Gateway）—— SSA create-or-update，path prefix `/apps/{app_id}`
    ///
    /// `http_port.strip_prefix=true` 时加 URLRewrite filter（ReplacePrefixMatch），让 EG 把
    /// `/apps/{id}/api` → `/api` 再转发给后端（与 Docker Pingora 模式行为对齐：
    /// Docker `/proxy/{port}/api` → 后端收到 `/api`，天然 strip）。
    pub(super) async fn apply_app_httproute(
        &self,
        app_id: &str,
        http_port: &AppPortSpec,
        gateway_name: &str,
        gateway_namespace: &str,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        let port = http_port.port;
        // 默认 true（与 Docker Pingora 模式一致：后端收到 clean path）。
        // 显式传 false 才保留 /apps/{id} 前缀。
        let strip_prefix = http_port.strip_prefix.unwrap_or(true);
        let routes = self.httproute_api();

        // strip_prefix=true → URLRewrite ReplacePrefixMatch：`/apps/{id}/api` → `/api`
        let filters = if strip_prefix {
            serde_json::json!([{
                "type": "URLRewrite",
                "urlRewrite": {
                    "path": {
                        "type": "ReplacePrefixMatch",
                        "replacePrefixMatch": "/"
                    }
                }
            }])
        } else {
            serde_json::json!([])
        };

        let route = serde_json::json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": {
                "name": self.app_http_route_name(app_id),
                "namespace": self.namespace,
                "labels": self.build_app_labels(app_id, tenant_id, space_id),
            },
            "spec": {
                "parentRefs": [{
                    "name": gateway_name,
                    "namespace": gateway_namespace,
                }],
                "rules": [{
                    "matches": [{
                        "path": {
                            "type": "PathPrefix",
                            "value": format!("/apps/{app_id}")
                        }
                    }],
                    "filters": filters,
                    "backendRefs": [{
                        "name": self.app_service_name(app_id),
                        "port": port as i32,
                    }]
                }]
            }
        });

        // 条件化 upsert（uid+RV 前置）：接管后的迟到写 409 而非静默收敛。
        conditioned_upsert_route(&routes, &self.app_http_route_name(app_id), route).await?;
        Ok(())
    }

    /// apply NodePort Service（TCP 端口对外暴露）—— SSA create-or-update，
    /// 返回实际分配的 node_port 列表（server 分配，apply 后从返回对象读取）。
    /// 当前 TCP 初期不对外，此函数暂未被调用（保留供未来启用）。
    #[allow(dead_code)]
    pub(super) async fn apply_app_nodeport(
        &self,
        app_id: &str,
        tcp_ports: &[AppPortSpec],
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> ContainerRuntimeResult<Vec<AppPortStatus>> {
        if tcp_ports.is_empty() {
            return Ok(vec![]);
        }
        let service_ports: Vec<ServicePort> = tcp_ports
            .iter()
            .map(|p| ServicePort {
                name: Some(p.name.clone()),
                port: p.port as i32,
                target_port: Some(IntOrString::Int(p.port as i32)),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            })
            .collect();
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(self.app_nodeport_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id, tenant_id, space_id)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("NodePort".to_string()),
                // selector 用稳定 core（创建后不可变）
                selector: Some(self.build_app_labels(app_id, None, None)),
                ports: Some(service_ports),
                ..Default::default()
            }),
            ..Default::default()
        };
        let body = serde_json::to_value(&svc)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize nodeport: {e}")))?;
        // 条件化 upsert（uid+RV 前置）：接管后的迟到写 409 而非静默收敛。
        let created =
            conditioned_upsert_service(&self.services_api(), &self.app_nodeport_name(app_id), body)
                .await?;

        // 提取实际分配的 node_port：按 name（回退 port 号）关联，避免 server 返回顺序与
        // 请求顺序不一致时 name 与 external_port 配错。
        use std::collections::HashMap;
        let mut result = vec![];
        if let Some(spec) = created.spec
            && let Some(ports) = spec.ports
        {
            let np_by_key: HashMap<String, u16> = ports
                .iter()
                .filter_map(|p| {
                    let np = p.node_port? as u16;
                    let key = p.name.clone().unwrap_or_else(|| format!("{}", p.port));
                    Some((key, np))
                })
                .collect();
            for req in tcp_ports {
                if let Some(&np) = np_by_key.get(&req.name) {
                    result.push(AppPortStatus {
                        name: req.name.clone(),
                        port: req.port,
                        expose_type: ExposeType::Tcp,
                        external_port: Some(np),
                    });
                }
            }
        }
        Ok(result)
    }
}

/// 条件化 upsert（Service）：活对象 → uid+RV 前置 merge patch；缺席 →
/// create，并发 409 → 读 winner 身份后条件 merge（同写面收敛）。step-D 写面
/// fencing 收口——接管后的迟到写在 409 上失败而非静默收敛。
async fn conditioned_upsert_service(
    api: &kube::Api<Service>,
    name: &str,
    body: serde_json::Value,
) -> ContainerRuntimeResult<Service> {
    use super::k8s_runtime_helpers::inject_object_identity;
    match api
        .get_opt(name)
        .await
        .map_err(|e| ContainerRuntimeError::K8sError(format!("get service {name}: {e}")))?
    {
        Some(live) => {
            let mut body = body;
            inject_object_identity(&mut body, &live.metadata)?;
            api.patch(name, &PatchParams::default(), &Patch::Merge(body))
                .await
                .map_err(|e| {
                    ContainerRuntimeError::K8sError(format!(
                        "conditioned update service {name}: {e}"
                    ))
                })
        }
        None => {
            let create_body: Service = serde_json::from_value(body.clone())
                .map_err(|e| ContainerRuntimeError::K8sError(format!("decode service: {e}")))?;
            match api.create(&PostParams::default(), &create_body).await {
                Ok(created) => Ok(created),
                Err(kube::Error::Api(ae)) if ae.code == 409 => {
                    let live = api.get(name).await.map_err(|e| {
                        ContainerRuntimeError::K8sError(format!("get service winner {name}: {e}"))
                    })?;
                    let mut body = body;
                    inject_object_identity(&mut body, &live.metadata)?;
                    api.patch(name, &PatchParams::default(), &Patch::Merge(body))
                        .await
                        .map_err(|e| {
                            ContainerRuntimeError::K8sError(format!(
                                "conditioned adopt service {name}: {e}"
                            ))
                        })
                }
                Err(e) => Err(ContainerRuntimeError::K8sError(format!(
                    "create service {name}: {e}"
                ))),
            }
        }
    }
}

/// 条件化 upsert（HTTPRoute 动态资源）：机理同 [`conditioned_upsert_service`]。
async fn conditioned_upsert_route(
    api: &kube::Api<DynamicObject>,
    name: &str,
    body: serde_json::Value,
) -> ContainerRuntimeResult<()> {
    use super::k8s_runtime_helpers::inject_object_identity;
    match api
        .get_opt(name)
        .await
        .map_err(|e| ContainerRuntimeError::K8sError(format!("get httproute {name}: {e}")))?
    {
        Some(live) => {
            let mut body = body;
            inject_object_identity(&mut body, &live.metadata)?;
            api.patch(name, &PatchParams::default(), &Patch::Merge(body))
                .await
                .map_err(|e| {
                    ContainerRuntimeError::K8sError(format!(
                        "conditioned update httproute {name}: {e}"
                    ))
                })?;
            Ok(())
        }
        None => {
            let create_body: DynamicObject = serde_json::from_value(body.clone())
                .map_err(|e| ContainerRuntimeError::K8sError(format!("decode httproute: {e}")))?;
            match api.create(&PostParams::default(), &create_body).await {
                Ok(_) => Ok(()),
                Err(kube::Error::Api(ae)) if ae.code == 409 => {
                    let live = api.get(name).await.map_err(|e| {
                        ContainerRuntimeError::K8sError(format!("get httproute winner {name}: {e}"))
                    })?;
                    let mut body = body;
                    inject_object_identity(&mut body, &live.metadata)?;
                    api.patch(name, &PatchParams::default(), &Patch::Merge(body))
                        .await
                        .map_err(|e| {
                            ContainerRuntimeError::K8sError(format!(
                                "conditioned adopt httproute {name}: {e}"
                            ))
                        })?;
                    Ok(())
                }
                Err(e) => Err(ContainerRuntimeError::K8sError(format!(
                    "create httproute {name}: {e}"
                ))),
            }
        }
    }
}
