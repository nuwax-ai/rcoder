//! Userapp 观测/操作（从 k8s_deployment.rs 拆出）：日志/事件/资源用量/exec。
//!
//! 这组方法只读 Pod（pods_api）+ Events/Metrics API，不触碰 Deployment，
//! 与 k8s_deployment.rs 的 Userapp Deployment 生命周期正交，故独立成模块。

use container_runtime_api::{ContainerLogEntry, ContainerRuntimeError, ContainerRuntimeResult};
use kube::api::{Api, ListParams};
use kube::core::{ApiResource, DynamicObject};
use tracing::{debug, warn};

use super::KubernetesRuntime;
use super::k8s_deployment::{APP_CONTAINER_NAME, RCODER_LABEL_PREFIX};

fn exec_exit_code(
    status: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Status>,
) -> ContainerRuntimeResult<i64> {
    let unknown = || {
        ContainerRuntimeError::ContainerExecError(
            "Exec completion was not confirmed; command outcome is unknown".into(),
        )
    };
    let status = status.ok_or_else(unknown)?;
    if status.status.as_deref() == Some("Success") && status.reason.is_none() {
        return Ok(0);
    }
    if status.status.as_deref() == Some("Failure")
        && status.reason.as_deref() == Some("NonZeroExitCode")
        && let Some(details) = status.details
        && let Some(causes) = details.causes
    {
        let codes: Vec<_> = causes
            .into_iter()
            .filter(|cause| cause.reason.as_deref() == Some("ExitCode"))
            .map(|cause| cause.message)
            .collect();
        if codes.len() == 1
            && let Some(message) = &codes[0]
            && let Ok(code) = message.parse::<i64>()
            && (1..=255).contains(&code)
        {
            return Ok(code);
        }
    }
    Err(unknown())
}

async fn read_exec_output<R: tokio::io::AsyncRead + Unpin>(
    stream: Option<R>,
) -> ContainerRuntimeResult<String> {
    use tokio::io::AsyncReadExt;
    let mut stream = stream.ok_or_else(|| {
        ContainerRuntimeError::ContainerExecError(
            "Requested exec output stream is missing; command outcome is unknown".into(),
        )
    })?;
    let mut output = String::new();
    stream.read_to_string(&mut output).await.map_err(|error| {
        ContainerRuntimeError::ContainerExecError(format!(
            "Read exec output: {error}; command outcome is unknown"
        ))
    })?;
    Ok(output)
}

