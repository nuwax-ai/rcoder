use super::*;

impl KubernetesRuntime {
    /// 构造期望的 builder StatefulSet（ensure 与受控升级共用；注解/执行域
    /// 标签的单一事实源）。
    pub(crate) fn desired_builder_statefulset(
        &self,
        context: &shared_types::UserAppExecutionContext,
        pod_spec: PodSpec,
    ) -> ContainerRuntimeResult<StatefulSet> {
        let family = ServiceType::UserappBuilder;
        let mut desired = self.build_agent_statefulset(&context.app_id, &family, pod_spec, 1)?;
        desired
            .metadata
            .annotations
            .get_or_insert_default()
            .extend(context.resource_metadata());
        let desired_spec = desired.spec.as_mut().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("Builder StatefulSet spec missing".into())
        })?;
        desired_spec
            .template
            .metadata
            .get_or_insert_default()
            .annotations
            .get_or_insert_default()
            .extend(context.resource_metadata());
        // 执行域注解（recovery v2 plan §6.1）：Downward API 卷把它投放到
        // 固定只读位置；值取 PodSpec 内同一 env 构造（单一事实源）。
        if let Some(domain) = crate::runtime::k8s_native_domain::domain_env_of_pod_spec(
            &desired_spec
                .template
                .spec
                .as_ref()
                .cloned()
                .unwrap_or_default(),
        ) {
            desired_spec
                .template
                .metadata
                .get_or_insert_default()
                .annotations
                .get_or_insert_default()
                .insert(runtime_supervisor::domain::DOMAIN_LABEL.to_string(), domain);
        }
        Ok(desired)
    }

    /// Builder creation never repairs an ownership/configuration conflict by
    /// deleting a workload. A conflicting POST re-reads and validates the winner.
    pub(crate) async fn ensure_builder_statefulset(
        &self,
        context: &shared_types::UserAppExecutionContext,
        pod_spec: PodSpec,
    ) -> ContainerRuntimeResult<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let desired = self.desired_builder_statefulset(context, pod_spec)?;
        let family = ServiceType::UserappBuilder;
        let name = self.pod_name(&context.app_id, &family)?;
        let api = self.statefulsets();
        let existing = match api.get_opt(&name).await {
            Ok(Some(existing)) => existing,
            Ok(None) => match api.create(&PostParams::default(), &desired).await {
                Ok(_) => return Ok(()),
                Err(kube::Error::Api(status)) if status.code == 409 => {
                    api.get(&name).await.map_err(|error| {
                        crate::runtime::builder_completion::k8s_error(
                            format!("Read competing builder StatefulSet: {error}"),
                            error,
                        )
                    })?
                }
                Err(error) => {
                    return Err(crate::runtime::builder_completion::k8s_error(
                        format!("Create builder StatefulSet: {error}"),
                        error,
                    ));
                }
            },
            Err(error) => {
                return Err(crate::runtime::builder_completion::k8s_error(
                    format!("Read builder StatefulSet: {error}"),
                    error,
                ));
            }
        };
        match validate_builder_statefulset(&existing, &desired, context)? {
            BuilderTemplateCheck::Current => {}
            BuilderTemplateCheck::NeedsUpgrade => {
                // recovery v2 R7（RV07 强化）：存量 builder 的 managed-owner
                // 平台注入漂移走受控模板替换（见 replace_builder_state-
                // set_controlled）。升级不是每次应用 Restart 的内部兜底：
                // 仅此签名触发一次。替换以 replicas=1 的期望模板重建，
                // 直接返回——**不得**再以替换前捕获的 existing（旧 uid/RV）
                // 走 scale 收尾：对象已删除，前置条件必然 409，物理替换
                // 成功却被误判 Failed（k3s 实测 P1）。
                self.replace_builder_statefulset_controlled(context, &existing, desired)
                    .await?;
                return Ok(());
            }
            BuilderTemplateCheck::StaleHash => {
                // 内容等价但注解是旧算法值：纯 metadata patch 重写后复用。
                let desired_hash = desired
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|values| values.get(TEMPLATE_HASH_ANNOTATION))
                    .ok_or_else(|| {
                        ContainerRuntimeError::ConfigurationError(
                            "Desired builder template hash is missing".into(),
                        )
                    })?;
                self.heal_builder_template_hash(&existing, desired_hash)
                    .await?;
            }
        }
        self.scale_captured_statefulset(&existing, &family, 1).await
    }

    /// RV07：存量 builder 的受控模板替换。工作区 PVC 为独立对象（非 STS
    /// ownerReference 管辖），替换不触碰卷与数据。序列：
    /// 1. 捕获旧 STS 的 UID/resourceVersion 与全部旧 Pod 名；
    /// 2. 缩容 0（replace 携带旧 RV，并发漂移 409）；
    /// 3. 删除 STS，**带捕获 UID/RV 前置**——迟到的删除不得命中并发重建
    ///    出的同名新对象；
    /// 4. 等待 STS 对象消失**且捕获的旧 Pod 全部退出**（后台级联时 STS
    ///    可能先消失而旧 Pod 仍 Ready——只等对象会把旧 Pod 的 readiness
    ///    留给按名读取的后续步骤）；
    /// 5. 超时返回 Err（操作保持可恢复，下次调用幂等重走；**绝不**在旧
    ///    物理未确认退出时创建新对象）；
    /// 6. 以期望模板创建。
    pub(crate) async fn replace_builder_statefulset_controlled(
        &self,
        context: &shared_types::UserAppExecutionContext,
        existing: &StatefulSet,
        desired: StatefulSet,
    ) -> ContainerRuntimeResult<()> {
        let family = ServiceType::UserappBuilder;
        let name = self.pod_name(&context.app_id, &family)?;
        let api = self.statefulsets();
        let captured_uid = existing.metadata.uid.clone();
        let captured_rv = existing.metadata.resource_version.clone();
        let replicas = existing
            .spec
            .as_ref()
            .and_then(|spec| spec.replicas)
            .unwrap_or(1);
        let captured_pods: Vec<String> = (0..replicas)
            .map(|ordinal| format!("{name}-{ordinal}"))
            .collect();
        if replicas > 0 {
            let scaled = {
                let mut patched = existing.clone();
                if let Some(spec) = patched.spec.as_mut() {
                    spec.replicas = Some(0);
                }
                patched
            };
            api.replace(&name, &PostParams::default(), &scaled)
                .await
                .map_err(|error| {
                    crate::runtime::builder_completion::k8s_error(
                        format!("Scale old builder StatefulSet to 0: {error}"),
                        error,
                    )
                })?;
        }
        let mut delete_params = DeleteParams::default();
        if captured_uid.is_some() || captured_rv.is_some() {
            delete_params.preconditions = Some(kube::api::Preconditions {
                uid: captured_uid.clone(),
                resource_version: captured_rv.clone(),
            });
        }
        api.delete(&name, &delete_params).await.map_err(|error| {
            crate::runtime::builder_completion::k8s_error(
                format!("Delete superseded builder StatefulSet: {error}"),
                error,
            )
        })?;
        // 等待 STS 对象消失且捕获的旧 Pod 全部退出（PVC 独立保留）。
        let pods: Api<k8s_openapi::api::core::v1::Pod> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(ContainerRuntimeError::Conflict(format!(
                    "superseded builder StatefulSet {name} (uid {:?}) or its captured pods \
                         did not finish terminating within the upgrade budget; the operation \
                         stays recoverable and no replacement was created",
                    captured_uid.as_deref().unwrap_or("unknown")
                )));
            }
            let sts_gone = api
                .get_opt(&name)
                .await
                .map_err(|error| {
                    crate::runtime::builder_completion::k8s_error(
                        format!("Wait for builder StatefulSet removal: {error}"),
                        error,
                    )
                })?
                .is_none();
            let pods_gone = if captured_pods.is_empty() {
                true
            } else {
                let mut all_gone = true;
                for pod_name in &captured_pods {
                    match pods.get_opt(pod_name).await {
                        Ok(None) => {}
                        Ok(Some(pod)) => {
                            // Terminating pods still hold the physical slot.
                            if pod.metadata.deletion_timestamp.is_none() {
                                all_gone = false;
                                break;
                            }
                        }
                        Err(error) => {
                            return Err(crate::runtime::builder_completion::k8s_error(
                                format!("Wait for captured builder pod removal: {error}"),
                                error,
                            ));
                        }
                    }
                }
                all_gone
            };
            if sts_gone && pods_gone {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        api.create(&PostParams::default(), &desired)
            .await
            .map_err(|error| {
                crate::runtime::builder_completion::k8s_error(
                    format!("Recreate upgraded builder StatefulSet: {error}"),
                    error,
                )
            })?;
        tracing::info!(
            app = %context.app_id,
            "upgraded legacy builder StatefulSet to the managed-owner template \
             (workspace PVC preserved; captured pods confirmed exited)"
        );
        Ok(())
    }

    /// template-hash 注解自愈（纯 metadata，不触碰模板）：validate 已证明
    /// launch 内容（镜像/command/args/归一化 env/容器集合/workspace claim）
    /// 与期望等价，仅注解为哈希算法演进前的存量值——跨算法不可比，比对
    /// 必然 mismatch，会把内容一致的存量 STS 永久判成漂移（围栏/收束循环）。
    /// 重写后旧 STS 首次全量 ensure 即愈合；后续校验回到正常指纹门。
    async fn heal_builder_template_hash(
        &self,
        existing: &StatefulSet,
        desired_hash: &str,
    ) -> ContainerRuntimeResult<()> {
        let Some(name) = existing.metadata.name.as_deref() else {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Existing builder StatefulSet has no name".into(),
            ));
        };
        // Merge patch 只写单个注解键，其余注解由 merge 语义保留。并发模板
        // 变更与本次 heal 竞态是无害的：注解只是"当前算法记账"，内容校验
        // （镜像/command/args/env/容器集合）才是复用门——即使注解被短暂
        // 盖到已漂移的模板上，下一次 validate 的内容比对仍会精确拒绝。
        let mut heal_patch = serde_json::json!({
            "metadata": {"annotations": {TEMPLATE_HASH_ANNOTATION: desired_hash}}
        });
        // step-D 写面 fencing：heal 同样带 uid+RV 前置——接管后的迟到写 409。
        crate::runtime::k8s_runtime_helpers::inject_object_identity(
            &mut heal_patch,
            &existing.metadata,
        )?;
        if let Err(error) = self
            .statefulsets()
            .patch(name, &PatchParams::default(), &Patch::Merge(heal_patch))
            .await
        {
            return Err(match &error {
                kube::Error::Api(status) if status.code == 409 || status.code == 422 => {
                    ContainerRuntimeError::Conflict(
                        "Builder StatefulSet changed during template-hash heal".into(),
                    )
                }
                _ => ContainerRuntimeError::K8sError(format!(
                    "Heal builder template hash {name}: {error}"
                )),
            });
        }
        warn!(
            "[K8S-STS] {} template-hash healed to current algorithm (legacy annotation superseded)",
            name
        );
        Ok(())
    }
}
