use super::*;

impl KubernetesRuntime {
    pub(crate) fn build_app_deployment(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<Deployment> {
        params.validate_execution_context()?;
        if params.project_id.as_deref() != Some(app_id) {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Rendered UserApp differs from its admitted project identity".into(),
            ));
        }
        let image = params.image_override.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Userapp create_deployment requires image_override".to_string(),
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
                name: "RCODER_PHYSICAL_POD_UID".into(),
                value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                    field_ref: Some(k8s_openapi::api::core::v1::ObjectFieldSelector {
                        api_version: Some("v1".into()),
                        field_path: "metadata.uid".into(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            // 执行域身份（builder 同款）：instance 由上方 fieldRef 在 Pod 内解析，
            // volume 绑定 per-app 工作区 PVC 与压平挂载视图。缺此声明时 app-cli
            // 无法把 PVC 上的跨容器监督状态判为"前容器残留"→ 误判恢复式启动 →
            // 冷部署声明被剥（K8s 冷部署卡死的根因）。条件 replace 收敛存量。
            EnvVar {
                name: runtime_supervisor::domain::DOMAIN_ENV.to_string(),
                value: Some(super::super::k8s_native_domain::app_domain_env(
                    &self.config.execution_authority,
                    &self.app_workspace_pvc_name(app_id)?,
                    &app_flat_volume_mounts(app_id),
                )),
                ..Default::default()
            },
            EnvVar {
                name: "PROJECT_ID".into(),
                value: Some(app_id.into()),
                ..Default::default()
            },
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
            // R07：平台注入稳定状态根——app-cli 运行内核的操作记录/事件/
            // desired 持久化锚点（env 权威：热部署换代 source/.run 目录被
            // 轮换时同一锁域不漂移；缺省回退 workspace 卷根推导）。指向
            // per-app 卷内专属子目录（prod/userapp/state/{app_id}），跨
            // Pod 重建与热部署稳定。
            EnvVar {
                name: "APP_CLI_STATE_ROOT".to_string(),
                value: Some(format!(
                    "{}/{}/state/{}",
                    shared_types::paths::USERAPP_DEV_HOME,
                    app_id,
                    app_id
                )),
                ..Default::default()
            },
        ]);

        // ── Userapp prod 单卷四 subPath 压平挂载（与 dev builder 完全同构）──────
        // per-app RWO RBD PVC 一块（卷内 `{app_id}/ + data/ + logs/ + agent-store/`
        // 四目录平级，subPath 目录由 kubelet 挂载时自动创建），四 subPath 挂到
        // 容器内 /home/user/{app_id}（workspace=发布代码根）、/home/user/data、
        // /home/user/logs、/home/user/.agent-store——段序与布局单一事实源
        // [`shared_types::paths::userapp_prod_subpaths`] 一一配对。
        // rcoder **不挂载**该卷（RBD 无 subvolumePath，挂根聚合天然不可达）——
        // 部署经 env 注入 APP_DEPLOY_URL 由 app-cli 启动段下载解压，文件操作经
        // 容器内 file-server-proxy (:60000)。Userapp 代码路径独立于主线
        // (Web/Computer 走 create_container 共享 PVC)。RWO 单 pod 独占
        // (Deployment replicas=1)；pod 重建需等 volume detach→attach（秒级，
        // K8s 自动处理）。
        let mut volumes_vec = vec![Volume {
            name: "app-workspace".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: self.app_workspace_pvc_name(app_id)?,
                read_only: Some(false),
            }),
            ..Default::default()
        }];
        // 固定只读平台绑定（recovery v2 plan §6.1）：builder 同款 Downward API
        // 投放（pod UID + 执行域注解）。
        volumes_vec.push(crate::runtime::k8s_native_domain::platform_binding_volume());
        let volumes = Some(volumes_vec);
        let mut mounts_vec: Vec<VolumeMount> = app_flat_volume_mounts(app_id)
            .into_iter()
            .map(|(sub_path, mount_path)| VolumeMount {
                name: "app-workspace".to_string(),
                mount_path,
                sub_path: Some(sub_path),
                read_only: Some(false),
                ..Default::default()
            })
            .collect();
        mounts_vec.push(VolumeMount {
            name: "rcoder-platform-binding".to_string(),
            mount_path: crate::runtime::k8s_native_domain::PLATFORM_BINDING_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
        let volume_mounts = Some(mounts_vec);

        let container = K8sContainer {
            name: APP_CONTAINER_NAME.to_string(),
            image: Some(image),
            image_pull_policy: Some("IfNotPresent".to_string()),
            // K8s:command 不设 → 用镜像 ENTRYPOINT(app-runtime 镜像 = start-app.sh,
            // 负责起 PG/dbx/ttyd 后 exec 用户 command)。
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
            // topologySpreadConstraints：所有 Userapp 共享 label app.kubernetes.io/name=user-app
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

        let mut annotations = merge_app_annotations(params).unwrap_or_default();
        if let Some(context) = &params.execution_context {
            context
                .validate_identity(app_id)
                .map_err(ContainerRuntimeError::ConfigurationError)?;
            annotations.extend(context.resource_metadata());
        }
        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some(self.app_deployment_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(full_labels.clone()),
                // Deployment metadata.annotations：port-expose + recycle 配置（SSA 单一事实源，
                // 供读路径/重启重建还原 expose_type 与回收策略）。
                annotations: Some(annotations),
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
                        // deploy-template-token：本次操作的写入者身份（operation_id）——
                        // 部署故障观察据此核验"观察到的 Pod 属于本次操作实际写入的模板"
                        // （同 UID 下旧 ReplicaSet 的 Pod 不携带新令牌）。
                        annotations: Some({
                            let mut ann = config_hash_annotations(params);
                            if let Some(context) = &params.execution_context {
                                ann.insert(
                                    crate::runtime::k8s_app_helpers::DEPLOY_TEMPLATE_TOKEN_ANNOTATION
                                        .to_string(),
                                    context.operation_id.clone(),
                                );
                            }
                            // 执行域注解（recovery v2 plan §6.1）：与 env 同源
                            // 构造，经 Downward API 投放到固定只读位置。
                            if let Some(domain) =
                                crate::runtime::k8s_native_domain::domain_env_of_pod_spec(&pod_spec)
                            {
                                ann.insert(
                                    runtime_supervisor::domain::DOMAIN_LABEL.to_string(),
                                    domain,
                                );
                            }
                            ann
                        }),
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

    /// Stage operation-owned configuration, then conditionally commit the Deployment.
    /// Configuration names are unique: losing writers cannot alter the running generation.
    pub(crate) async fn write_app_generation(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
        expected: Option<&str>,
        target: Option<&shared_types::AppResourceIdentity>,
    ) -> ContainerRuntimeResult<()> {
        use k8s_openapi::api::core::v1::Secret;
        use kube::api::{DeleteParams, PostParams, Preconditions};
        params.validate_execution_context()?;
        if params.execution_context.is_some() && expected.is_some() && target.is_none() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Conditional lifecycle update requires a captured target".into(),
            ));
        }
        if let Some(target) = target
            && (target.kind != shared_types::AppResourceKind::Deployment
                || target.uid.is_empty()
                || target.name != self.app_deployment_name(app_id)
                || target.resource_version.as_deref() != expected)
        {
            return Err(ContainerRuntimeError::Conflict(
                "Captured update target does not match requested commit".into(),
            ));
        }
        let operation = uuid::Uuid::new_v4().simple().to_string();
        let stem = format!("ua-{app_id}-{}", &operation[..16]);
        let cm_name = format!("{stem}-env");
        let secret_name = format!("{stem}-sec");
        let mut labels = self.build_app_labels(
            app_id,
            params.tenant_id.as_deref(),
            params.space_id.as_deref(),
        );
        labels.insert("rcoder.io/creation-operation".into(), operation);
        let metadata = |name: String| ObjectMeta {
            name: Some(name),
            namespace: Some(self.namespace.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        };
        let cm = self
            .configmaps_api()
            .create(
                &PostParams::default(),
                &ConfigMap {
                    metadata: metadata(cm_name.clone()),
                    data: Some(params.env.clone().unwrap_or_default().into_iter().collect()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("stage generation env: {e}")))?;
        let cm_uid = cm.metadata.uid.clone().ok_or_else(|| {
            ContainerRuntimeError::K8sError("created configmap missing UID".into())
        })?;
        let mut secret_uid = None;
        let mut compensation_safe = true;
        let result = async {
            let secret = self
                .secrets_api()
                .create(
                    &PostParams::default(),
                    &Secret {
                        metadata: metadata(secret_name.clone()),
                        string_data: Some(
                            params
                                .secrets
                                .clone()
                                .unwrap_or_default()
                                .into_iter()
                                .collect(),
                        ),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| {
                    ContainerRuntimeError::K8sError(format!("stage generation secret: {e}"))
                })?;
            secret_uid = secret.metadata.uid;
            if secret_uid.is_none() {
                return Err(ContainerRuntimeError::K8sError(
                    "created secret missing UID".into(),
                ));
            }
            let mut deployment = self.build_app_deployment(app_id, params)?;
            deployment.metadata.resource_version = expected.map(str::to_owned);
            deployment.metadata.uid = target.as_ref().map(|resource| resource.uid.clone());
            let container = deployment
                .spec
                .as_mut()
                .and_then(|s| s.template.spec.as_mut())
                .and_then(|p| p.containers.first_mut())
                .ok_or_else(|| {
                    ContainerRuntimeError::ConfigurationError(
                        "deployment missing app container".into(),
                    )
                })?;
            container.env_from = Some(vec![
                EnvFromSource {
                    config_map_ref: Some(ConfigMapEnvSource {
                        name: cm_name.clone(),
                        optional: Some(false),
                    }),
                    ..Default::default()
                },
                EnvFromSource {
                    secret_ref: Some(SecretEnvSource {
                        name: secret_name.clone(),
                        optional: Some(false),
                    }),
                    ..Default::default()
                },
            ]);
            let api = self.deployments_api();
            compensation_safe = false;
            let commit = if expected.is_some() {
                api.replace(
                    &self.app_deployment_name(app_id),
                    &PostParams::default(),
                    &deployment,
                )
                .await
            } else {
                api.create(&PostParams::default(), &deployment).await
            };
            // A lost response may hide a successful commit. Retain its referenced
            // configuration unless the API definitively rejected the write.
            if let Err(kube::Error::Api(error)) = &commit {
                compensation_safe = (400..500).contains(&error.code) && error.code != 408;
            }
            commit.map_err(|e| match e {
                kube::Error::Api(ref error) if error.code == 409 => {
                    ContainerRuntimeError::Conflict(format!("conditional deployment commit: {e}"))
                }
                _ => ContainerRuntimeError::K8sError(format!("conditional deployment commit: {e}")),
            })?;
            Ok(())
        }
        .await;
        if result.is_err() && compensation_safe {
            // Only successful create receipts authorize compensation. No PVC/service/deployment deletion.
            let dp = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(cm_uid),
                    resource_version: None,
                }),
                ..Default::default()
            };
            if let Err(error) = self.configmaps_api().delete(&cm_name, &dp).await {
                tracing::warn!("cleanup owned generation env failed: {error}");
            }
            if let Some(uid) = secret_uid {
                let dp = DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(uid),
                        resource_version: None,
                    }),
                    ..Default::default()
                };
                if let Err(error) = self.secrets_api().delete(&secret_name, &dp).await {
                    tracing::warn!("cleanup owned generation secret failed: {error}");
                }
            }
        }
        if result.is_ok() {
            // 换代提交成功：回收本 app 被取代的历史配置代（含历史失败 staging 残留）。
            // 同 app 发布在操作锁内串行，不存在并发 staging 交错。
            self.reclaim_superseded_app_config(app_id, &cm_name, &secret_name)
                .await;
        }
        result
    }

    /// 换代提交成功后回收被取代的配置代：按 app label（与 capture_deletion 同源
    /// 选择器）list env ConfigMap / secret，保留本次提交的活跃引用名，其余
    /// [`is_superseded_generation`] 命中的换代 staging 对象按 UID 前置条件删除。
    /// 回收失败不影响发布正确性（活跃配置已生效）——warn 记录，残留由下次发布
    /// （幂等）或销毁路径（capture_deletion 全量捕获）回收，非吞业务错。
    pub(crate) async fn reclaim_superseded_app_config(
        &self,
        app_id: &str,
        active_cm: &str,
        active_secret: &str,
    ) {
        use kube::api::{DeleteParams, ListParams, Preconditions};

        let selector = format!(
            "app.kubernetes.io/instance={app_id},app.kubernetes.io/managed-by=rcoder-app-manager"
        );
        let params = ListParams::default().labels(&selector);
        match self.configmaps_api().list(&params).await {
            Ok(list) => {
                for cm in list.items {
                    let Some(name) = cm.metadata.name.clone() else {
                        continue;
                    };
                    if !is_superseded_generation(app_id, active_cm, &name) {
                        continue;
                    }
                    let Some(uid) = cm.metadata.uid.clone() else {
                        tracing::warn!("skip superseded generation env {name}: missing UID");
                        continue;
                    };
                    let dp = DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: Some(uid),
                            resource_version: None,
                        }),
                        ..Default::default()
                    };
                    if let Err(error) = self.configmaps_api().delete(&name, &dp).await {
                        tracing::warn!("reclaim superseded generation env {name} failed: {error}");
                    }
                }
            }
            Err(error) => {
                tracing::warn!("list superseded generation env configmaps failed: {error}");
            }
        }
        match self.secrets_api().list(&params).await {
            Ok(list) => {
                for secret in list.items {
                    let Some(name) = secret.metadata.name.clone() else {
                        continue;
                    };
                    if !is_superseded_generation(app_id, active_secret, &name) {
                        continue;
                    }
                    let Some(uid) = secret.metadata.uid.clone() else {
                        tracing::warn!("skip superseded generation secret {name}: missing UID");
                        continue;
                    };
                    let dp = DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: Some(uid),
                            resource_version: None,
                        }),
                        ..Default::default()
                    };
                    if let Err(error) = self.secrets_api().delete(&name, &dp).await {
                        tracing::warn!(
                            "reclaim superseded generation secret {name} failed: {error}"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!("list superseded generation secrets failed: {error}");
            }
        }
    }

    /// Create UserApp resources using an exclusive Deployment create:
    /// ConfigMap/Secret/Service/Deployment/HTTPRoute/NodePort。
    pub async fn create_app_resources(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
        gateway_name: Option<&str>,
        gateway_namespace: Option<&str>,
        http_expose: HttpExpose,
    ) -> ContainerRuntimeResult<Vec<AppPortStatus>> {
        self.write_app_resources(
            app_id,
            params,
            gateway_name,
            gateway_namespace,
            http_expose,
            None,
        )
        .await
    }

    pub(crate) async fn write_app_resources(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
        gateway_name: Option<&str>,
        gateway_namespace: Option<&str>,
        http_expose: HttpExpose,
        expected: Option<&str>,
    ) -> ContainerRuntimeResult<Vec<AppPortStatus>> {
        params.validate_execution_context()?;
        // 阶段进度在编排层逐段累计（进程内事实，非远端观察）：失败时包成
        // CreationAborted 携带「保留资源清单 + 拒绝确定性」上抛，供上层
        // 生成整操作级安全结束证明（见 CreationProgress::safe_finish_ok）。
        let mut progress = container_runtime_api::CreationProgress::default();
        let aborted = |stage: container_runtime_api::CreationStage,
                       progress: &mut container_runtime_api::CreationProgress,
                       source: ContainerRuntimeError| {
            progress.failed_at = stage;
            progress.definitive_rejection = source.is_definitive_rejection();
            ContainerRuntimeError::CreationAborted {
                progress: progress.clone(),
                source: Box::new(source),
            }
        };
        let target = if let Some(target) = &params.mutation_target {
            let resource = &target.resource;
            if resource.kind != shared_types::AppResourceKind::Deployment
                || resource.name != self.app_deployment_name(app_id)
                || resource.resource_version.as_deref() != expected
            {
                progress.failed_at = container_runtime_api::CreationStage::Capture;
                progress.definitive_rejection = true;
                return Err(ContainerRuntimeError::CreationAborted {
                    progress,
                    source: Box::new(ContainerRuntimeError::Conflict(
                        "Captured application update target changed".into(),
                    )),
                });
            }
            Some(resource.clone())
        } else {
            match (&params.execution_context, expected) {
                (Some(context), Some(version)) => match self
                    .capture_owned_app_identity(context, Some(version))
                    .await
                {
                    Ok(captured) => Some(captured),
                    Err(error) => {
                        return Err(aborted(
                            container_runtime_api::CreationStage::Capture,
                            &mut progress,
                            error,
                        ));
                    }
                },
                _ => None,
            }
        };
        let tenant_id = params.tenant_id.as_deref();
        let space_id = params.space_id.as_deref();
        // 0. workspace PVC: Userapp (K8s 永远 per-app) per-app RWO RBD 单卷——
        //    卷内四目录（{app_id}/ data/ logs/ agent-store/）经 subPath 挂载，
        //    subPath 目录由 kubelet 自动创建，故只 ensure 单块 PVC
        //    （历史第二块 `-data` PVC 已随单卷化退役；destroy 侧兜底回收存量）。
        //    销毁走 destroy_app_pvc。
        let ensured_pvc = self.workspace_pvc_name(app_id, &ServiceType::Userapp)?;
        let ensure_result = if let Some(context) = &params.execution_context {
            self.ensure_owned_workspace_pvc(
                context,
                &ServiceType::Userapp,
                params.storage_size.as_deref(),
            )
            .await
        } else {
            self.ensure_workspace_pvc(
                app_id,
                &ServiceType::Userapp,
                params.storage_size.as_deref(),
            )
            .await
        };
        if let Err(error) = ensure_result {
            return Err(aborted(
                container_runtime_api::CreationStage::PvcEnsure,
                &mut progress,
                error,
            ));
        }
        progress
            .retained_idempotent_resources
            .push(format!("workspace pvc ensured: {ensured_pvc}"));
        if let Err(error) = self
            .claim_app_storage_with_context(app_id, params.execution_context.as_ref())
            .await
        {
            // claim 逐 PVC 顺序执行：失败时无法从外部得知已成功标注了几个——
            // 如实记录注解可能残留，不冒充零变更。
            progress
                .retained_idempotent_resources
                .push("storage-claim annotations may persist on application PVCs".into());
            return Err(aborted(
                container_runtime_api::CreationStage::StorageClaim,
                &mut progress,
                error,
            ));
        }
        progress
            .retained_idempotent_resources
            .push("storage claim annotations applied".into());
        if let Err(error) = self
            .write_app_generation(app_id, params, expected, target.as_ref())
            .await
        {
            return Err(aborted(
                container_runtime_api::CreationStage::WriteGeneration,
                &mut progress,
                error,
            ));
        }
        // Publish networking only after the conditional commit succeeded.
        if let Err(error) = self.apply_app_service(app_id, params).await {
            return Err(aborted(
                container_runtime_api::CreationStage::ApplyService,
                &mut progress,
                error,
            ));
        }
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
/// （k8s_agent_create UserappBuilder 分支）完全同构；subPath 目录由 kubelet
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

/// 判定配置对象名是否为本 app 已被取代的换代 staging 代：`ua-{app_id}-` 前缀
/// 且非当前活跃引用。仅匹配换代 staging 命名（前缀含结尾 `-`，app id 10 与
/// 104 无前缀碰撞），不触碰其它来源的配置对象（如 apply 路径的固定名）。
pub(super) fn is_superseded_generation(app_id: &str, active: &str, name: &str) -> bool {
    name != active && name.starts_with(&format!("ua-{app_id}-"))
}