impl KubernetesRuntime {
    pub(super) async fn capture_configuration_pod(
        &self,
        context: &shared_types::UserAppExecutionContext,
        generation: &str,
    ) -> ContainerRuntimeResult<shared_types::RuntimeConfigurationTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if generation.is_empty() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Configuration generation is required".into(),
            ));
        }
        // Confirm the current lifecycle owns the workload even when no Pod is
        // running. Absence must not mask a foreign/replaced workload.
        self.capture_owned_app_identity(context, None).await?;
        let pods = self
            .pods_api()
            .list(
                &ListParams::default()
                    .labels(&format!("{RCODER_LABEL_PREFIX}/app-id={}", context.app_id)),
            )
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Capture configuration Pod: {error}"))
            })?;
        let candidates: Vec<_> = pods
            .items
            .into_iter()
            .filter(|pod| {
                pod.metadata.deletion_timestamp.is_none()
                    && pod
                        .metadata
                        .annotations
                        .as_ref()
                        .and_then(|values| {
                            values.get(super::k8s_app_helpers::DEPLOY_TEMPLATE_TOKEN_ANNOTATION)
                        })
                        .is_some_and(|token| token == generation)
                    && pod
                        .status
                        .as_ref()
                        .and_then(|status| status.container_statuses.as_ref())
                        .is_some_and(|statuses| {
                            statuses.iter().any(|status| {
                                status.name == APP_CONTAINER_NAME
                                    && status
                                        .state
                                        .as_ref()
                                        .is_some_and(|state| state.running.is_some())
                            })
                        })
            })
            .collect();
        if candidates.is_empty() {
            return Err(ContainerRuntimeError::ManagementNotRunning);
        }
        if candidates.len() != 1 {
            return Err(ContainerRuntimeError::Conflict(
                "Expected one running management container for the captured deployment operation"
                    .into(),
            ));
        }
        let target = shared_types::RuntimeConfigurationTarget {
            physical_uid: candidates[0]
                .metadata
                .uid
                .clone()
                .filter(|uid| !uid.is_empty())
                .ok_or_else(|| {
                    ContainerRuntimeError::Conflict("Configuration Pod UID is missing".into())
                })?,
            deployment_generation: generation.into(),
        };
        // Read-only in-container check validates both the owner chain and the
        // frozen generation. It deliberately does not wait for business Ready.
        let probe = self
            .app_configuration_exec(context, &target, vec!["true".into()])
            .await?;
        if probe.exit_code != 0 {
            return Err(ContainerRuntimeError::Conflict(
                "Configuration Pod generation check failed".into(),
            ));
        }
        Ok(target)
    }

    /// Management exec may run before business Ready. The API resolves exec by
    /// name, so check the captured UID again inside the destination container.
    pub(super) async fn app_configuration_exec(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        if target.physical_uid.is_empty()
            || target.deployment_generation.is_empty()
            || command.is_empty()
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Configuration exec requires a physical UID, generation and command".into(),
            ));
        }
        let deployment = self.capture_owned_app_identity(context, None).await?;
        let pods = self
            .pods_api()
            .list(
                &ListParams::default()
                    .labels(&format!("{RCODER_LABEL_PREFIX}/app-id={}", context.app_id)),
            )
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("List configuration target: {error}"))
            })?;
        let pod = pods
            .items
            .into_iter()
            .find(|pod| pod.metadata.uid.as_deref() == Some(target.physical_uid.as_str()))
            .ok_or_else(|| {
                ContainerRuntimeError::Conflict(
                    "Captured configuration Pod no longer exists".into(),
                )
            })?;
        let conflict = || {
            ContainerRuntimeError::Conflict(
                "Configuration Pod ownership or management identity changed".into(),
            )
        };
        let pod_name = pod.metadata.name.as_deref().ok_or_else(conflict)?;
        if pod.metadata.deletion_timestamp.is_some() {
            return Err(conflict());
        }
        let owner = pod
            .metadata
            .owner_references
            .as_ref()
            .and_then(|owners| {
                owners.iter().find(|owner| {
                    owner.controller == Some(true)
                        && owner.kind == "ReplicaSet"
                        && owner.api_version == "apps/v1"
                })
            })
            .ok_or_else(conflict)?;
        let replicasets: Api<k8s_openapi::api::apps::v1::ReplicaSet> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let replicaset = replicasets.get(&owner.name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Read configuration Pod owner: {error}"))
        })?;
        if replicaset.metadata.uid.as_deref() != Some(owner.uid.as_str())
            || !replicaset
                .metadata
                .owner_references
                .as_ref()
                .is_some_and(|owners| {
                    owners.iter().any(|owner| {
                        owner.controller == Some(true)
                            && owner.kind == "Deployment"
                            && owner.api_version == "apps/v1"
                            && owner.name == deployment.name
                            && owner.uid == deployment.uid
                    })
                })
        {
            return Err(conflict());
        }
        let container = pod
            .spec
            .as_ref()
            .and_then(|spec| {
                spec.containers
                    .iter()
                    .find(|container| container.name == APP_CONTAINER_NAME)
            })
            .ok_or_else(conflict)?;
        let uid_fields: Vec<_> = container
            .env
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|env| env.name == "RCODER_PHYSICAL_POD_UID")
            .collect();
        if uid_fields.len() != 1
            || uid_fields[0].value.is_some()
            || !uid_fields[0]
                .value_from
                .as_ref()
                .and_then(|source| source.field_ref.as_ref())
                .is_some_and(|field| field.field_path == "metadata.uid")
        {
            return Err(conflict());
        }
        // Positional arguments preserve arbitrary command arguments without shell
        // interpolation. Frozen Pod environment checks also cover a replacement
        // occurring after the API reads above and before the exec connection.
        self.exec_app_pod(pod_name, configuration_guard_command(target, command))
            .await
    }

    /// 拉取 app Pod 的 stdout/stderr 日志（最近 `tail` 行）。
    /// 按 `rcoder.io/app-id` label 定位 Pod；`timestamps=true` 时 K8s 在每行前缀 RFC3339。
    /// K8s logs API 合并 stdout/stderr 返回，stream 统一记 "stdout"。
    pub async fn app_logs(
        &self,
        app_id: &str,
        tail: u32,
        timestamps: bool,
    ) -> ContainerRuntimeResult<Vec<ContainerLogEntry>> {
        use kube::api::LogParams;
        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
        let pods = self
            .pods_api()
            .list(&lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("list pods for logs: {e}")))?;
        // 无 Pod（app stopped / 副本缩为 0）→ 返回空，与 Docker 侧"容器不存在→空日志"一致，
        // 避免 stopped app 查日志被误报 404（应用还在，只是当前无运行实例）。
        let Some(pod_name) = pods
            .items
            .into_iter()
            .next()
            .and_then(|p| p.metadata.name.clone())
        else {
            return Ok(vec![]);
        };
        let log_lp = LogParams {
            tail_lines: Some(tail as i64),
            timestamps,
            ..Default::default()
        };
        let raw = self
            .pods_api()
            .logs(&pod_name, &log_lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("pod logs: {e}")))?;
        // K8s logs API 合并 stdout/stderr，stream 统一记 "stdout"
        Ok(raw
            .lines()
            .map(|l| {
                let (ts, msg) = container_runtime_api::split_log_timestamp(l, timestamps);
                ContainerLogEntry {
                    timestamp: ts,
                    stream: "stdout".to_string(),
                    message: msg,
                }
            })
            .collect())
    }

    /// 在 app Pod 内执行命令(kubectl exec 等价):Pod 定位(label)→ Api::exec → AttachedProcess。
    /// 用于数据库管理(reset-password / create-database 跑 psql)。
    /// 并发排空 stdout/stderr，只有明确的远端 Status 才能确认退出码。
    pub async fn app_exec(
        &self,
        app_id: &str,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        // 1. Pod 定位(复用 app_logs 的 label selector)
        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
        let pods = self
            .pods_api()
            .list(&lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("list pods for exec: {e}")))?;
        let Some(pod_name) = pods
            .items
            .into_iter()
            .next()
            .and_then(|p| p.metadata.name.clone())
        else {
            // exec 是写操作,需活 Pod;无 Pod(app stopped)→ ContainerNotFound
            return Err(ContainerRuntimeError::ContainerNotFound(format!(
                "no running pod for app {app_id}"
            )));
        };

        self.exec_app_pod(&pod_name, command).await
    }

    async fn exec_app_pod(
        &self,
        pod_name: &str,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        self.exec_pod_container(pod_name, APP_CONTAINER_NAME, command)
            .await
    }

    pub(super) async fn exec_pod_container(
        &self,
        pod_name: &str,
        container_name: &str,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        use kube::api::AttachParams;
        // Drain both streams even for management commands with large diagnostics.
        let ap = AttachParams::default()
            .container(container_name)
            .stdout(true)
            .stderr(true)
            .stdin(false)
            .tty(false)
            .max_stdout_buf_size(1024 * 1024)
            .max_stderr_buf_size(1024 * 1024);
        let mut attached = self
            .pods_api()
            .exec(pod_name, command, &ap)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerExecError(format!("exec: {e}")))?;

        // Readers returned by kube 4.2 own their pipes. Drain both concurrently:
        // sequential reads can deadlock when the remote process fills stderr
        // while we are waiting for stdout EOF.
        let status = attached.take_status();
        let stdout = attached.stdout();
        let stderr = attached.stderr();
        let (stdout, stderr, exit_code) =
            tokio::try_join!(read_exec_output(stdout), read_exec_output(stderr), async {
                match status {
                    Some(status) => exec_exit_code(status.await),
                    None => exec_exit_code(None),
                }
            })?;
        attached.join().await.map_err(|error| {
            ContainerRuntimeError::ContainerExecError(format!(
                "Join exec transport: {error}; command outcome is unknown"
            ))
        })?;

        Ok(container_runtime_api::ExecResult {
            stdout,
            stderr,
            exit_code,
        })
    }

    /// 启动日志流（follow）：返回 mpsc::Receiver。内部 spawn 任务读 K8s `log_stream(follow)`，
    /// 逐行 send 到 channel。receiver drop（客户端断开）→ send 出错 → 任务退出释放日志源。
    ///
    /// 命名 `_inner` 与同文件 `app_logs`/`scale_app`/`restart_app` 约定一致（trait 同名方法
    /// 转调不同名的 inherent，避免 trait impl 内 self.同名() 依赖方法解析优先级）。
    pub async fn stream_app_logs_inner(
        &self,
        app_id: &str,
        tail: u32,
    ) -> ContainerRuntimeResult<container_runtime_api::mpsc::Receiver<ContainerLogEntry>> {
        use futures_util::{AsyncBufReadExt, StreamExt};
        use kube::api::LogParams;

        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
        let pods = self.pods_api().list(&lp).await.map_err(|e| {
            ContainerRuntimeError::K8sError(format!("list pods for log stream: {e}"))
        })?;
        let pod_name = pods
            .items
            .into_iter()
            .next()
            .and_then(|p| p.metadata.name.clone())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(format!(
                    "app {app_id} 当前无运行 Pod（可能已 stopped）"
                ))
            })?;
        let timestamps = true;
        let log_lp = LogParams {
            tail_lines: if tail > 0 { Some(tail as i64) } else { None },
            follow: true,
            timestamps,
            ..Default::default()
        };
        let reader = self
            .pods_api()
            .log_stream(&pod_name, &log_lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("log_stream: {e}")))?;
        let (tx, rx) = container_runtime_api::mpsc::channel::<ContainerLogEntry>(64);
        tokio::spawn(async move {
            // kube log_stream 返回 futures_util::AsyncBufRead；lines() 返回 Stream<Item=io::Result<String>>。
            // Box::pin 保证 Unpin（lines 需 Self: Unpin）。
            let reader = Box::pin(reader);
            let mut lines = reader.lines();
            while let Some(result) = lines.next().await {
                match result {
                    Ok(line) => {
                        let (ts, msg) =
                            container_runtime_api::split_log_timestamp(&line, timestamps);
                        let entry = ContainerLogEntry {
                            timestamp: ts,
                            stream: "stdout".to_string(),
                            message: msg,
                        };
                        if tx.send(entry).await.is_err() {
                            break; // 客户端断开，receiver 已 drop
                        }
                    }
                    Err(e) => {
                        warn!("[K8S-APP] log_stream 读行失败 (终止流): {e}");
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }

    /// 查询 app 相关的 K8s Events（调度/拉取/启动/崩溃），按时间倒序，取最近 50 条。
    ///
    /// 归属判定 = **确定性对象名单 + 等值匹配**（不用名字前缀）：
    /// - 名单一：六个确定性派生对象名（Deployment 本体 / -config / -secret /
    ///   -svc / -route / -nodeport）+ workspace PVC；
    /// - 名单二：该 app 的 Pod 名集合（Deployment hash 名不可预测，按
    ///   `rcoder.io/app-id` 标签实查一次）。
    ///
    /// 前缀匹配已被证伪：`rcoder-app-3` 是 `rcoder-app-39-xxx` 的前缀，数字型
    /// app_id 互为前缀即串台；等值名单对任何 app_id 形态恒正确。
    pub async fn app_events(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::AppEventInfo>> {
        use k8s_openapi::api::core::v1::Event;
        let object_names = self.app_event_object_names(app_id).await;
        let events: Api<Event> = Api::namespaced(self.client.clone(), &self.namespace);
        // list namespace 内所有 events（K8s 默认保留 ~1h，数量有限）
        let list = events
            .list(&ListParams::default())
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("list events: {e}")))?;
        let mut result: Vec<_> = list
            .items
            .into_iter()
            .filter_map(|ev| {
                let name = ev.involved_object.name.as_ref()?;
                if !event_belongs_to_app(name, &object_names) {
                    return None;
                }
                Some(container_runtime_api::AppEventInfo {
                    event_type: ev.type_.clone().unwrap_or_else(|| "Normal".to_string()),
                    reason: ev.reason.clone().unwrap_or_default(),
                    message: ev.message.clone().unwrap_or_default(),
                    timestamp: ev
                        .last_timestamp
                        .as_ref()
                        .map(|t| t.0.to_string())
                        .unwrap_or_default(),
                    object: name.clone(),
                    count: ev.count.unwrap_or(1),
                })
            })
            .collect();
        // 按时间倒序
        result.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        result.truncate(50);
        Ok(result)
    }

    /// app 全部关联 K8s 对象名集合（事件归属白名单）。
    async fn app_event_object_names(&self, app_id: &str) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::from([
            self.app_deployment_name(app_id),
            self.app_config_name(app_id),
            self.app_secret_name(app_id),
            self.app_service_name(app_id),
            self.app_http_route_name(app_id),
            self.app_nodeport_name(app_id),
        ]);
        if let Ok(pvc) = self.app_workspace_pvc_name(app_id) {
            names.insert(pvc);
        }
        // Pod 名 = {deploy}-{rs-hash}-{pod-hash} 不可预测——按 app-id 标签实查
        // （resource_version=0 走 watch cache，与 fetch_app_pod_info 一致——
        // 事件归属名单只需名字，不需要最新版本）
        let lp = ListParams {
            label_selector: Some(format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX)),
            resource_version: Some("0".to_string()),
            ..Default::default()
        };
        if let Ok(pods) = self.pods_api().list(&lp).await {
            for pod in pods.items {
                if let Some(name) = pod.metadata.name {
                    names.insert(name);
                }
            }
        }
        names
    }

    /// 查询 app 实时资源用量（CPU/内存）。
    ///
    /// 用量来自 metrics.k8s.io PodMetrics（k8s-openapi 无此类型，用 DynamicObject 查）；
    /// 限额来自关联 Pod 的 `containers[].resources.limits`。各容器求和。network 不含
    /// （metrics.k8s.io 不提供）。metrics 查询失败（无 metrics-server / 403 / pod 刚起未采集）
    /// 降级为用量 0（不报错，由 app_manager 层组装为 0），保证 stats 接口不 500。
    pub async fn app_resource_usage(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<container_runtime_api::ResourceUsage> {
        self.app_resource_usage_by_labels(
            app_id,
            format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX),
        )
        .await
    }

    /// 按双键标准标签定位任意 rcoder 托管容器的资源用量——开发容器
    /// （UserappBuilder STS）stats 的 dev 环境入口：builder 由
    /// `build_standard_labels` 打标，同 app_id 下与 prod Deployment 以
    /// service-type 维度区分，零创建链改动即可被 metrics API 命中。
    pub async fn app_resource_usage_for(
        &self,
        app_id: &str,
        service_type: &shared_types::ServiceType,
    ) -> ContainerRuntimeResult<container_runtime_api::ResourceUsage> {
        self.app_resource_usage_by_labels(
            app_id,
            format!(
                "app.kubernetes.io/instance={app_id},{}/service-type={service_type}",
                RCODER_LABEL_PREFIX
            ),
        )
        .await
    }

    /// 资源用量采集内核：按 label selector 定位 Pod（单副本取第一个），
    /// PodMetrics 用量 + pod spec limits 限额。
    async fn app_resource_usage_by_labels(
        &self,
        app_id: &str,
        label_selector: String,
    ) -> ContainerRuntimeResult<container_runtime_api::ResourceUsage> {
        use container_runtime_api::ResourceUsage;

        // 1. 关联 Pod（Userapp 单副本；取第一个）
        let lp = ListParams::default().labels(&label_selector);
        let pod = self
            .pods_api()
            .list(&lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("list pods for metrics: {e}")))?
            .items
            .into_iter()
            .next();
        let Some(pod) = pod else {
            return Ok(ResourceUsage::default()); // 无 Pod（app 未起/已删）→ 0
        };
        let pod_name = pod.metadata.name.clone().unwrap_or_default();

        // 2. 用量：metrics.k8s.io/v1beta1 PodMetrics（DynamicObject，取 data["containers"])
        let mut usage_cpu = 0.0f64;
        let mut usage_mem = 0u64;
        let ar = ApiResource {
            group: "metrics.k8s.io".to_string(),
            version: "v1beta1".to_string(),
            api_version: "metrics.k8s.io/v1beta1".to_string(),
            plural: "pods".to_string(),
            kind: "PodMetrics".to_string(),
        };
        let metrics_api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.namespace, &ar);
        match metrics_api.get(&pod_name).await {
            Ok(dynobj) => {
                if let Some(containers) = dynobj.data.get("containers").and_then(|c| c.as_array()) {
                    for c in containers {
                        if let Some(usage) = c.get("usage") {
                            if let Some(cpu) = usage.get("cpu").and_then(|v| v.as_str()) {
                                usage_cpu += shared_types::parse_cpu_quantity(cpu).unwrap_or(0.0);
                            }
                            if let Some(mem) = usage.get("memory").and_then(|v| v.as_str()) {
                                usage_mem += shared_types::parse_memory_quantity(mem).unwrap_or(0);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // metrics 查询失败（无 metrics-server / RBAC / pod 刚起未采集）→ 用量降级 0，不报错
                debug!(
                    "[K8S-APP] metrics query failed app_id={} pod={}: {}",
                    app_id, pod_name, e
                );
            }
        }

        // 3. 限额：关联 Pod 的 resources.limits（app 容器 + sidecar 求和；无 limit 则 0 → 百分比 0）
        let mut limit_cpu = 0.0f64;
        let mut limit_mem = 0u64;
        if let Some(spec) = &pod.spec {
            for c in &spec.containers {
                if let Some(limits) = c.resources.as_ref().and_then(|r| r.limits.as_ref()) {
                    if let Some(q) = limits.get("cpu") {
                        limit_cpu += shared_types::parse_cpu_quantity(&q.0).unwrap_or(0.0);
                    }
                    if let Some(q) = limits.get("memory") {
                        limit_mem += shared_types::parse_memory_quantity(&q.0).unwrap_or(0);
                    }
                }
            }
        }

        Ok(ResourceUsage {
            cpu_usage_cores: usage_cpu,
            mem_usage_bytes: usage_mem,
            cpu_limit_cores: limit_cpu,
            mem_limit_bytes: limit_mem,
        })
    }
}

fn configuration_guard_command(
    target: &shared_types::RuntimeConfigurationTarget,
    command: Vec<String>,
) -> Vec<String> {
    let mut guarded = vec![
        "sh".into(), "-c".into(),
        "if [ \"${RCODER_PHYSICAL_POD_UID:-}\" != \"$1\" ] || [ \"${APP_DEPLOY_GENERATION_ID:-}\" != \"$2\" ]; then printf '%s\\n' 'Configuration target identity changed' >&2; exit 125; fi; shift 2; exec \"$@\"".into(),
        "rcoder-configuration-exec".into(), target.physical_uid.clone(), target.deployment_generation.clone(),
    ];
    guarded.extend(command);
    guarded
}

/// 事件关联对象名是否属于该 app（等值白名单匹配，纯函数）。
fn event_belongs_to_app(name: &str, object_names: &std::collections::HashSet<String>) -> bool {
    object_names.contains(name)
}

#[cfg(test)]
mod app_event_tests {
    use super::event_belongs_to_app;
    use std::collections::HashSet;

    /// app 3 的名单必须覆盖六派生名 + Pod 名，且不含 app 39 的任何对象名——
    /// 数字型 app_id 互为前缀是旧 `starts_with` 方案的串台根源，等值匹配恒免疫。
    #[test]
    fn exact_name_list_isolates_numeric_prefix_app_ids() {
        let names: HashSet<String> = [
            "rcoder-app-3",
            "rcoder-app-3-config",
            "rcoder-app-3-secret",
            "rcoder-app-3-svc",
            "rcoder-app-3-route",
            "rcoder-app-3-nodeport",
            "rcoder-app-3-6f9c8d7b5-x2p4q", // Pod（hash 名，来自标签实查）
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        for hit in [
            "rcoder-app-3",
            "rcoder-app-3-svc",
            "rcoder-app-3-6f9c8d7b5-x2p4q",
        ] {
            assert!(event_belongs_to_app(hit, &names), "{hit} should match");
        }
        // app 39 的对象名（deployment/pod/svc）一个都不命中
        for miss in [
            "rcoder-app-39",
            "rcoder-app-39-64dd4c4879-w6lrt",
            "rcoder-app-39-svc",
            "rcoder-app-39-config",
        ] {
            assert!(!event_belongs_to_app(miss, &names), "{miss} must NOT match");
        }
        // builder pod（STS 名形态）也不在 app 名单内
        assert!(!event_belongs_to_app("rcoder-app-builder-3-0", &names));
    }
}

#[cfg(test)]
mod exec_completion_tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;

    #[cfg(unix)]
    #[test]
    fn replacement_pod_cannot_execute_a_captured_credential_command() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("executed");
        let target = shared_types::RuntimeConfigurationTarget {
            physical_uid: "original-pod".into(),
            deployment_generation: "generation-one".into(),
        };
        // The payload intentionally contains shell metacharacters. It remains a
        // positional argument and is evaluated only by the intended inner shell.
        let arguments = configuration_guard_command(
            &target,
            vec![
                "sh".into(),
                "-c".into(),
                "printf '%s' \"$2\" > \"$1\"".into(),
                "payload".into(),
                marker.to_string_lossy().into_owned(),
                "literal;$(false)".into(),
            ],
        );
        for (uid, generation, allowed) in [
            ("replacement-pod", "generation-one", false),
            ("original-pod", "generation-two", false),
            ("", "generation-one", false),
            ("original-pod", "generation-one", true),
        ] {
            let output = std::process::Command::new(&arguments[0])
                .args(&arguments[1..])
                .env("RCODER_PHYSICAL_POD_UID", uid)
                .env("APP_DEPLOY_GENERATION_ID", generation)
                .output()
                .unwrap();
            assert_eq!(output.status.success(), allowed);
            assert_eq!(marker.exists(), allowed);
            if allowed {
                assert_eq!(
                    std::fs::read_to_string(&marker).unwrap(),
                    "literal;$(false)"
                );
            } else {
                assert_eq!(output.status.code(), Some(125));
            }
        }
    }

    #[test]
    fn only_explicit_exec_status_confirms_exit() {
        let success: Status =
            serde_json::from_value(serde_json::json!({"status":"Success"})).unwrap();
        assert_eq!(exec_exit_code(Some(success)).unwrap(), 0);
        let nonzero: Status = serde_json::from_value(serde_json::json!({
            "status":"Failure", "reason":"NonZeroExitCode",
            "details":{"causes":[{"reason":"ExitCode","message":"137"}]}
        }))
        .unwrap();
        assert_eq!(exec_exit_code(Some(nonzero)).unwrap(), 137);
        assert!(exec_exit_code(None).is_err());
        for value in [
            serde_json::json!({}),
            serde_json::json!({"status":"Failure","reason":"InternalError"}),
            serde_json::json!({"status":"Failure","reason":"NonZeroExitCode"}),
            serde_json::json!({"status":"Success","reason":"NonZeroExitCode"}),
            serde_json::json!({"status":"Failure","reason":"NonZeroExitCode",
                "details":{"causes":[{"reason":"ExitCode"},{"reason":"ExitCode","message":"1"}]}}),
        ] {
            assert!(exec_exit_code(Some(serde_json::from_value(value).unwrap())).is_err());
        }
        for code in ["0", "-1", "256", "invalid"] {
            let status = serde_json::from_value(serde_json::json!({
                "status":"Failure", "reason":"NonZeroExitCode",
                "details":{"causes":[{"reason":"ExitCode","message":code}]}
            }))
            .unwrap();
            assert!(exec_exit_code(Some(status)).is_err());
        }
    }

    #[tokio::test]
    async fn missing_or_truncated_output_cannot_report_success() {
        assert!(
            read_exec_output::<std::io::Cursor<Vec<u8>>>(None)
                .await
                .is_err()
        );
        assert!(
            read_exec_output(Some(std::io::Cursor::new(vec![0xff])))
                .await
                .is_err()
        );
    }
}
