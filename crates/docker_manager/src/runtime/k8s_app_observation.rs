//! UserApp 观测/操作（从 k8s_deployment.rs 拆出）：日志/事件/资源用量/exec。
//!
//! 这组方法只读 Pod（pods_api）+ Events/Metrics API，不触碰 Deployment，
//! 与 k8s_deployment.rs 的 UserApp Deployment 生命周期正交，故独立成模块。

use container_runtime_api::{ContainerLogEntry, ContainerRuntimeError, ContainerRuntimeResult};
use kube::api::{Api, ListParams};
use kube::core::{ApiResource, DynamicObject};
use tracing::{debug, warn};

use super::k8s_deployment::{APP_CONTAINER_NAME, RCODER_LABEL_PREFIX};
use super::KubernetesRuntime;

impl KubernetesRuntime {
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
    /// stdout/stderr 借 &mut self 顺序读(各自独立 DuplexStream,顺序不丢);exit code 从 Status 取。
    pub async fn app_exec(
        &self,
        app_id: &str,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        use kube::api::AttachParams;
        use tokio::io::AsyncReadExt;

        // 解析 exec 退出码:Status.reason == "NonZeroExitCode" 且 details.causes[] 有
        // reason == "ExitCode"(message 是退出码字符串)。无 NonZeroExitCode → 0。
        // 不用 status.code(那是 HTTP 状态码,非命令退出码)。
        fn parse_exit(status: k8s_openapi::apimachinery::pkg::apis::meta::v1::Status) -> i64 {
            // Status.reason == "NonZeroExitCode" 且 details.causes[] 有 reason == "ExitCode"
            // (其 message 是退出码字符串)。无 NonZeroExitCode → 0。
            if status.reason.as_deref() == Some("NonZeroExitCode")
                && let Some(details) = &status.details
                && let Some(causes) = &details.causes
            {
                for cause in causes {
                    if cause.reason.as_deref() == Some("ExitCode")
                        && let Some(msg) = &cause.message
                        && let Ok(code) = msg.parse::<i64>()
                    {
                        return code;
                    }
                }
            }
            0
        }

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

        // 2. exec(指定 app 容器,stdout+stderr,非 tty;调大 buffer 防 psql 输出反压)
        let ap = AttachParams::default()
            .container(APP_CONTAINER_NAME)
            .stdout(true)
            .stderr(true)
            .stdin(false)
            .tty(false)
            .max_stdout_buf_size(1024 * 1024)
            .max_stderr_buf_size(1024 * 1024);
        let mut attached = self
            .pods_api()
            .exec(&pod_name, command, &ap)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerExecError(format!("exec: {e}")))?;

        // 3. take_status 先移出(免后续 stdout/stderr 借用 &mut self 冲突)
        let mut status_fut = attached.take_status();

        // 4. 顺序读 stdout → stderr(借 &mut self,不能并发;各自独立 DuplexStream,顺序不丢)
        let mut stdout = String::new();
        if let Some(mut r) = attached.stdout() {
            let _ = r.read_to_string(&mut stdout).await;
        }
        let mut stderr = String::new();
        if let Some(mut r) = attached.stderr() {
            let _ = r.read_to_string(&mut stderr).await;
        }
        // readers 出作用域 drop(join 前 drop,防 DuplexStream 满死锁)

        // 5. exit code
        let exit_code = if let Some(fut) = status_fut.as_mut() {
            fut.await.map(parse_exit).unwrap_or(0)
        } else {
            0
        };

        // 6. join 收尾(reader 已 drop)
        if let Err(e) = attached.join().await {
            tracing::debug!("[K8S-APP] exec join: {e}");
        }

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

    /// 查询 app 相关的 K8s Events（调度/拉取/启动/崩溃）。
    /// 过滤 involvedObject.name 以 deployment 名开头的 events，按时间倒序，取最近 50 条。
    pub async fn app_events(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::AppEventInfo>> {
        use k8s_openapi::api::core::v1::Event;
        let deploy_name = self.app_deployment_name(app_id);
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
                // 只要关联对象名以 deployment 名开头（覆盖 Pod rcoder-app-{id}-xxx + Deployment 本身）
                if !name.starts_with(&deploy_name) {
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
        use container_runtime_api::ResourceUsage;

        // 1. 关联 Pod（UserApp 单副本；取第一个）
        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
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
