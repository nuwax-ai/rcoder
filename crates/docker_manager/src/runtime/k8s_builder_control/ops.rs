use super::*;

impl KubernetesRuntime {
    pub(crate) async fn capture_builder_volume_witness(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Vec<AppResourceIdentity>> {
        target.validate().map_err(Error::ConfigurationError)?;
        let workload = target
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Builder volume workload missing".into()))?;
        if workload.kind != AppResourceKind::StatefulSet {
            return Err(Error::ConfigurationError(
                "Builder volume witness requires StatefulSet".into(),
            ));
        }
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Capture builder workspace volumes", error))?;
        verify_workload_stable(
            &current,
            &target.context,
            target.resource_binding.as_ref(),
            workload,
            None,
        )
        .map_err(Error::Conflict)?;
        if current.metadata.resource_version != workload.resource_version
            || current.metadata.deletion_timestamp.is_some()
        {
            return Err(Error::Conflict(
                "Builder changed before volume capture".into(),
            ));
        }
        let spec = current
            .spec
            .ok_or_else(|| Error::Conflict("Builder spec missing during volume capture".into()))?;
        if spec.replicas.is_some_and(|count| count > 1) {
            return Err(Error::Conflict(
                "Multiple builder ordinals require explicit volume recovery".into(),
            ));
        }
        let ordinal = spec
            .ordinals
            .as_ref()
            .and_then(|value| value.start)
            .unwrap_or(0);
        let names = spec
            .volume_claim_templates
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|claim| {
                claim
                    .metadata
                    .name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .map(|name| format!("{name}-{}-{ordinal}", workload.name))
                    .ok_or_else(|| Error::Conflict("Builder volume template name missing".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        let pod = spec.template.spec.as_ref().ok_or_else(|| {
            Error::Conflict("Builder Pod spec missing during volume capture".into())
        })?;
        self.discover_volume_identities(&target.context, pod, names)
            .await
    }

    pub(crate) async fn prepare_builder_conditional_retry(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<BuilderControlTarget>> {
        target.validate().map_err(Error::ConfigurationError)?;
        let workload = target
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Original builder target missing".into()))?;
        if workload.kind != AppResourceKind::StatefulSet {
            return Err(Error::ConfigurationError(
                "Builder retry requires a StatefulSet".into(),
            ));
        }
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let before = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Observe original builder startup", error))?;
        verify_workload_stable(
            &before,
            &target.context,
            target.resource_binding.as_ref(),
            workload,
            None,
        )
        .map_err(Error::Conflict)?;
        let applied = before
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("rcoder.io/compute-start-receipt"))
            .map(|value| serde_json::from_str::<UserAppExecutionContext>(value))
            .transpose()
            .map_err(|error| {
                Error::ConfigurationError(format!(
                    "Decode original builder startup receipt: {error}"
                ))
            })?;
        if applied.as_ref() == Some(&target.context) {
            return Ok(None);
        }
        if !self.reconcile_builder_stop_receipt(target).await? {
            return Err(Error::Conflict(
                "Original builder stop is not confirmed".into(),
            ));
        }
        let fresh = self
            .capture_builder_compute_with_binding(
                &target.context,
                target.resource_binding.as_ref(),
                false,
            )
            .await?;
        let current = fresh.workload.as_ref().ok_or_else(|| {
            Error::Conflict("Builder disappeared during retry preparation".into())
        })?;
        // Keep the version read BEFORE stop observation. A concurrent original
        // startup invalidates this witness instead of being silently rebased.
        if fresh.pod.is_some()
            || current.uid != workload.uid
            || current.name != workload.name
            || before
                .metadata
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
            || current.resource_version != before.metadata.resource_version
        {
            return Err(Error::Conflict(
                "Builder changed while preparing conditional retry".into(),
            ));
        }
        let mut fresh = fresh;
        fresh.restart_image = target.restart_image.clone();
        Ok(Some(fresh))
    }

    pub(crate) async fn fence_builder_conditional_write(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<bool> {
        target.validate().map_err(Error::ConfigurationError)?;
        let workload = target
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Builder fence target missing".into()))?;
        if workload.kind != AppResourceKind::StatefulSet {
            return Ok(false);
        }
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Read builder compute fence", error))?;
        verify_workload_stable(
            &current,
            &target.context,
            target.resource_binding.as_ref(),
            workload,
            None,
        )
        .map_err(Error::Conflict)?;
        if current
            .metadata
            .resource_version
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Err(Error::Conflict("Builder fence version missing".into()));
        }
        if current.metadata.resource_version != workload.resource_version {
            return Ok(true);
        }
        let marker = serde_json::to_string(&target.context)
            .map_err(|error| Error::ConfigurationError(format!("Encode builder fence: {error}")))?;
        api.patch(
            &workload.name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata":{"uid":workload.uid,"resourceVersion":workload.resource_version,
                    "annotations":{"rcoder.io/compute-write-fence":marker}}
            })),
        )
        .await
        .map_err(|error| api_error("Fence builder compute write", error))?;
        let after = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Recheck builder compute fence", error))?;
        Ok(after.metadata.uid.as_deref() == Some(workload.uid.as_str())
            && after
                .metadata
                .resource_version
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            && after.metadata.resource_version != workload.resource_version)
    }

