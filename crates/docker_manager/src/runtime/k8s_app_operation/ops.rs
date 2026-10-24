use super::*;

impl KubernetesRuntime {
    /// A successor Lease retires the captured authority only. This never proves
    /// old resource writes complete and never releases the successor's mutex.
    pub(crate) async fn captured_deletion_lease_replaced(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        receipt
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            namespace,
            name,
            uid,
            token,
            ..
        } = receipt
        else {
            return Err(ContainerRuntimeError::Conflict(
                "Deletion lease runtime differs".into(),
            ));
        };
        if namespace != &self.namespace || name != &operation_name(&context.app_id, service_type)? {
            return Err(ContainerRuntimeError::Conflict(
                "Deletion lease scope differs".into(),
            ));
        }
        let api: Api<Lease> = Api::namespaced(self.client.clone(), namespace);
        let Some(current) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Inspect deletion lease successor: {error}"))
        })?
        else {
            // The next ordinary inspection handles absence. Do not infer a
            // replaced legacy ConfigMap's release semantics from a Lease read.
            return Ok(false);
        };
        let labels = current.metadata.labels.as_ref();
        if labels.and_then(|labels| labels.get("rcoder.io/operation-app")) != Some(&context.app_id)
            || labels
                .and_then(|labels| labels.get("rcoder.io/operation-family"))
                .map(String::as_str)
                != Some(service_type.to_string().as_str())
            || current
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("rcoder.io/lifecycle-id"))
                .is_some_and(|life| life != &context.lifecycle_id)
        {
            return Err(ContainerRuntimeError::Conflict(
                "Deletion lease successor belongs to another application/family/lifecycle".into(),
            ));
        }
        let current_uid = current
            .metadata
            .uid
            .as_deref()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::K8sError("Deletion lease successor has no UID".into())
            })?;
        let current_token = holder_token(&current)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::K8sError(
                    "Deletion lease successor has no holder token".into(),
                )
            })?;
        Ok(current_uid != uid || current_token != *token)
    }

    pub(crate) async fn validate_captured_application_operation(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        receipt
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            namespace,
            name,
            uid,
            token,
            ..
        } = receipt
        else {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Kubernetes lease receipt is required".into(),
            ));
        };
        if namespace != &self.namespace || name != &operation_name(&context.app_id, service_type)? {
            return Err(ContainerRuntimeError::Conflict(
                "Operation lease scope changed".into(),
            ));
        }
        // Lease first, legacy ConfigMap fallback second (same name, different
        // API group). Lease identity is uid + holder token; the receipt's
        // resourceVersion is advisory because renewals move it forward.
        let lease_api: Api<Lease> = Api::namespaced(self.client.clone(), namespace);
        if let Some(current) = lease_api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe captured operation lease: {error}"))
        })? {
            let identity_ok = current.metadata.uid.as_deref() == Some(uid.as_str())
                && holder_token(&current).as_deref() == Some(token.as_str())
                && current
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|values| values.get("rcoder.io/operation-app"))
                    .map(String::as_str)
                    == Some(context.app_id.as_str())
                && current
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|values| values.get("rcoder.io/operation-family"))
                    .map(String::as_str)
                    == Some(service_type.to_string().as_str());
            if !identity_ok {
                return Err(ContainerRuntimeError::Conflict(
                    "Captured operation lease identity changed".into(),
                ));
            }
            // TTL 过期 = 不再被有效持有：Lease 对象过期后并不消失，缺此判定
            // 会让 holder 死亡兜底把"过期未接管"的租约永远当活持有——而接管
            // 只发生在 acquire，新操作正被围栏挡着 = 死锁回归。
            if lease_expired_at(k8s_openapi::jiff::Timestamp::now(), &current) {
                return Ok(false);
            }
            return Ok(true);
        }
        self.validate_captured_configmap(context, service_type, name, receipt)
            .await
    }

    /// A retained original Lease can identify a completed result after its
    /// holder stopped renewing. This observation never renews or acquires it.
    pub(crate) async fn observe_original_application_lease(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<container_runtime_api::AppOperationLeaseObservation> {
        use container_runtime_api::AppOperationLeaseObservation;
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        receipt
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            namespace,
            name,
            uid,
            token,
            ..
        } = receipt
        else {
            return Ok(AppOperationLeaseObservation::Unconfirmed);
        };
        if namespace != &self.namespace || name != &operation_name(&context.app_id, service_type)? {
            return Err(ContainerRuntimeError::Conflict(
                "Original operation lease scope changed".into(),
            ));
        }
        let api: Api<Lease> = Api::namespaced(self.client.clone(), namespace);
        let Some(current) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe original operation lease: {error}"))
        })?
        else {
            return Ok(AppOperationLeaseObservation::Unconfirmed);
        };
        let annotations = current.metadata.annotations.as_ref().ok_or_else(|| {
            ContainerRuntimeError::Conflict(
                "Original operation lease execution metadata missing".into(),
            )
        })?;
        let expected_holder = if annotations.get("rcoder.io/legacy-operation-id") == Some(token) {
            // Ordinary application operations acquire this exact mutex before
            // SQL admission assigns an executor. Its durable binding associates
            // the random token with that later operation; missing execution
            // annotations are expected, not evidence of a different owner.
            // Existing contradictory fields still identify a different attempt.
            if annotations
                .get("rcoder.io/compute-lease")
                .map(String::as_str)
                == Some("true")
                || annotations
                    .get("rcoder.io/lease-token")
                    .is_some_and(|value| !value.is_empty())
                || context.resource_metadata().iter().any(|(key, expected)| {
                    annotations
                        .get(key)
                        .is_some_and(|actual| !actual.is_empty() && actual != expected)
                })
            {
                return Err(ContainerRuntimeError::Conflict(
                    "Original legacy operation lease execution identity changed".into(),
                ));
            }
            format!("legacy:{token}")
        } else {
            // Context-bound operations stamp every execution field. Only the
            // redundant application annotation can be supplied by its label.
            let mut execution_metadata = annotations.clone();
            if !execution_metadata.contains_key("rcoder.io/application-id")
                && let Some(app) = current
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("rcoder.io/operation-app"))
            {
                execution_metadata.insert("rcoder.io/application-id".into(), app.clone());
            }
            context
                .validate_operation_metadata(&execution_metadata)
                .map_err(ContainerRuntimeError::Conflict)?;
            if annotations
                .get("rcoder.io/compute-lease")
                .map(String::as_str)
                == Some("true")
                && (token != &context.executor_id
                    || annotations.get("rcoder.io/lease-token") != Some(token))
            {
                return Err(ContainerRuntimeError::Conflict(
                    "Original compute lease attempt token changed".into(),
                ));
            }
            format!("{}:{}", context.executor_id, context.operation_id)
        };
        let identity_matches = current.metadata.uid.as_deref() == Some(uid.as_str())
            && holder_token(&current).as_deref() == Some(token.as_str())
            && current
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("rcoder.io/operation-app"))
                == Some(&context.app_id)
            && current
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("rcoder.io/operation-family"))
                == Some(&service_type.to_string())
            && current
                .spec
                .as_ref()
                .and_then(|spec| spec.holder_identity.as_deref())
                == Some(expected_holder.as_str())
            && current.metadata.deletion_timestamp.is_none();
        if !identity_matches {
            return Err(ContainerRuntimeError::Conflict(
                "Original operation lease identity changed".into(),
            ));
        }
        Ok(
            if lease_expired_at(k8s_openapi::jiff::Timestamp::now(), &current) {
                AppOperationLeaseObservation::ExpiredOriginalIdentity
            } else {
                AppOperationLeaseObservation::Held
            },
        )
    }

    /// Legacy ConfigMap lease validation (pre-Lease migration stock): strict
    /// uid + resourceVersion + token match — ConfigMap revisions never churn.
    async fn validate_captured_configmap(
        &self,
        context: &shared_types::UserAppExecutionContext,
        service_type: &ServiceType,
        name: &str,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        let (uid, resource_version, token) = match receipt {
            shared_types::UserAppOperationLeaseReceipt::Kubernetes {
                uid,
                resource_version,
                token,
                ..
            } => (uid, resource_version, token),
            _ => {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "Kubernetes lease receipt is required".into(),
                ));
            }
        };
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(current) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe captured operation lease: {error}"))
        })?
        else {
            return Ok(false);
        };
        if !configmap_identity_matches(
            &current,
            context,
            service_type,
            uid,
            resource_version,
            token,
        ) {
            return Err(ContainerRuntimeError::Conflict(
                "Captured operation lease identity changed".into(),
            ));
        }
        Ok(true)
    }

    /// Read-only discovery of a historical builder creation's native
    /// authority for a stale `Running` EnsureBuilder without a persisted
    /// receipt. Identity is located by durable fields only (name from
    /// app/family, holder from the record's executor/operation); annotations
    /// are validated whenever present, so an older partial stamp cannot spoof
    /// another execution. Legacy ConfigMap stock is not consulted: it is
    /// swept after 24h and never fences a Lease-era record.
    pub(crate) async fn observe_unbound_builder_creation_authority(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<container_runtime_api::UnboundBuilderCreationAuthority> {
        use container_runtime_api::UnboundBuilderCreationAuthority;
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let service_type = ServiceType::UserappBuilder;
        let name = operation_name(&context.app_id, &service_type)?;
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(current) = api.get_opt(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe unbound builder authority: {error}"))
        })?
        else {
            return Ok(UnboundBuilderCreationAuthority::Absent);
        };
        let labels = current.metadata.labels.as_ref();
        let foreign = |holder: Option<String>| UnboundBuilderCreationAuthority::Foreign { holder };
        let holder = current
            .spec
            .as_ref()
            .and_then(|spec| spec.holder_identity.clone());
        if labels.and_then(|labels| labels.get("rcoder.io/operation-app")) != Some(&context.app_id)
            || labels
                .and_then(|labels| labels.get("rcoder.io/operation-family"))
                .map(String::as_str)
                != Some(service_type.to_string().as_str())
            || current.metadata.deletion_timestamp.is_some()
        {
            return Ok(foreign(holder));
        }
        let annotations = current.metadata.annotations.as_ref();
        // Present identity annotations must match exactly; absent ones (older
        // stamps) leave the holder string as the binding identity.
        if let Some(annotations) = annotations {
            for (key, expected) in context.resource_metadata() {
                if annotations
                    .get(&key)
                    .is_some_and(|actual| !actual.is_empty() && actual != &expected)
                {
                    return Ok(foreign(holder));
                }
            }
            if annotations
                .get("rcoder.io/compute-lease")
                .map(String::as_str)
                == Some("true")
                && annotations.get("rcoder.io/lease-token") != Some(&context.executor_id)
            {
                return Ok(foreign(holder));
            }
        }
        let expected_holder = format!("{}:{}", context.executor_id, context.operation_id);
        let Some(holder) = holder else {
            return Ok(foreign(None));
        };
        if holder != expected_holder {
            return Ok(foreign(Some(holder)));
        }
        Ok(
            if lease_expired_at(k8s_openapi::jiff::Timestamp::now(), &current) {
                UnboundBuilderCreationAuthority::ExpiredOriginal { holder }
            } else {
                UnboundBuilderCreationAuthority::ActiveOriginal
            },
        )
    }

    pub(crate) async fn release_captured_application_operation(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        receipt
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            namespace,
            name,
            uid,
            token,
            ..
        } = receipt
        else {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Kubernetes lease receipt is required".into(),
            ));
        };
        if namespace != &self.namespace || name != &operation_name(&context.app_id, service_type)? {
            return Err(ContainerRuntimeError::Conflict(
                "Operation lease scope changed".into(),
            ));
        }
        // Lease delete attempt doubles as the kind dispatch: a single read
        // resolves existence, identity and the live resourceVersion.
        let lease_api: Api<Lease> = Api::namespaced(self.client.clone(), namespace);
        match remove_owned_lease(&lease_api, name, uid, token).await {
            Ok(LeaseRemoval::Deleted) => return Ok(()),
            Ok(LeaseRemoval::TakenOver) => {
                // The successor's Lease now owns the object — this receipt's
                // authority is gone, which is a released state for it. Never
                // delete the successor's lock and never fall through to the
                // legacy ConfigMap: the name hosts a live Lease, not
                // migration stock.
                tracing::debug!(%name, "captured operation lease was taken over before release");
                return Ok(());
            }
            Ok(LeaseRemoval::Absent) => {}
            Err(error) => return Err(ContainerRuntimeError::K8sError(error)),
        }
        self.release_captured_configmap(context, service_type, name, receipt)
            .await
    }

    /// Legacy ConfigMap release path for migration stock.
    async fn release_captured_configmap(
        &self,
        context: &shared_types::UserAppExecutionContext,
        service_type: &ServiceType,
        name: &str,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        let (uid, resource_version, token) = match receipt {
            shared_types::UserAppOperationLeaseReceipt::Kubernetes {
                uid,
                resource_version,
                token,
                ..
            } => (uid, resource_version, token),
            _ => {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "Kubernetes lease receipt is required".into(),
                ));
            }
        };
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(current) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe captured operation lease: {error}"))
        })?
        else {
            return Ok(());
        };
        if !configmap_identity_matches(
            &current,
            context,
            service_type,
            uid,
            resource_version,
            token,
        ) {
            return Err(ContainerRuntimeError::Conflict(
                "Captured operation lease identity changed".into(),
            ));
        }
        remove_owned_configmap(&api, name, uid, resource_version)
            .await
            .map_err(ContainerRuntimeError::K8sError)
    }

    pub(crate) async fn acquire_application_operation(
        &self,
        app_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        self.acquire_application_operation_with_context(app_id, service_type, None)
            .await
    }

    /// Migration orphan sweep for legacy ConfigMap operation locks: acquire
    /// only creates Lease objects now, so any remaining `rcoder-operation-*`
    /// ConfigMap is pre-migration stock. Age-gated (24h ≫ any legitimate
    /// operation, and ≫ the rolling-upgrade window where old replicas still
    /// create ConfigMap locks) and uid-preconditioned; deleting one only
    /// releases a stale mutex and never authorizes a mutation. Concurrency
    /// safety does not rest on this sweep: durable PG admission remains the
    /// first serialization layer for every mutating path, so a swept lock
    /// cannot by itself let two operations through.
    pub(crate) async fn sweep_legacy_operation_configmaps(
        &self,
        older_than: std::time::Duration,
    ) -> ContainerRuntimeResult<Vec<String>> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let list = api
            .list(&ListParams::default().labels("rcoder.io/operation-app"))
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("list legacy operation locks: {error}"))
            })?;
        let now = k8s_openapi::jiff::Timestamp::now();
        let threshold =
            k8s_openapi::jiff::SignedDuration::try_from(older_than).map_err(|error| {
                ContainerRuntimeError::ConfigurationError(format!(
                    "legacy operation lock sweep window is invalid: {error}"
                ))
            })?;
        let mut deleted = Vec::new();
        for object in list.items {
            let Some(name) = object.metadata.name.clone() else {
                continue;
            };
            let Some(uid) = object.metadata.uid.clone() else {
                continue;
            };
            let Some(created) = object.metadata.creation_timestamp.as_ref() else {
                continue;
            };
            // Span seconds carry the whole age; younger than the threshold
            // (including skew-negative ages) is skipped. Second granularity
            // is plenty for a 24h-class sweep window.
            if (now - created.0).get_seconds() < threshold.as_secs() {
                continue;
            }
            let params = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(uid),
                    resource_version: object.metadata.resource_version.clone(),
                }),
                ..Default::default()
            };
            match api.delete(&name, &params).await {
                Ok(_) => {
                    tracing::info!(
                        name = %name,
                        "legacy ConfigMap operation lock swept (pre-Lease migration stock)"
                    );
                    deleted.push(name);
                }
                Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => {
                    // Vanished or concurrently mutated: not ours to judge this
                    // round; the next sweep re-evaluates.
                }
                Err(error) => {
                    tracing::warn!(name = %name, %error, "legacy operation lock delete failed");
                }
            }
        }
        Ok(deleted)
    }

    pub(crate) async fn acquire_application_operation_with_context(
        &self,
        app_id: &str,
        service_type: &ServiceType,
        context: Option<&shared_types::UserAppExecutionContext>,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        self.acquire_operation_lease(app_id, service_type, context, false)
            .await
    }

    pub(super) async fn acquire_operation_lease(
        &self,
        app_id: &str,
        service_type: &ServiceType,
        context: Option<&shared_types::UserAppExecutionContext>,
        compute: bool,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        self.acquire_operation_lease_mode(app_id, service_type, context, compute, true, None)
            .await
    }

    pub(crate) async fn acquire_vacant_application_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        self.acquire_operation_lease_mode(app_id, &ServiceType::Userapp, None, false, false, None)
            .await
    }

    pub(crate) async fn acquire_observed_expired_application_operation(
        &self,
        app_id: &str,
        current_lifecycle_id: &str,
        observed: &container_runtime_api::AppOperationMutexIdentity,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        if app_id.is_empty() || current_lifecycle_id.is_empty() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Observed-expired acquisition requires application and current lifecycle IDs"
                    .into(),
            ));
        }
        let name = operation_name(app_id, &ServiceType::Userapp)?;
        observation::validate_observed_scope(
            observed,
            &self.namespace,
            &name,
            current_lifecycle_id,
        )?;
        self.acquire_operation_lease_mode(
            app_id,
            &ServiceType::Userapp,
            None,
            false,
            false,
            Some((current_lifecycle_id, observed)),
        )
        .await
    }

    async fn acquire_operation_lease_mode(
        &self,
        app_id: &str,
        service_type: &ServiceType,
        context: Option<&shared_types::UserAppExecutionContext>,
        compute: bool,
        allow_takeover: bool,
        observed_expired: Option<(&str, &container_runtime_api::AppOperationMutexIdentity)>,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        if !matches!(
            service_type,
            ServiceType::Userapp | ServiceType::UserappBuilder
        ) {
            return Err(ContainerRuntimeError::ConfigurationError(
                "application operation lease only supports UserApp families".into(),
            ));
        }
        let name = operation_name(app_id, service_type)?;
        let mut annotations = std::collections::BTreeMap::new();
        let holder_identity;
        if let Some(context) = context {
            context
                .validate_identity(app_id)
                .map_err(ContainerRuntimeError::ConfigurationError)?;
            annotations.insert(
                "rcoder.io/operation-id".into(),
                context.operation_id.clone(),
            );
            annotations.insert(
                "rcoder.io/lifecycle-id".into(),
                context.lifecycle_id.clone(),
            );
            annotations.insert("rcoder.io/executor-id".into(), context.executor_id.clone());
            annotations.insert(
                "rcoder.io/request-fingerprint".into(),
                context.request_fingerprint.clone(),
            );
            holder_identity = format!("{}:{}", context.executor_id, context.operation_id);
        } else {
            // Legacy locks are intentionally not advertised as durable operations.
            let legacy_id = uuid::Uuid::new_v4().to_string();
            annotations.insert("rcoder.io/legacy-operation-id".into(), legacy_id.clone());
            holder_identity = format!("legacy:{legacy_id}");
        }
        if let Some((current_lifecycle_id, _)) = observed_expired {
            annotations.insert("rcoder.io/lifecycle-id".into(), current_lifecycle_id.into());
        }
        if compute {
            let context = context.ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "Compute lease requires execution identity".into(),
                )
            })?;
            annotations.insert("rcoder.io/lease-token".into(), context.executor_id.clone());
            annotations.insert("rcoder.io/compute-lease".into(), "true".into());
        }
        let token = annotations
            .get("rcoder.io/lease-token")
            .or_else(|| annotations.get("rcoder.io/operation-id"))
            .or_else(|| annotations.get("rcoder.io/legacy-operation-id"))
            .cloned()
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("Operation token is missing".into())
            })?;
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let now = lease_now();
        let object = Lease {
            metadata: kube::api::ObjectMeta {
                name: Some(name.clone()),
                labels: Some(std::collections::BTreeMap::from([
                    (
                        "rcoder.io/operation-family".into(),
                        service_type.to_string(),
                    ),
                    ("rcoder.io/operation-app".into(), app_id.into()),
                ])),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(holder_identity),
                lease_duration_seconds: Some(LEASE_TTL_SECONDS),
                acquire_time: Some(now.clone()),
                renew_time: Some(now),
                lease_transitions: Some(0),
                ..Default::default()
            }),
        };
        let acquired = if let Some((current_lifecycle_id, observed)) = observed_expired {
            // Never POST from an expired observation: disappearance, renewal or
            // a successor requires a new read and normal admission decision.
            match take_over_observed_expired(
                &api,
                &self.namespace,
                &name,
                app_id,
                current_lifecycle_id,
                observed,
                &object,
            )
            .await?
            {
                TakeOver::Acquired(taken) => *taken,
                TakeOver::InProgress(operation_id) => {
                    return Err(ContainerRuntimeError::OperationInProgress(Box::new(
                        shared_types::UserAppOperationInProgress {
                            app_id: app_id.into(),
                            service_type: *service_type,
                            resource_name: name,
                            operation_id,
                        },
                    )));
                }
            }
        } else {
            match api.create(&PostParams::default(), &object).await {
                Ok(created) => created,
                Err(kube::Error::Api(status)) if status.code == 409 => {
                    if !allow_takeover {
                        // The losing create is definitively rejected. Observe only;
                        // expiry and preflight absence never authorize a patch.
                        let current = api.get_opt(&name).await.map_err(|error| {
                            ContainerRuntimeError::K8sError(format!(
                                "Read occupied application mutex after rejected create: {error}"
                            ))
                        })?;
                        let operation_id = current.as_ref().and_then(holder_token);
                        return Err(ContainerRuntimeError::OperationInProgress(Box::new(
                            shared_types::UserAppOperationInProgress {
                                app_id: app_id.into(),
                                service_type: *service_type,
                                resource_name: name,
                                operation_id,
                            },
                        )));
                    }
                    match take_over_expired(&api, &name, &object).await? {
                        TakeOver::Acquired(taken) => *taken,
                        TakeOver::InProgress(operation_id) => {
                            // Report the live holder's identity, never this losing
                            // request's UUID; None means an unidentifiable holder.
                            return Err(ContainerRuntimeError::OperationInProgress(Box::new(
                                shared_types::UserAppOperationInProgress {
                                    app_id: app_id.into(),
                                    service_type: *service_type,
                                    resource_name: name,
                                    operation_id,
                                },
                            )));
                        }
                    }
                }
                Err(error) => {
                    return Err(ContainerRuntimeError::K8sError(format!(
                        "acquire application operation {name}: {error}"
                    )));
                }
            }
        };
        let uid = acquired
            .metadata
            .uid
            .clone()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "acquired operation lease has no UID".into(),
                )
            })?;
        let version = acquired
            .metadata
            .resource_version
            .clone()
            .filter(|version| !version.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "acquired operation lease has no version".into(),
                )
            })?;
        let cancel = tokio_util::sync::CancellationToken::new();
        spawn_renewal(
            api.clone(),
            name.clone(),
            uid.clone(),
            token.clone(),
            cancel.clone(),
        );
        let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type: *service_type,
            namespace: self.namespace.clone(),
            name: name.clone(),
            uid: uid.clone(),
            resource_version: version,
            token: token.clone(),
        };
        Ok(Box::new(OperationLease {
            receipt,
            api,
            name,
            uid,
            token,
            released: false,
            cancel,
        }))
    }
}
