//! UserApp Deployment 创建路径(从 k8s_deployment.rs 拆出)。
//!
//! apply_app_configmap/secret/service/httproute/nodeport/deployment + build_app_deployment +
//! create_app_resources 编排。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    AppPortSpec, AppPortStatus, ContainerCreateParams, ContainerRuntimeError,
    ContainerRuntimeResult, ExposeType, HttpExpose,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapEnvSource, Container as K8sContainer, ContainerPort, EnvFromSource, EnvVar,
    PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec,
    SecretEnvSource, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use kube::api::{Api, Patch};
#[cfg(feature = "kubernetes")]
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
#[cfg(feature = "kubernetes")]
use tracing::info;

#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;

#[cfg(feature = "kubernetes")]
use super::k8s_pvc::K8sPvcOps;
use super::kubernetes_runtime::KubernetesRuntime;
use super::k8s_app_helpers::{
    build_app_resource_requirements, build_probe, config_hash_annotations,
    encode_port_expose_annotations,
};
use super::k8s_deployment::APP_CONTAINER_NAME;


impl KubernetesRuntime {


    /// apply ConfigMap（存 env，非敏感）—— SSA create-or-update
    async fn apply_app_configmap(
        &self,
        app_id: &str,
        env: &std::collections::HashMap<String, String>,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(self.app_config_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id, tenant_id, space_id)),
                ..Default::default()
            },
            data: Some(env.clone().into_iter().collect()),
            ..Default::default()
        };
        let body = serde_json::to_value(&cm)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize configmap: {e}")))?;
        self.configmaps_api()
            .patch(
                &self.app_config_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply configmap: {e}")))?;
        Ok(())
    }


    /// apply Secret（存 secrets，敏感）—— SSA create-or-update
    async fn apply_app_secret(
        &self,
        app_id: &str,
        secrets: &std::collections::HashMap<String, String>,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        // K8s Secret data 需要 base64；StringData 更方便
        use k8s_openapi::api::core::v1::Secret;
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(self.app_secret_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id, tenant_id, space_id)),
                ..Default::default()
            },
            string_data: Some(secrets.clone().into_iter().collect()),
            ..Default::default()
        };
        let body = serde_json::to_value(&secret)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize secret: {e}")))?;
        self.secrets_api()
            .patch(
                &self.app_secret_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply secret: {e}")))?;
        Ok(())
    }


    /// 构建 Deployment 资源
    fn build_app_deployment(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<Deployment> {
        let image = params.image_override.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "UserApp create_deployment requires image_override".to_string(),
            )
        })?;

        let tenant_id = params.tenant_id.as_deref();
        let space_id = params.space_id.as_deref();
        // selector 用稳定 core（创建后不可变），metadata/template 用 full（含 tenant/space）
        let selector_labels = self.build_app_labels(app_id, None, None);
        let full_labels = self.build_app_labels(app_id, tenant_id, space_id);

        // 端口
        let ports: Vec<ContainerPort> = params
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| ContainerPort {
                        name: Some(p.name.clone()),
                        container_port: p.port as i32,
                        ..Default::default()
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 资源：requests/limits 解耦策略下沉到 build_app_resource_requirements（与 agent 侧
        // build_resource_requirements 共享 build_decoupled_resources，值一致）。
        let resources = params
            .app_resources
            .as_ref()
            .and_then(build_app_resource_requirements);

        // 健康检查 probe
        let (liveness, readiness) = params.health_check.as_ref().map_or((None, None), |hc| {
            let probe = build_probe(hc);
            (probe.clone(), probe)
        });

        // 环境变量（ConfigMap + Secret 通过 envFrom 引用）
        // ConfigMap/Secret 均设 optional=true：只有 env/secrets 非空时才建，引用安全。
        let env_from = Some(vec![
            EnvFromSource {
                config_map_ref: Some(ConfigMapEnvSource {
                    name: self.app_config_name(app_id),
                    optional: Some(true),
                }),
                ..Default::default()
            },
            EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: self.app_secret_name(app_id),
                    optional: Some(true),
                }),
                ..Default::default()
            },
        ]);

        // 额外直接注入 APP_ID 环境变量
        let env = Some(vec![EnvVar {
            name: "APP_ID".to_string(),
            value: Some(app_id.to_string()),
            ..Default::default()
        }]);

        // UserApp (K8s 永远 per-app): per-app CephFS subvolume PVC (subPath=None, subvolume 天然边界),
        // 由 create_app_resources ensure。rcoder 经挂根聚合 ({cephfs_root}/{subvolumePath}) 访问。
        // UserApp 代码路径独立于主线 (Web/Computer 走 create_container 共享 PVC)。
        let volumes = Some(vec![Volume {
            name: "app-workspace".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: self.app_workspace_pvc_name(app_id)?,
                read_only: Some(false),
            }),
            ..Default::default()
        }]);
        let volume_mounts = Some(vec![VolumeMount {
            name: "app-workspace".to_string(),
            mount_path: "/app".to_string(),
            sub_path: None,
            read_only: Some(false),
            ..Default::default()
        }]);

        let container = K8sContainer {
            name: APP_CONTAINER_NAME.to_string(),
            image: Some(image),
            image_pull_policy: Some("IfNotPresent".to_string()),
            // K8s:command 不设 → 用镜像 ENTRYPOINT(app-runtime 镜像 = start-app.sh,
            // 负责起 PG/pgweb/ttyd 后 exec 用户 command)。
            // args = 用户 command(等同 docker CMD 语义:有 ENTRYPOINT 时作其参数,
            // 无 ENTRYPOINT 时(如 node:20-alpine)docker 自动作命令运行)。
            // 这样 app-runtime 镜像的 ENTRYPOINT 生效跑内置服务,普通镜像用户 command 直接运行。
            command: None,
            args: params.command.clone(),
            env,
            env_from,
            ports: if ports.is_empty() { None } else { Some(ports) },
            resources,
            volume_mounts,
            liveness_probe: liveness,
            readiness_probe: readiness,
            ..Default::default()
        };

        let pod_spec = PodSpec {
            volumes,
            containers: vec![container],
            restart_policy: Some("Always".to_string()),
            ..Default::default()
        };

        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some(self.app_deployment_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(full_labels.clone()),
                // 记录每个端口的 expose_type（"port:type,..."），供读路径/重启重建还原——
                // 不依赖 NodePort，TCP 不对外时也能准确区分 Http/Tcp
                annotations: encode_port_expose_annotations(params),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                selector: LabelSelector {
                    match_labels: Some(selector_labels),
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(full_labels),
                        // env/secrets 改的是 ConfigMap/Secret 数据，env_from 引用名不变 →
                        // 不触发 rollout。此 annotation 让"内容变 → hash 变 → spec 变 → 自动
                        // rollout"，使 env 更新对运行中 Pod 生效（K8s 标准模式）。
                        annotations: Some(config_hash_annotations(params)),
                        ..Default::default()
                    }),
                    spec: Some(pod_spec),
                },
                ..Default::default()
            }),
            status: None,
        };
        Ok(deployment)
    }


    /// apply ClusterIP Service（暴露 app 端口）—— SSA create-or-update
    async fn apply_app_service(
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
        if ports.is_empty() {
            return Ok(());
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
        self.services_api()
            .patch(
                &self.app_service_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply app service: {e}")))?;
        Ok(())
    }


    /// apply HTTPRoute（HTTP 端口 → Gateway）—— SSA create-or-update，path prefix `/apps/{app_id}`
    ///
    /// `http_port.strip_prefix=true` 时加 URLRewrite filter（ReplacePrefixMatch），让 EG 把
    /// `/apps/{id}/api` → `/api` 再转发给后端（与 Docker Pingora 模式行为对齐：
    /// Docker `/proxy/{port}/api` → 后端收到 `/api`，天然 strip）。
    async fn apply_app_httproute(
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
        let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
        let api_resource = ApiResource::from_gvk(&gvk);
        let routes: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.namespace, &api_resource);

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

        routes
            .patch(
                &self.app_http_route_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(route),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply httproute: {e}")))?;
        Ok(())
    }


    /// apply NodePort Service（TCP 端口对外暴露）—— SSA create-or-update，
    /// 返回实际分配的 node_port 列表（server 分配，apply 后从返回对象读取）。
    /// 当前 TCP 初期不对外，此函数暂未被调用（保留供未来启用）。
    #[allow(dead_code)]
    async fn apply_app_nodeport(
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
        let created = self
            .services_api()
            .patch(
                &self.app_nodeport_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply nodeport: {e}")))?;

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


    /// apply Deployment（SSA create-or-update）。抽出供 create_app_resources 与
    /// patch_deployment（Phase 3）复用。
    async fn apply_app_deployment(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<()> {
        let deployment = self.build_app_deployment(app_id, params)?;
        let body = serde_json::to_value(&deployment)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize deployment: {e}")))?;
        self.deployments_api()
            .patch(
                &self.app_deployment_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply deployment: {e}")))?;
        Ok(())
    }


    /// 创建 UserApp 的全部 K8s 资源（SSA apply，幂等 create-or-update）：
    /// ConfigMap/Secret/Service/Deployment/HTTPRoute/NodePort。
    pub async fn create_app_resources(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
        gateway_name: Option<&str>,
        gateway_namespace: Option<&str>,
        http_expose: HttpExpose,
    ) -> ContainerRuntimeResult<Vec<AppPortStatus>> {
        let tenant_id = params.tenant_id.as_deref();
        let space_id = params.space_id.as_deref();
        // 0. workspace PVC: UserApp (K8s 永远 per-app) ensure per-app CephFS subvolume
        //    (配额由 requests.storage 经 CSI 服务端设, 绕开 client setfattr; PVC 默认保留,
        //    重建时 ensure "active" 分支复用; 销毁走 destroy_app_pvc)。create_app 流程已在
        //    app_manager ensure_app_workspace_ready 预 ensure + 等 subvolumePath 就绪, 这里命中 "active" 复用分支。
        self.ensure_workspace_pvc(app_id, &ServiceType::UserApp, params.storage_size.as_deref())
            .await?;
        // 1. ConfigMap（env）
        if let Some(env) = &params.env
            && !env.is_empty()
        {
            self.apply_app_configmap(app_id, env, tenant_id, space_id)
                .await?;
        }
        // 2. Secret（secrets）
        if let Some(secrets) = &params.secrets
            && !secrets.is_empty()
        {
            self.apply_app_secret(app_id, secrets, tenant_id, space_id)
                .await?;
        }
        // 3. Service（ClusterIP，所有端口；HTTPRoute 用它做 backendRef）
        self.apply_app_service(app_id, params).await?;
        // 4. Deployment（SSA apply）
        self.apply_app_deployment(app_id, params).await?;
        info!("[K8S-APP] Deployment applied for app: {app_id}");
        // 5. HTTP 入口 —— 按 http_expose：
        //    - Gateway 模式：apply HTTPRoute（path /apps/{id}），失败降级 warn 不阻塞
        //      （app 主体已创建不可回滚；避免重试 name 冲突）
        //    - Pingora 模式（默认）：不建 HTTPRoute（走 RCoder 内置 Pingora /proxy/{port}）
        //    两种模式都登记 HTTP 端口状态（external_port=None，保持返回结构；access 实际由 service.rs 从 request.ports / status.ports 生成）。
        let mut external_ports: Vec<AppPortStatus> = vec![];
        if let Some(ports) = params.ports.as_ref()
            && let Some(http_port) = ports.iter().find(|p| p.expose_type == ExposeType::Http)
        {
            if http_expose == HttpExpose::Gateway
                && let (Some(gw), Some(gw_ns)) = (gateway_name, gateway_namespace)
            {
                match self
                    .apply_app_httproute(app_id, http_port, gw, gw_ns, tenant_id, space_id)
                    .await
                {
                    Ok(_) => {
                        external_ports.push(AppPortStatus {
                            name: http_port.name.clone(),
                            port: http_port.port,
                            expose_type: ExposeType::Http,
                            external_port: None,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[K8S-APP] HTTPRoute apply 失败，app 主体已创建但 HTTP 入口暂不可用（待 Gateway/CRD 就绪后 reconcile）: {}",
                            e
                        );
                    }
                }
            } else {
                // Pingora 模式：不建 HTTPRoute（走 RCoder 内置 Pingora），仅登记端口状态保持返回结构
                external_ports.push(AppPortStatus {
                    name: http_port.name.clone(),
                    port: http_port.port,
                    expose_type: ExposeType::Http,
                    external_port: None,
                });
            }
        }
        // 6. TCP 端口：初期不对外（仅 ClusterIP 集群内访问，见步骤 3 apply_app_service）。
        //    apply_app_nodeport 保留供未来启用 TCP 对外暴露时调用。
        Ok(external_ports)
    }
}