    pub(crate) async fn reconcile_builder_start_receipt(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<ContainerBasicInfo>> {
        target.validate().map_err(Error::ConfigurationError)?;
        let Some(workload) = target.workload.as_ref() else {
            return Ok(None);
        };
        if workload.kind != AppResourceKind::StatefulSet {
            return Ok(None);
        }
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Read builder start receipt", error))?;
        verify_workload_stable(
            &current,
            &target.context,
            target.resource_binding.as_ref(),
            workload,
            Some(1),
        )
        .map_err(Error::Conflict)?;
        let receipt = current
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("rcoder.io/compute-start-receipt"))
            .map(|value| serde_json::from_str::<UserAppExecutionContext>(value))
            .transpose()
            .map_err(|error| {
                Error::ConfigurationError(format!("Decode builder start receipt: {error}"))
            })?;
        if receipt.as_ref() != Some(&target.context)
            || current.metadata.deletion_timestamp.is_some()
            || !statefulset_agent_image_matches(&current, target.restart_image.as_deref())
        {
            return Ok(None);
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = self.agent_pod_name(&target.context.app_id, &ServiceType::UserappBuilder)?;
        let Some(pod) = pods
            .get_opt(&name)
            .await
            .map_err(|error| api_error("Read restarted builder Pod", error))?
        else {
            return Ok(None);
        };
        if pod.metadata.deletion_timestamp.is_some()
            || !builder_agent_running(&pod)
            || !pod_agent_image_matches(&pod, target.restart_image.as_deref())
        {
            return Ok(None);
        }
        let Some(status) = current.status.as_ref() else {
            return Ok(None);
        };
        if status
            .observed_generation
            .zip(current.metadata.generation)
            .is_none_or(|(observed, desired)| observed < desired)
            || status.replicas < 1
        {
            return Ok(None);
        }
        if let Some(revision) = status.update_revision.as_ref()
            && pod
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("controller-revision-hash"))
                != Some(revision)
        {
            return Ok(None);
        }
        let BuilderObservation::Ready(ready) = ready_builder_info(
            &pod,
            workload,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )
        .map_err(Error::Conflict)?
        else {
            return Ok(None);
        };
        let (info, identity) = *ready;
        let after_pod = pods
            .get(&name)
            .await
            .map_err(|error| api_error("Recheck restarted builder Pod", error))?;
        if pod_identity(&after_pod, workload).map_err(|error| Error::Conflict(error.to_string()))?
            != identity
            || after_pod.metadata.deletion_timestamp.is_some()
        {
            return Ok(None);
        }
        let after = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Recheck builder start receipt", error))?;
        let after_receipt = after
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get("rcoder.io/compute-start-receipt"))
            .map(|value| serde_json::from_str::<UserAppExecutionContext>(value))
            .transpose()
            .map_err(|error| {
                Error::ConfigurationError(format!(
                    "Decode rechecked builder start receipt: {error}"
                ))
            })?;
        if after.metadata.uid != current.metadata.uid
            || after_receipt.as_ref() != Some(&target.context)
            || after.spec.as_ref().and_then(|spec| spec.replicas) != Some(1)
            || !statefulset_agent_image_matches(&after, target.restart_image.as_deref())
            || !pod_agent_image_matches(&after_pod, target.restart_image.as_deref())
        {
            return Ok(None);
        }
        Ok(Some(info))
    }

    pub(crate) async fn reconcile_builder_stop_receipt(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<bool> {
        target.validate().map_err(Error::ConfigurationError)?;
        let Some(workload) = target.workload.as_ref() else {
            return Ok(false);
        };
        if workload.kind != AppResourceKind::StatefulSet {
            return Ok(false);
        }
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let confirm = |current: &StatefulSet| -> Result<bool> {
            verify_workload_stable(
                current,
                &target.context,
                target.resource_binding.as_ref(),
                workload,
                Some(0),
            )
            .map_err(Error::Conflict)?;
            let receipt = current
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("rcoder.io/compute-stop-receipt"))
                .map(|value| serde_json::from_str::<UserAppExecutionContext>(value))
                .transpose()
                .map_err(|error| {
                    Error::ConfigurationError(format!("Decode builder stop receipt: {error}"))
                })?;
            Ok(receipt.as_ref() == Some(&target.context)
                && current.metadata.deletion_timestamp.is_none())
        };
        let current = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Read builder stop receipt", error))?;
        if !confirm(&current)? {
            return Ok(false);
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let listed = pods
            .list(&kube::api::ListParams::default().labels(&format!(
                "rcoder.io/identifier={},rcoder.io/service-type={}",
                target.context.app_id,
                ServiceType::UserappBuilder
            )))
            .await
            .map_err(|error| api_error("Observe stopped builder Pods", error))?;
        if !listed.items.is_empty() {
            return Ok(false);
        }
        let after = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Recheck builder stop receipt", error))?;
        confirm(&after)
    }

    pub(crate) async fn exec_bound_builder(
        &self,
        target: &BuilderControlTarget,
        command: Vec<String>,
    ) -> Result<container_runtime_api::ExecResult> {
        target.validate().map_err(Error::Conflict)?;
        let expected = target
            .pod
            .as_ref()
            .ok_or_else(|| Error::Conflict("Captured builder Pod is absent".into()))?;
        if command.is_empty() {
            return Err(Error::ConfigurationError(
                "Builder exec command is empty".into(),
            ));
        }
        let actual = self
            .capture_builder_compute_with_binding(
                &target.context,
                target.resource_binding.as_ref(),
                false,
            )
            .await?;
        if actual.workload.as_ref().map(|v| (&v.uid, &v.name))
            != target.workload.as_ref().map(|v| (&v.uid, &v.name))
            || actual.pod.as_ref().map(|v| (&v.uid, &v.name))
                != Some((&expected.uid, &expected.name))
        {
            return Err(Error::Conflict(
                "Captured builder exec identity changed".into(),
            ));
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod = pods
            .get(&expected.name)
            .await
            .map_err(|error| api_error("Inspect builder exec Pod", error))?;
        if pod.metadata.uid.as_deref() != Some(expected.uid.as_str())
            || pod.metadata.deletion_timestamp.is_some()
        {
            return Err(Error::Conflict("Builder Pod changed before exec".into()));
        }
        let container = pod
            .spec
            .as_ref()
            .and_then(|spec| spec.containers.iter().find(|v| v.name == "agent"))
            .ok_or_else(|| Error::Conflict("Builder agent container is missing".into()))?;
        let identities: Vec<_> = container
            .env
            .iter()
            .flatten()
            .filter(|v| v.name == "RCODER_PHYSICAL_POD_UID")
            .collect();
        if identities.len() != 1
            || identities[0].value.is_some()
            || !identities[0]
                .value_from
                .as_ref()
                .and_then(|v| v.field_ref.as_ref())
                .is_some_and(|v| v.field_path == "metadata.uid")
        {
            return Err(Error::Conflict(
                "Builder lacks its physical Pod identity; recreate before management writes".into(),
            ));
        }
        self.exec_pod_container(
            &expected.name,
            "agent",
            builder_exec_guard(&expected.uid, command),
        )
        .await
    }

    /// RV07：返回值第二项 = 本次操作内执行了受控物理替换时的新物理绑定
    ///（新 STS UID）——调用方（agent_runtime 收据路径）必须用它取代旧
    /// 绑定做最终捕获，否则升级成功后收据仍按旧 UID 校验而失败。
    pub(crate) async fn resume_bound_builder(
        &self,
        params: &container_runtime_api::ContainerCreateParams,
    ) -> Result<(
        ContainerBasicInfo,
        Option<shared_types::UserAppResourceBinding>,
    )> {
        let mut upgrade_attempted = false;
        let (target, upgraded_binding) = async {
            let context = params.execution_context.as_ref().ok_or_else(|| {
                Error::ConfigurationError("Bound builder requires execution context".into())
            })?;
            let binding = params.resource_binding.as_ref().ok_or_else(|| {
                Error::ConfigurationError("Bound builder requires durable resource proof".into())
            })?;
            let target = self
                .capture_builder_compute_with_binding(context, Some(binding), false)
                .await?;
            let workload = target.workload.as_ref().ok_or_else(|| {
                Error::Conflict(
                    "Bound builder disappeared; automatic replacement is forbidden".into(),
                )
            })?;
            let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
            let current = api
                .get(&workload.name)
                .await
                .map_err(|error| api_error("Inspect bound builder configuration", error))?;
            if workload_identity_with_binding(&current, context, Some(binding), false)? != *workload
            {
                return Err(Error::Conflict(
                    "Bound builder changed during configuration validation".into(),
                ));
            }
            let desired =
                self.build_agent_pod_spec(&context.app_id, &ServiceType::UserappBuilder, params)?;
            let actual = current
                .spec
                .as_ref()
                .and_then(|spec| spec.template.spec.as_ref())
                .ok_or_else(|| {
                    Error::ConfigurationError("Bound builder Pod template missing".into())
                })?;
            if !configured_fields_match(
                &serde_json::to_value(&desired)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?,
                &serde_json::to_value(actual)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?,
            ) {
                // RV07：配置漂移不再一律 Conflict 围栏——managed-owner 升级
                // 签名内的漂移（APP_CLI_MANAGED 三键）在**本持久操作内**
                // 执行受控替换（缩容→前置条件删除→确认旧 Pod 退出→重建；
                // PVC 独立保留），然后按创建等价身份捕获新物理负载并交出
                // 新绑定。任何其他漂移仍拒绝（不盲删存量）。
                let desired_sts = self
                    .desired_builder_statefulset(context, desired)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?;
                match crate::runtime::k8s_statefulset::helpers::validate_builder_statefulset(
                    &current,
                    &desired_sts,
                    context,
                )
                .map_err(|error| Error::Conflict(error.to_string()))?
                {
                    crate::runtime::k8s_statefulset::helpers::BuilderTemplateCheck::NeedsUpgrade => {
                        upgrade_attempted = true;
                        self.replace_builder_statefulset_controlled(context, &current, desired_sts)
                            .await
                            .map_err(|error| Error::Conflict(error.to_string()))?;
                        // 新物理身份按创建等价校验捕获：替换后的 STS 带完整
                        // 应用/lifecycle 元数据注解，无需旧 UID 绑定背书
                        //（与全新创建同一信任级别，不是"任意通过"）。
                        let new_target = self
                            .capture_builder_compute_with_binding(context, None, true)
                            .await?;
                        let new_workload = new_target.workload.as_ref().ok_or_else(|| {
                            Error::Conflict(
                                "Upgraded builder StatefulSet disappeared before capture".into(),
                            )
                        })?;
                        let new_binding = shared_types::UserAppResourceBinding {
                            app_id: context.app_id.clone(),
                            lifecycle_id: context.lifecycle_id.clone(),
                            service_type: ServiceType::UserappBuilder,
                            physical_uid: new_workload.uid.clone(),
                            adopted_by_operation: context.operation_id.clone(),
                        };
                        tracing::info!(
                            app = %context.app_id,
                            old_uid = ?current.metadata.uid,
                            new_uid = %new_workload.uid,
                            "bound builder upgraded in-operation; durable binding handed over"
                        );
                        return Ok((new_target, Some(new_binding)));
                    }
                    _ => {
                        return Err(Error::Conflict(
                            "Bound builder Pod configuration changed".into(),
                        ));
                    }
                }
            }
            Ok((target, None))
        }
        .await
        .map_err(|error| if upgrade_attempted { error } else { rejected_before_write(error.to_string()) })?;
        let outcome = self
            .apply_builder_compute_mode_inner(
                &target,
                true,
                true,
                true,
                Some(params.creation_cancelled.as_ref()),
            )
            .await;
        self.report_control_outcome(&target, true, true, &outcome);
        let info = outcome?
            .ok_or_else(|| Error::Conflict("Bound builder did not return a running Pod".into()))?;
        // A resumed creation may have a ready Pod but no public Service yet.
        // Repair it under the original lease before committing the completion
        // receipt, so host-side NodePort registration is not skipped.
        self.create_agent_service(&target.context.app_id, &ServiceType::UserappBuilder)
            .await?;
        Ok((info, upgraded_binding))
    }

    pub(crate) async fn capture_builder_compute(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<BuilderControlTarget> {
        self.capture_builder_compute_with_binding(context, None, false)
            .await
    }

    pub(crate) async fn capture_builder_compute_with_binding(
        &self,
        context: &UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
        adoption: bool,
    ) -> Result<BuilderControlTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::Conflict)?;
        let name = self.pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let sts = match api.get(&name).await {
            Ok(sts) => sts,
            Err(kube::Error::Api(error)) if error.code == 404 => {
                self.confirm_builder_compute_absent(context).await?;
                return Ok(BuilderControlTarget {
                    resource_binding: None,
                    context: context.clone(),
                    workload: None,
                    pod: None,
                    restart_image: None,
                });
            }
            Err(error) => return Err(api_error("Capture builder workload", error)),
        };
        let workload = workload_identity_with_binding(&sts, context, binding, adoption)?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_name = self.agent_pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        let pod = match pods.get(&pod_name).await {
            Ok(pod) => Some(pod_identity(&pod, &workload)?),
            Err(kube::Error::Api(error)) if error.code == 404 => None,
            Err(error) => return Err(api_error("Capture builder pod", error)),
        };
        Ok(BuilderControlTarget {
            resource_binding: binding.cloned(),
            context: context.clone(),
            workload: Some(workload),
            pod,
            restart_image: None,
        })
    }

    pub(crate) async fn capture_orphan_stop(
        &self,
        context: &UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
        inspect_only: bool,
    ) -> Result<Option<shared_types::BuilderOrphanStopTarget>> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::Conflict)?;
        let name = self.pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        let controllers: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        match controllers.get(&name).await {
            Ok(_) => return Ok(None),
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => return Err(api_error("Inspect orphan controller", error)),
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_name = self.agent_pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        let pod = match pods.get(&pod_name).await {
            Ok(pod) => pod,
            Err(kube::Error::Api(error)) if error.code == 404 => return Ok(None),
            Err(error) => return Err(api_error("Inspect orphan Pod", error)),
        };
        let labels = pod.metadata.labels.clone().unwrap_or_default();
        if labels.get("rcoder.io/identifier") != Some(&context.app_id)
            || labels.get("rcoder.io/service-type")
                != Some(&ServiceType::UserappBuilder.to_string())
        {
            return Err(Error::Conflict("Orphan builder labels differ".into()));
        }
        let owner = pod
            .metadata
            .owner_references
            .as_ref()
            .and_then(|owners| owners.iter().find(|owner| owner.controller == Some(true)))
            .ok_or_else(|| Error::Conflict("Orphan builder controller identity missing".into()))?;
        if owner.api_version != "apps/v1" || owner.kind != "StatefulSet" || owner.name != name {
            return Err(Error::Conflict(
                "Orphan builder controller identity differs".into(),
            ));
        }
        if !shared_types::builder_identity_is_bound(
            context,
            &pod.metadata.annotations.clone().unwrap_or_default(),
            &owner.uid,
            binding,
        )
        .map_err(Error::Conflict)?
            && !inspect_only
        {
            return Err(Error::Conflict(
                "Orphan builder requires lifecycle recovery before stop".into(),
            ));
        }
        let target = shared_types::BuilderOrphanStopTarget {
            resource_binding: binding.cloned(),
            context: context.clone(),
            controller_name: name.clone(),
            controller_uid: owner.uid.clone(),
            orphan_pod: BuilderPodIdentity {
                name: pod_name,
                uid: required(pod.metadata.uid.as_deref(), "orphan Pod UID")?,
                resource_version: required(
                    pod.metadata.resource_version.as_deref(),
                    "orphan Pod version",
                )?,
            },
        };
        target.validate().map_err(Error::Conflict)?;
        match controllers.get(&name).await {
            Err(kube::Error::Api(error)) if error.code == 404 => Ok(Some(target)),
            Err(error) => Err(api_error("Recheck orphan controller", error)),
            Ok(_) => Err(Error::Conflict(
                "Builder controller appeared during orphan capture".into(),
            )),
        }
    }

    pub(crate) async fn apply_orphan_stop(
        &self,
        target: &shared_types::BuilderOrphanStopTarget,
    ) -> Result<()> {
        target.validate().map_err(rejected_before_write)?;
        let current = self
            .capture_orphan_stop(&target.context, target.resource_binding.as_ref(), false)
            .await
            .map_err(|error| rejected_before_write(error.to_string()))?;
        if let Some(current) = current {
            if current.controller_name != target.controller_name
                || current.controller_uid != target.controller_uid
                || current.orphan_pod != target.orphan_pod
            {
                return Err(rejected_before_write(
                    "Orphan Pod changed before stop".into(),
                ));
            }
            let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
            let params = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(target.orphan_pod.uid.clone()),
                    resource_version: Some(target.orphan_pod.resource_version.clone()),
                }),
                ..Default::default()
            };
            match pods.delete(&target.orphan_pod.name, &params).await {
                Ok(_) => {}
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(api_error("Delete original orphan builder Pod", error)),
            }
        }
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                if self.observe_orphan_stopped(target).await? {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .map_err(|_| Error::Timeout("Orphan Pod termination is unconfirmed".into()))?
    }

    pub(crate) async fn observe_orphan_stopped(
        &self,
        target: &shared_types::BuilderOrphanStopTarget,
    ) -> Result<bool> {
        target.validate().map_err(Error::Conflict)?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        match pods.get(&target.orphan_pod.name).await {
            Ok(pod) if pod.metadata.uid.as_deref() == Some(target.orphan_pod.uid.as_str()) => {
                return Ok(false);
            }
            Ok(_) => return Err(Error::Conflict("A replacement builder Pod exists".into())),
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => return Err(api_error("Observe original orphan Pod termination", error)),
        }
        self.confirm_builder_compute_absent(&target.context).await?;
        Ok(true)
    }

    /// A missing controller alone does not prove compute has stopped: its Pods
    /// can still be terminating, or have been orphaned by controller deletion.
    pub(crate) async fn confirm_builder_compute_absent(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let listed = pods
            .list(&kube::api::ListParams::default().labels(&format!(
                "rcoder.io/identifier={},rcoder.io/service-type={}",
                context.app_id,
                ServiceType::UserappBuilder
            )))
            .await
            .map_err(|error| api_error("Inspect remaining builder Pods", error))?;
        if !listed.items.is_empty() {
            return Err(Error::Conflict(
                "Builder controller is absent but Pods remain; stop is not confirmed".into(),
            ));
        }
        // Also inspect the canonical name so missing/changed labels cannot hide
        // the original instance from the selector above.
        let pod_name = self.agent_pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        match pods.get(&pod_name).await {
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => return Err(api_error("Inspect absent builder Pod", error)),
            Ok(_) => {
                return Err(Error::Conflict(
                    "Builder Pod still exists without its controller; stop is not confirmed".into(),
                ));
            }
        }
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = self.pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        match api.get(&name).await {
            Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
            Err(error) => Err(api_error("Recheck absent builder controller", error)),
            Ok(_) => Err(Error::Conflict(
                "Builder controller appeared during absence verification".into(),
            )),
        }
    }

    pub(crate) async fn start_builder_compute(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<ContainerBasicInfo>> {
        self.apply_builder_compute_mode(target, true, true).await
    }

    pub(crate) async fn apply_builder_compute(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        self.apply_builder_compute_mode(target, restart, false)
            .await
    }

    /// 控制编排 + 关键阶段诊断事件（批次 C）：成功事件在真实结果确认后
    /// 发布；失败区分明确拒绝（RequestRejected/Conflict）与不可确认
    /// （Timeout/传输错误，对齐 RecoveryRequired 语义）。非阻塞 fire-and-
    /// forget，发布失败绝不改变控制结果。
    pub(crate) async fn apply_builder_compute_mode(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
        only_start: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        let outcome = self
            .apply_builder_compute_mode_inner(target, restart, only_start, false, None)
            .await;
        self.report_control_outcome(target, restart, only_start, &outcome);
        outcome
    }

    pub(crate) fn report_control_outcome(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
        only_start: bool,
        outcome: &Result<Option<ContainerBasicInfo>>,
    ) {
        let Some(workload) = &target.workload else {
            return;
        };
        let action = match (restart, only_start) {
            (_, true) => "WakeCompute",
            (true, false) => "RestartCompute",
            (false, false) => "StopCompute",
        };
        let (type_, reason) = match outcome {
            Ok(None) => (
                crate::runtime::k8s_event_publisher::DiagnosticEventType::Normal,
                "ComputeStopped",
            ),
            Ok(Some(_)) => (
                crate::runtime::k8s_event_publisher::DiagnosticEventType::Normal,
                "ComputeStarted",
            ),
            Err(Error::RequestRejected(_) | Error::Conflict(_)) => (
                crate::runtime::k8s_event_publisher::DiagnosticEventType::Warning,
                "ControlRejected",
            ),
            Err(_) => (
                crate::runtime::k8s_event_publisher::DiagnosticEventType::Warning,
                "ControlUncertain",
            ),
        };
        let detail = match outcome {
            Ok(None) => "stopped".to_string(),
            Ok(Some(info)) => format!("pod={}", info.container_id),
            Err(error) => format!("{error}"),
        };
        let note = format!(
            "app={} lifecycle={} operation={} {detail}",
            target.context.app_id, target.context.lifecycle_id, target.context.operation_id
        );
        self.event_publisher
            .publish(crate::runtime::k8s_event_publisher::DiagnosticEvent::new(
                type_,
                reason,
                action,
                note,
                k8s_openapi::api::core::v1::ObjectReference {
                    api_version: Some("apps/v1".into()),
                    kind: Some("StatefulSet".into()),
                    name: Some(workload.name.clone()),
                    namespace: Some(self.namespace.clone()),
                    uid: Some(workload.uid.clone()),
                    ..Default::default()
                },
            ));
    }

    pub(crate) async fn apply_builder_compute_mode_inner(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
        only_start: bool,
        require_ready: bool,
        cancellation: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Option<ContainerBasicInfo>> {
        let check_cancelled = || {
            if cancellation.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
                Err(Error::CreationCancelled)
            } else {
                Ok(())
            }
        };
        check_cancelled()?;
        target.validate().map_err(rejected_before_write)?;
        let Some(workload) = &target.workload else {
            if restart {
                return Err(rejected_before_write(
                    "Cannot start an absent captured builder".into(),
                ));
            }
            self.confirm_builder_compute_absent(&target.context)
                .await
                .map_err(|error| rejected_before_write(error.to_string()))?;
            return Ok(None);
        };
        if workload.kind != AppResourceKind::StatefulSet {
            return Err(rejected_before_write(
                "Non-Kubernetes builder control target".into(),
            ));
        }
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = sts_api
            .get(&workload.name)
            .await
            .map_err(|error| rejected_before_write(format!("Verify builder workload: {error}")))?;
        if workload_identity_with_binding(
            &current,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )
        .map_err(|error| rejected_before_write(error.to_string()))?
            != *workload
        {
            return Err(rejected_before_write(
                "Builder workload changed after capture".into(),
            ));
        }
        check_cancelled()?;
        if restart && only_start {
            let replicas = current
                .spec
                .as_ref()
                .and_then(|spec| spec.replicas)
                .unwrap_or(1);
            if !matches!(replicas, 0 | 1) {
                return Err(rejected_before_write(
                    "Bound builder has unexpected replicas".into(),
                ));
            }
            if replicas == 0 {
                let receipt = serde_json::to_string(&target.context).map_err(|error| {
                    Error::ConfigurationError(format!("Encode builder start receipt: {error}"))
                })?;
                let patch = builder_compute_start_patch(
                    workload,
                    &receipt,
                    target.restart_image.as_deref(),
                );
                if target.restart_image.is_some() {
                    sts_api
                        .patch(
                            &workload.name,
                            &PatchParams::default(),
                            &Patch::Strategic(patch),
                        )
                        .await
                } else {
                    sts_api
                        .patch(
                            &workload.name,
                            &PatchParams::default(),
                            &Patch::Merge(patch),
                        )
                        .await
                }
                .map_err(|error| {
                    crate::runtime::builder_completion::k8s_error(
                        format!("Wake captured builder workload: {error}"),
                        error,
                    )
                })?;
            }
        } else if restart {
            let pod = target.pod.as_ref().ok_or_else(|| {
                rejected_before_write("Builder restart requires a captured pod".into())
            })?;
            let actual = pods
                .get(&pod.name)
                .await
                .map_err(|error| rejected_before_write(format!("Verify restart pod: {error}")))?;
            if pod_identity(&actual, workload)
                .map_err(|error| rejected_before_write(error.to_string()))?
                != *pod
            {
                return Err(rejected_before_write(
                    "Builder pod changed after capture".into(),
                ));
            }
            pods.delete(&pod.name, &pod_delete_params(pod))
                .await
                .map_err(|error| {
                    crate::runtime::builder_completion::k8s_error(
                        format!("Restart captured builder pod: {error}"),
                        error,
                    )
                })?;
        } else {
            let receipt = serde_json::to_string(&target.context).map_err(|error| {
                Error::ConfigurationError(format!("Encode builder stop receipt: {error}"))
            })?;
            let mut patch = stop_patch(workload);
            patch["metadata"]["annotations"] =
                serde_json::json!({"rcoder.io/compute-stop-receipt":receipt});
            sts_api
                .patch(
                    &workload.name,
                    &PatchParams::default(),
                    &Patch::Merge(patch),
                )
                .await
                .map_err(|error| {
                    crate::runtime::builder_completion::k8s_error(
                        format!("Stop captured builder workload: {error}"),
                        error,
                    )
                })?;
        }
        // Every mutation above returned. Cancellation from here may drop only
        // GET/watch futures; the original runtime lease is still held by caller.
        check_cancelled()?;
        let observation = async {
            // Only observation is time bounded. A timeout never authorizes lease
            // release; the caller records an uncertain operation for reconciliation.
            let deadline = std::time::Instant::now() + Duration::from_secs(90);
            let pod_name =
                self.agent_pod_name(&target.context.app_id, &ServiceType::UserappBuilder)?;
            // 批次 B（plan §3）：STS + Pod 双资源 watch 观察——两条流共享总
            // deadline/退避边界；身份/replicas 冲突快速失败；完成候选由下方
            // 最后 GET 复核（观察不提供跨对象事务，也不授权任何写或租约释放，
            // KR06）。同一 Pod 删除后重现（同 UID）回到观察，不放大预算。
            loop {
                let observed = {
                    let workload = workload.clone();
                    let sts_name = workload.name.clone();
                    let context = target.context.clone();
                    let binding = target.resource_binding.clone();
                    let captured_pod = target.pod.clone();
                    let expected_image = target.restart_image.clone();
                    match crate::runtime::k8s_observation::await_builder_verdict(
                        &sts_api,
                        &sts_name,
                        &pods,
                        &pod_name,
                        deadline,
                        tokio_util::sync::CancellationToken::new(),
                        move |event| {
                            builder_verdict(
                                event,
                                &workload,
                                &context,
                                binding.as_ref(),
                                captured_pod.as_ref(),
                                expected_image.as_deref(),
                                BuilderVerdictMode {
                                    restart,
                                    only_start,
                                    require_ready,
                                },
                            )
                        },
                    )
                    .await
                    {
                        Ok(crate::runtime::k8s_observation::Verdict::Complete(outcome)) => outcome,
                        Ok(crate::runtime::k8s_observation::Verdict::Rejected(reason)) => {
                            return Err(Error::Conflict(reason));
                        }
                        Ok(crate::runtime::k8s_observation::Verdict::Pending) => {
                            return Err(Error::K8sError(
                                "Builder observation ended while pending".into(),
                            ));
                        }
                        Err(error) => return Err(observation_error(error)),
                    }
                };
                match observed {
                    BuilderObservation::Stopped => {
                        // K04：最终权威复核共用观察的绝对 deadline——接近截止时
                        // 出现的候选不得在复核阶段无界等待（GET hang/慢响应由
                        // 剩余预算截断，超时按未知结果交上层保护，不当作缺席/稳定）。
                        if std::time::Instant::now() >= deadline {
                            return Err(Error::K8sError(
                                "Builder stop verification budget exhausted".into(),
                            ));
                        }
                        // 复核①：Pod 确已消失。同 UID 重现回到观察（STS 控制器
                        // 仍在终止窗口），异 UID 视为替换冲突。
                        match pods.get(&pod_name).await {
                            Err(kube::Error::Api(error)) if error.code == 404 => {}
                            Err(error) => {
                                return Err(api_error("Verify stopped builder pod", error));
                            }
                            Ok(pod) => {
                                let seen = pod_identity(&pod, workload)
                                    .map_err(|error| Error::Conflict(error.to_string()))?;
                                if target.pod.as_ref().is_some_and(|old| old.uid == seen.uid) {
                                    continue;
                                }
                                return Err(Error::Conflict(
                                    "A replacement builder pod appeared while stopping".into(),
                                ));
                            }
                        }
                        // 复核②：STS 身份未替换且 replicas 保持 0。STS 整体消失
                        // （被带外删除）显式归类为冲突（K01）——捕获身份已不存在，
                        // 不当作成功也不混入通用后端错误。K04：同样受绝对 deadline
                        // 约束（复核①耗尽剩余预算时不再进入无界 GET）。
                        if std::time::Instant::now() >= deadline {
                            return Err(Error::K8sError(
                                "Builder stop verification budget exhausted".into(),
                            ));
                        }
                        let current = match sts_api.get(&workload.name).await {
                            Ok(current) => current,
                            Err(kube::Error::Api(error)) if error.code == 404 => {
                                return Err(Error::Conflict(
                                    "Builder workload vanished while stopping".into(),
                                ));
                            }
                            Err(error) => {
                                return Err(api_error("Verify stopped builder workload", error));
                            }
                        };
                        verify_workload_stable(
                            &current,
                            &target.context,
                            target.resource_binding.as_ref(),
                            workload,
                            Some(0),
                        )
                        // K02：写后复核失败不得归类为写前拒绝——patch 已落盘，
                        // RequestRejected 会让上层释放 mutating 并记 Failed，
                        // 丢失未知结果保护。复核冲突 = 不确定 → Conflict（上层
                        // 保留保护，操作转 RecoveryRequired）
                        .map_err(Error::Conflict)?;
                        return Ok(None);
                    }
                    BuilderObservation::Ready(boxed) => {
                        let (info, captured) = *boxed;
                        // K04：Ready 复核共用绝对 deadline（同上，不无界等待）
                        if std::time::Instant::now() >= deadline {
                            return Err(Error::K8sError(
                                "Builder readiness verification budget exhausted".into(),
                            ));
                        }
                        // 复核①：完成候选 Pod 仍是同一物理对象（UID 核验）。
                        let pod = pods
                            .get(&pod_name)
                            .await
                            .map_err(|error| api_error("Verify ready builder pod", error))?;
                        let seen = pod_identity(&pod, workload)
                            .map_err(|error| Error::Conflict(error.to_string()))?;
                        if seen.uid != captured.uid {
                            return Err(Error::Conflict(
                                "Builder pod changed between observation and verification".into(),
                            ));
                        }
                        if !pod_agent_image_matches(&pod, target.restart_image.as_deref()) {
                            continue;
                        }
                        // 复核②：STS 身份未替换（wake 还须 replicas 保持 1）。
                        // K04：预算耗尽时不进入无界 GET。
                        if std::time::Instant::now() >= deadline {
                            return Err(Error::K8sError(
                                "Builder readiness verification budget exhausted".into(),
                            ));
                        }
                        let current = sts_api
                            .get(&workload.name)
                            .await
                            .map_err(|error| api_error("Verify ready builder workload", error))?;
                        verify_workload_stable(
                            &current,
                            &target.context,
                            target.resource_binding.as_ref(),
                            workload,
                            only_start.then_some(1),
                        )
                        // K02：同上——写后（wake/restart 的 scale 写入已发生）复核
                        // 冲突保持未知结果保护，不当作写前拒绝
                        .map_err(Error::Conflict)?;
                        if !statefulset_agent_image_matches(
                            &current,
                            target.restart_image.as_deref(),
                        ) {
                            continue;
                        }
                        // Wake/Restart 后的 Pod 是新执行域：对旧 Pod 遗留代次做
                        // 平台核验恢复（bounded，失败保留保护不阻塞控制结果）。
                        self.reconcile_builder_execution_domain_bounded(&target.context.app_id)
                            .await;
                        return Ok(Some(info));
                    }
                }
            }
        };
        tokio::select! {
            biased;
            _ = async {
                if let Some(flag) = cancellation {
                    loop {
                        if flag.load(std::sync::atomic::Ordering::Acquire) { break; }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                } else {
                    std::future::pending::<()>().await;
                }
            } => Err(Error::CreationCancelled),
            result = observation => result,
        }
    }
}
