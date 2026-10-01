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
        let captured_uid = existing
            .metadata
            .uid
            .as_deref()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("Builder replacement UID missing".into())
            })?
            .to_string();
        let mut captured_rv = existing
            .metadata
            .resource_version
            .as_deref()
            .filter(|rv| !rv.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "Builder replacement resourceVersion missing".into(),
                )
            })?
            .to_string();
        let replicas = existing
            .spec
            .as_ref()
            .and_then(|spec| spec.replicas)
            .unwrap_or(1);
        let pods: Api<k8s_openapi::api::core::v1::Pod> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let captured_pods = pods
            .list(&kube::api::ListParams::default().labels(&format!(
                "rcoder.io/identifier={},rcoder.io/service-type={family}",
                context.app_id
            )))
            .await
            .map_err(|error| {
                crate::runtime::builder_completion::k8s_error(
                    "Capture builder pods before replacement".into(),
                    error,
                )
            })?
            .items
            .into_iter()
            .filter(|pod| {
                pod.metadata
                    .owner_references
                    .as_ref()
                    .is_some_and(|owners| {
                        owners.iter().any(|owner| {
                            owner.controller == Some(true)
                                && owner.kind == "StatefulSet"
                                && owner.uid == captured_uid
                        })
                    })
            })
            .map(|pod| {
                let name = pod
                    .metadata
                    .name
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ContainerRuntimeError::ConfigurationError(
                            "Captured builder Pod name missing".into(),
                        )
                    })?;
                let uid = pod
                    .metadata
                    .uid
                    .filter(|uid| !uid.is_empty())
                    .ok_or_else(|| {
                        ContainerRuntimeError::ConfigurationError(
                            "Captured builder Pod UID missing".into(),
                        )
                    })?;
                Ok((name, uid))
            })
            .collect::<ContainerRuntimeResult<Vec<_>>>()?;
        if replicas > 0 {
            let scaled = {
                let mut patched = existing.clone();
                if let Some(spec) = patched.spec.as_mut() {
                    spec.replicas = Some(0);
                }
                patched
            };
            let scaled = api
                .replace(&name, &PostParams::default(), &scaled)
                .await
                .map_err(|error| {
                    crate::runtime::builder_completion::k8s_error(
                        format!("Scale old builder StatefulSet to 0: {error}"),
                        error,
                    )
                })?;
            if scaled.metadata.uid.as_deref() != Some(captured_uid.as_str()) {
                return Err(ContainerRuntimeError::Conflict(
                    "Builder workload changed while scaling for replacement".into(),
                ));
            }
            captured_rv = scaled
                .metadata
                .resource_version
                .filter(|rv| !rv.is_empty())
                .ok_or_else(|| {
                    ContainerRuntimeError::ConfigurationError(
                        "Scaled builder resourceVersion missing".into(),
                    )
                })?;
        }
        let delete_params = DeleteParams {
            preconditions: Some(kube::api::Preconditions {
                uid: Some(captured_uid.clone()),
                resource_version: Some(captured_rv),
            }),
            ..DeleteParams::default()
        };
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
                    captured_uid
                )));
            }
            let observed_sts = api.get_opt(&name).await.map_err(|error| {
                crate::runtime::builder_completion::k8s_error(
                    format!("Wait for builder StatefulSet removal: {error}"),
                    error,
                )
            })?;
            if observed_sts
                .as_ref()
                .is_some_and(|sts| sts.metadata.uid.as_deref() != Some(captured_uid.as_str()))
            {
                return Err(ContainerRuntimeError::Conflict(
                    "Builder workload name was rebound before replacement completed".into(),
                ));
            }
            let sts_gone = observed_sts.is_none();
            let pods_gone = if captured_pods.is_empty() {
                true
            } else {
                let mut all_gone = true;
                for (pod_name, pod_uid) in &captured_pods {
                    match pods.get_opt(pod_name).await {
                        Ok(None) => {}
                        Ok(Some(pod)) => {
                            if pod.metadata.uid.as_deref() != Some(pod_uid.as_str()) {
                                return Err(ContainerRuntimeError::Conflict(
                                    "Captured builder Pod name was rebound during replacement"
                                        .into(),
                                ));
                            }
                            // A deletion timestamp is only an accepted intent.
                            // The old Pod still occupies the slot until absent.
                            all_gone = false;
                            break;
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

#[cfg(test)]
mod replacement_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn replacement_deletes_scaled_version_and_waits_for_terminating_pod_absence() {
        drop(rustls::crypto::ring::default_provider().install_default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().unwrap(),
        ))
        .unwrap();
        let runtime = KubernetesRuntime {
            client,
            namespace: "upgrade-test".into(),
            config: super::super::super::kubernetes_runtime::KubernetesRuntimeConfig {
                namespace: "upgrade-test".into(),
                cluster_domain: "cluster.local".into(),
                pod_ttl_seconds: None,
                image_pull_secret: None,
                service_account_name: "test".into(),
                nfs_server: "unused".into(),
                nfs_path: "/unused".into(),
                storage_class: "unused".into(),
                access_mode: "ReadWriteOnce".into(),
                docker_manager_config: Default::default(),
                kubernetes_config: Default::default(),
                execution_authority: "k8s:test".into(),
            },
            pod_cache: Default::default(),
            subvolume_path_cache: Default::default(),
            event_publisher: Default::default(),
            event_counters: Arc::new(
                crate::runtime::k8s_event_publisher::PublisherCounters::default(),
            ),
        };
        let context = shared_types::UserAppExecutionContext {
            app_id: "upgradetest".into(),
            lifecycle_id: "lifeone".into(),
            operation_id: "operationone".into(),
            executor_id: "executorone".into(),
            request_fingerprint: "a".repeat(64),
        };
        let name = runtime
            .pod_name(&context.app_id, &ServiceType::UserappBuilder)
            .unwrap();
        let old = StatefulSet {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                uid: Some("oldcontroller".into()),
                resource_version: Some("10".into()),
                ..Default::default()
            },
            spec: Some(StatefulSetSpec {
                replicas: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut desired = old.clone();
        desired.metadata.uid = None;
        desired.metadata.resource_version = None;
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pod_reads = Arc::new(AtomicUsize::new(0));
        let recorded = requests.clone();
        let reads = pod_reads.clone();
        let workload = serde_json::to_value(&old).unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut input = Vec::new();
                let mut buffer = [0u8; 4096];
                let (header_end, body_size) = loop {
                    let n = stream.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        break (0, 0);
                    }
                    input.extend_from_slice(&buffer[..n]);
                    if let Some(end) = input.windows(4).position(|part| part == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&input[..end]);
                        let size = header
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if input.len() >= end + 4 + size {
                            break (end, size);
                        }
                    }
                };
                if header_end == 0 {
                    continue;
                }
                let header = String::from_utf8_lossy(&input[..header_end]);
                let mut parts = header.lines().next().unwrap().split_whitespace();
                let method = parts.next().unwrap().to_string();
                let path = parts.next().unwrap().to_string();
                let body = if body_size == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&input[header_end + 4..header_end + 4 + body_size])
                        .unwrap()
                };
                recorded
                    .lock()
                    .unwrap()
                    .push((method.clone(), path.clone(), body.clone()));
                let mut status = 200;
                let response = if method == "GET" && path.contains("/pods?") {
                    serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":[{
                        "metadata":{"name":format!("{name}-0"),"uid":"oldpod","resourceVersion":"1",
                            "ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":name,"uid":"oldcontroller","controller":true}]}}]})
                } else if method == "PUT" {
                    let mut scaled = workload.clone();
                    scaled["metadata"]["resourceVersion"] = serde_json::json!("11");
                    scaled["spec"]["replicas"] = serde_json::json!(0);
                    scaled
                } else if method == "DELETE" {
                    if body["preconditions"]["uid"] != "oldcontroller"
                        || body["preconditions"]["resourceVersion"] != "11"
                    {
                        status = 409;
                        serde_json::json!({"apiVersion":"v1","kind":"Status","code":409,"reason":"Conflict","message":"stale version","status":"Failure"})
                    } else {
                        serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success"})
                    }
                } else if method == "GET"
                    && path.contains("/pods/")
                    && reads.fetch_add(1, Ordering::SeqCst) == 0
                {
                    serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":format!("{name}-0"),"uid":"oldpod","deletionTimestamp":"2026-10-02T00:00:00Z"}})
                } else if method == "POST" {
                    if reads.load(Ordering::SeqCst) < 2 {
                        status = 409;
                        serde_json::json!({"apiVersion":"v1","kind":"Status","code":409,"reason":"Conflict","message":"old Pod still exists","status":"Failure"})
                    } else {
                        let mut created = body.clone();
                        created["metadata"]["uid"] = serde_json::json!("newcontroller");
                        created["metadata"]["resourceVersion"] = serde_json::json!("12");
                        created
                    }
                } else {
                    status = 404;
                    serde_json::json!({"apiVersion":"v1","kind":"Status","code":404,"reason":"NotFound","message":"gone","status":"Failure"})
                };
                let text = serde_json::to_string(&response).unwrap();
                let packet = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                stream.write_all(packet.as_bytes()).await.unwrap();
            }
        });
        let result = runtime
            .replace_builder_statefulset_controlled(&context, &old, desired)
            .await;
        server.abort();
        drop(server.await);
        result.expect("replacement after exact old Pod exit");
        assert_eq!(
            pod_reads.load(Ordering::SeqCst),
            2,
            "terminating Pod must be observed again until absent"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(method, _, _)| method == "POST")
                .count(),
            1
        );
        let delete = requests
            .iter()
            .find(|(method, _, _)| method == "DELETE")
            .unwrap();
        assert_eq!(delete.2["preconditions"]["resourceVersion"], "11");
    }
}
