//! UserApp Deployment 创建路径(从 k8s_deployment.rs 拆出)。
//!
//! apply_app_configmap/secret/service/httproute/nodeport/deployment + build_app_deployment +
//! create_app_resources 编排。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    AppPortStatus, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    ExposeType, HttpExpose,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapEnvSource, Container as K8sContainer, ContainerPort, EnvFromSource, EnvVar,
    PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, SecretEnvSource, Volume,
    VolumeMount,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
#[cfg(feature = "kubernetes")]
use kube::api::Patch;
#[cfg(feature = "kubernetes")]
use tracing::info;

#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;

use super::k8s_app_helpers::{
    build_app_resource_requirements, build_hostname_spread_constraint, build_probe,
    config_hash_annotations, merge_app_annotations,
};
use super::k8s_deployment::{APP_CONTAINER_NAME, APP_NAME_LABEL_VALUE};
#[cfg(feature = "kubernetes")]
use super::k8s_pvc::K8sPvcOps;
use super::kubernetes_runtime::KubernetesRuntime;

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

        // 健康检查 probe:liveness 用 liveness_path(缺省回退 path),readiness 用 path。
        // 拆成两个语义不同的探针:liveness(进程活,不被后端 bug 杀)+ readiness(能服务,可摘流)。
        let (liveness, readiness) = params.health_check.as_ref().map_or((None, None), |hc| {
            (build_probe(hc, true), build_probe(hc, false))
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

        // 额外直接注入 APP_ID + 平台 env（压平挂载点绑定；直接 env 优先于
        // envFrom，覆盖 ConfigMap 用户值——start-app.sh 均为 ${VAR:-...} 覆盖模式，
        // 镜像缺省回退 /app 仅本地直跑语义）。
        let env = Some(vec![
            EnvVar {
                name: "APP_ID".to_string(),
                value: Some(app_id.to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "PGDATA".to_string(),
                value: Some(shared_types::paths::USERAPP_DEV_PGDATA.to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "DBX_DATA_DIR".to_string(),
                value: Some(shared_types::paths::USERAPP_DEV_DBX_DATA.to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "USERAPP_WORKSPACE_DIR".to_string(),
                value: Some(format!(
                    "{}/{}",
                    shared_types::paths::USERAPP_DEV_HOME,
                    app_id
                )),
                ..Default::default()
            },
        ]);

        // ── UserApp prod 单卷四 subPath 压平挂载（与 dev builder 完全同构）──────
        // per-app RWO RBD PVC 一块（卷内 `{app_id}/ + data/ + logs/ + agent-store/`
        // 四目录平级，subPath 目录由 kubelet 挂载时自动创建），四 subPath 挂到
        // 容器内 /home/user/{app_id}（workspace=发布代码根）、/home/user/data、
        // /home/user/logs、/home/user/.agent-store——段序与布局单一事实源
        // [`shared_types::paths::userapp_prod_subpaths`] 一一配对。
        // rcoder **不挂载**该卷（RBD 无 subvolumePath，挂根聚合天然不可达）——
        // 部署经 env 注入 APP_DEPLOY_URL 由 app-cli 启动段下载解压，文件操作经
        // 容器内 file-server-proxy (:60000)。UserApp 代码路径独立于主线
        // (Web/Computer 走 create_container 共享 PVC)。RWO 单 pod 独占
        // (Deployment replicas=1)；pod 重建需等 volume detach→attach（秒级，
        // K8s 自动处理）。
        let volumes = Some(vec![Volume {
            name: "app-workspace".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: self.app_workspace_pvc_name(app_id)?,
                read_only: Some(false),
            }),
            ..Default::default()
        }]);
        let volume_mounts = Some(
            app_flat_volume_mounts(app_id)
                .into_iter()
                .map(|(sub_path, mount_path)| VolumeMount {
                    name: "app-workspace".to_string(),
                    mount_path,
                    sub_path: Some(sub_path),
                    read_only: Some(false),
                    ..Default::default()
                })
                .collect(),
        );

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
            // topologySpreadConstraints：所有 UserApp 共享 label app.kubernetes.io/name=user-app
            // （build_app_labels 写入），按它分组可把【N 个不同 app 的 Deployment】跨节点摊开
            // （约束按 label 统计，跨 Deployment 生效）。单 Deployment replicas=1，组内无均衡
            // 意义，价值全在跨 app。ScheduleAnyway 绝不阻断用户 app 创建；存量 Deployment
            // 要等下次 SSA 更新触发 rollout，新 pod 才带约束（只影响新调度）。
            // 策略细节见 build_hostname_spread_constraint。
            topology_spread_constraints: Some(vec![build_hostname_spread_constraint(
                APP_NAME_LABEL_VALUE,
            )]),
            ..Default::default()
        };

        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some(self.app_deployment_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(full_labels.clone()),
                // Deployment metadata.annotations：port-expose + recycle 配置（SSA 单一事实源，
                // 供读路径/重启重建还原 expose_type 与回收策略）。
                annotations: merge_app_annotations(params),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                // RWO 块卷（RBD）单挂载：Recreate 先删旧 Pod 再建新 Pod，避免
                // RollingUpdate maxSurge=1 期间新旧 Pod 争抢同一块卷的
                // Multi-Attach 错误（单副本本就有停机窗口，语义不变）。
                strategy: Some(DeploymentStrategy {
                    type_: Some("Recreate".to_string()),
                    ..Default::default()
                }),
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
        // 0. workspace PVC: UserApp (K8s 永远 per-app) per-app RWO RBD 单卷——
        //    卷内四目录（{app_id}/ data/ logs/ agent-store/）经 subPath 挂载，
        //    subPath 目录由 kubelet 自动创建，故只 ensure 单块 PVC
        //    （历史第二块 `-data` PVC 已随单卷化退役；destroy 侧兜底回收存量）。
        //    销毁走 destroy_app_pvc。
        self.ensure_workspace_pvc(
            app_id,
            &ServiceType::UserApp,
            params.storage_size.as_deref(),
        )
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

/// prod 单卷四 subPath 压平挂载映射（卷内子目录 → 容器内路径），段序与
/// [`shared_types::paths::userapp_prod_subpaths`] 一一配对——与 dev builder
/// （k8s_agent_create UserAppBuilder 分支）完全同构；subPath 目录由 kubelet
/// 挂载时自动创建。
pub(crate) fn app_flat_volume_mounts(app_id: &str) -> [(String, String); 4] {
    [
        (
            app_id.to_string(),
            format!("{}/{}", shared_types::paths::USERAPP_DEV_HOME, app_id),
        ),
        (
            "data".to_string(),
            shared_types::paths::USERAPP_DEV_DATA.to_string(),
        ),
        (
            "logs".to_string(),
            shared_types::paths::USERAPP_DEV_LOGS.to_string(),
        ),
        (
            "agent-store".to_string(),
            shared_types::paths::USERAPP_DEV_AGENT_STORE.to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_flat_volume_mounts_mirror_dev_layout() {
        let mounts = app_flat_volume_mounts("a1");
        assert_eq!(
            mounts,
            [
                ("a1".to_string(), "/home/user/a1".to_string()),
                ("data".to_string(), "/home/user/data".to_string()),
                ("logs".to_string(), "/home/user/logs".to_string()),
                (
                    "agent-store".to_string(),
                    "/home/user/.agent-store".to_string()
                ),
            ]
        );
        // 段序与布局事实源配对：卷内 {app_id}/ 对应宿主树 workspace 段，
        // data/logs/agent-store 平级子目录对应宿主树三数据段的 app 子层
        let subs = shared_types::paths::userapp_prod_subpaths("u1", "a1");
        assert_eq!(subs[0], "prod/u1/a1");
        assert!(mounts[0].0 == "a1" && mounts[0].1.ends_with("/a1"));
        for (m, sub_suffix) in mounts
            .iter()
            .skip(1)
            .zip(["data/a1", "logs/a1", "agent-store/a1"])
        {
            assert!(
                sub_suffix.starts_with(&m.0),
                "卷内平级目录 {m:?} 应是宿主段 {sub_suffix} 的前缀"
            );
        }
    }
}
