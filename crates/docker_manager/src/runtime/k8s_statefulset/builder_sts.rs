use super::*;

impl KubernetesRuntime {
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
