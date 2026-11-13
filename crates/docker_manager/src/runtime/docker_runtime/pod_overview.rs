//! `/computer/pod/overview` 的 docker compose 实现（方案：docs/pod-overview.md P1）。
//!
//! 一次请求 = 1 次权威 list（bollard `all=true`，**不是** RCoder 内存登记表——
//! 未登记的 mysql/redis 等容器也必须出现）+ 身份关联（登记表按容器名对齐，
//! 查不到身份就留空，**不从名字猜造**）+ N 次 inspect（并发 8）+ events。
//!
//! 事件链的三个硬事实（独立复审 R2 证实）：
//! - **`since`/`until` 必须是 Unix 秒**：CLI 的 "1h" 写法由 CLI 换算，直接
//!   传 Engine API 返回 400（invalid timestamp）；
//! - **容器事件判别字段是顶层 `Type=container`**，不是 Attributes.container
//!   （后者只在 network connect 等场景出现，正常容器事件没有它）；
//! - **流错误必须传播**：吞成 break 会把 HTTP 500 伪装成"无事件"。
//!   timebox 到点或截断要标注 partial，不能当完整成功。
//!   另：官方只回放最近 256 条——"最近 1h"是覆盖上界，不是完整历史。

use std::collections::HashMap;

use bollard::models::{ContainerInspectResponse, ContainerState, EventMessageTypeEnum};
use bollard::query_parameters::{
    EventsOptionsBuilder, InspectContainerOptionsBuilder, ListContainersOptionsBuilder,
};
use container_runtime_api::AppEventInfo;
use container_runtime_api::pod_overview::{
    PodHealth, PodIdentity, PodOverviewDegraded, PodOverviewItem, PodOverviewSnapshot,
    PodResourcesSpec, PodVolumeSpec,
};
use futures_util::StreamExt;
use tracing::warn;

/// 事件采集窗口（秒）。docker 只回放最近 256 条，这是上界不是保证。
const EVENT_WINDOW_SECS: i64 = 3600;
/// timebox：events 是无限流，回放完会继续推实时事件。
const EVENT_TIMEBOX_MS: u64 = 300;
/// 防御性上限：异常 busy 的 daemon 不至于把内存吃爆。
const EVENT_HARD_CAP: usize = 2000;

impl super::DockerRuntime {
    /// 采集本 daemon 全部容器的概览（P1：spec + health + events，无 usage）。
    pub(crate) async fn pod_overview_inner(
        &self,
        events_limit: usize,
    ) -> container_runtime_api::ContainerRuntimeResult<PodOverviewSnapshot> {
        let mut degraded = Vec::new();
        let client = self.inner.get_docker_client();

        // 1) 权威列表（all=true，含停止的容器；失败即整体失败——没有列表
        //    这个查询没有意义）
        let list_options = ListContainersOptionsBuilder::new().all(true).build();
        let summary_list = client
            .list_containers(Some(list_options))
            .await
            .map_err(|e| {
                container_runtime_api::ContainerRuntimeError::DockerError(format!(
                    "list containers for overview: {e}"
                ))
            })?;

        // 2) 身份关联：登记表（内存缓存）按容器名对齐；查询失败按"无身份"
        //    处理（受管容器的身份缺失不影响未登记容器出现）。
        //    **直接用结构化槽位**，不再从 project_id 重新拆（复审 R3：
        //    重新拆会把 builder 的 app_id 拆成空串、丢 user_id）。
        let registered: HashMap<String, container_runtime_api::RuntimeContainerInfo> =
            match self.fetch_containers().await {
                Ok(list) => list
                    .into_iter()
                    .map(|info| (info.container_name.clone(), info))
                    .collect(),
                Err(e) => {
                    warn!("[POD_OVERVIEW] managed registry degraded: {e}");
                    degraded.push(PodOverviewDegraded {
                        section: "identity".to_string(),
                        reason: "managed_registry_failed".to_string(),
                        detail: Some(e.to_string()),
                    });
                    HashMap::new()
                }
            };

        // 3) 事件（全量收集，健康在截断前计算；失败降级但**如实标注**）
        let (full_events, events_partial) = match collect_recent_events(self).await {
            Ok((events, partial)) => (events, partial),
            Err(e) => {
                warn!("[POD_OVERVIEW] docker events degraded: {e}");
                degraded.push(PodOverviewDegraded {
                    section: "events".to_string(),
                    reason: "docker_events_failed".to_string(),
                    detail: Some(e),
                });
                (Vec::new(), false)
            }
        };
        if events_partial {
            degraded.push(PodOverviewDegraded {
                section: "events".to_string(),
                reason: "docker_events_partial".to_string(),
                detail: Some(
                    "timebox 或条数上限截断；且 docker 只回放最近 256 条，\
                     覆盖可能不足 1h"
                        .to_string(),
                ),
            });
        }

        // 4) 逐容器 inspect（并发 8；单个失败跳过——容器恰好在列表与检查之间被删）
        let names: Vec<String> = summary_list
            .iter()
            .filter_map(|c| c.names.as_ref().and_then(|n| n.first().cloned()))
            .map(|n| n.trim_start_matches('/').to_string())
            .collect();
        let inspections = inspect_all(self, &names).await;

        // 5) 组装
        let mut items = Vec::with_capacity(summary_list.len());
        for container in &summary_list {
            let Some(raw_name) = container.names.as_ref().and_then(|n| n.first().cloned()) else {
                continue;
            };
            let name = raw_name.trim_start_matches('/').to_string();
            let inspect = inspections.get(&name);
            let identity = registered.get(&name).and_then(|info| {
                info.service_type.map(|service_type| PodIdentity {
                    service_type: Some(service_type),
                    // 结构化槽位直读（R3）
                    user_id: info.user_id.clone(),
                    project_id: info.project_id.clone(),
                    app_id: info.app_id.clone(),
                    pod_id: info.pod_id.clone(),
                })
            });
            let state = inspect.and_then(|i| i.state.as_ref());
            // phase：Option 解开后的 Display（"running"/"exited"/…）——
            // format!("{:?}") 会输出 "Some(Running)"（R4）
            let phase = state
                .and_then(|s| s.status.as_ref())
                .map(|st| st.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let running = state.and_then(|s| s.running).unwrap_or(false);
            // 展示事件 = 全量按 limit 截断；健康用全量算（R6：展示参数
            // 不应改变健康计数）
            let display_events: Vec<AppEventInfo> = full_events
                .iter()
                .filter(|e| e.object == name)
                .take(events_limit)
                .cloned()
                .collect();
            let container_events: Vec<AppEventInfo> = full_events
                .iter()
                .filter(|e| e.object == name)
                .cloned()
                .collect();

            items.push(PodOverviewItem {
                name: name.clone(),
                workload: None, // docker：容器名即 workload 身份
                identity,
                phase,
                ready: running,
                node: None,
                // IP 在 networks map 的 EndpointSettings 里（顶层无 ip_address）
                ip: inspect
                    .and_then(|i| {
                        i.network_settings.as_ref().and_then(|n| {
                            n.networks
                                .as_ref()
                                .and_then(|nets| nets.values().next())
                                .and_then(|ep| ep.ip_address.clone())
                        })
                    })
                    .filter(|ip| !ip.is_empty()),
                created_at: inspect.and_then(|i| {
                    i.created.as_ref().and_then(|c| {
                        chrono::DateTime::parse_from_rfc3339(&c.to_string())
                            .ok()
                            .map(|dt| dt.with_timezone(&chrono::Utc))
                    })
                }),
                spec: docker_spec(inspect),
                health: docker_health(state, &container_events),
                events: display_events,
            });
        }

        Ok(PodOverviewSnapshot {
            pods: items,
            degraded,
            source: "docker".to_string(),
            namespace: None,
        })
    }
}

/// 并发 inspect（上限 8；单个失败跳过，不阻塞整体）。
async fn inspect_all(
    runtime: &super::DockerRuntime,
    names: &[String],
) -> HashMap<String, ContainerInspectResponse> {
    use std::sync::Arc;
    let client = Arc::new(runtime.inner.get_docker_client().clone());
    let mut out = HashMap::new();
    const CONCURRENCY: usize = 8;
    let mut stream = futures_util::stream::iter(names.to_vec())
        .map(|name| {
            let client = client.clone();
            async move {
                let options = InspectContainerOptionsBuilder::new().build();
                let result = client.inspect_container(&name, Some(options)).await;
                (name, result)
            }
        })
        .buffer_unordered(CONCURRENCY);
    while let Some((name, result)) = stream.next().await {
        match result {
            Ok(inspect) => {
                out.insert(name, inspect);
            }
            Err(e) => {
                // 容器可能在 list 与 inspect 之间被删除——跳过并留痕
                warn!("[POD_OVERVIEW] inspect {name} failed (skipped): {e}");
            }
        }
    }
    out
}

/// 事件收集的中间形态（保留纳秒序，排序后再转 AppEventInfo）。
struct RawEvent {
    time_nano: i64,
    action: String,
    container_name: String,
}

/// 收集最近 1h 的 docker **容器**事件。
///
/// 返回 `(按时间倒序的事件, 是否截断)`；请求/解码错误传播为 Err
/// （R2-3：吞错会把失败伪装成"无事件"）。
async fn collect_recent_events(
    runtime: &super::DockerRuntime,
) -> Result<(Vec<AppEventInfo>, bool), String> {
    let client = runtime.inner.get_docker_client();
    let now = chrono::Utc::now().timestamp();
    let options = EventsOptionsBuilder::new()
        .since(&(now - EVENT_WINDOW_SECS).to_string()) // Unix 秒（R2-1："1h" 是 CLI 写法，API 会 400）
        .until(&now.to_string())
        .build();
    let mut stream = client.events(Some(options));

    let mut raw: Vec<RawEvent> = Vec::new();
    let mut stream_failed: Option<String> = None;
    let mut truncated = false;
    let deadline = tokio::time::Duration::from_millis(EVENT_TIMEBOX_MS);
    let collected = tokio::time::timeout(deadline, async {
        while let Some(item) = stream.next().await {
            match item {
                // 流错误传播：HTTP 500/解码失败不能伪装成"无事件"（R2-3）
                Err(e) => {
                    stream_failed = Some(e.to_string());
                    break;
                }
                Ok(message) => {
                    // 容器事件判别 = 顶层 Type==container（R2-2）
                    if message.typ.as_ref() != Some(&EventMessageTypeEnum::CONTAINER) {
                        continue;
                    }
                    let (Some(action), Some(actor)) =
                        (message.action.as_ref(), message.actor.as_ref())
                    else {
                        continue;
                    };
                    let Some(container_name) = actor
                        .attributes
                        .as_ref()
                        .and_then(|attrs| attrs.get("name"))
                    else {
                        continue;
                    };
                    raw.push(RawEvent {
                        time_nano: message.time_nano.unwrap_or_default(),
                        action: action.clone(),
                        container_name: container_name.clone(),
                    });
                    if raw.len() >= EVENT_HARD_CAP {
                        truncated = true;
                        break;
                    }
                }
            }
        }
    })
    .await;
    if collected.is_err() {
        truncated = true; // timebox 到点：用已收到的部分
    }
    if let Some(error) = stream_failed {
        return Err(error);
    }

    // 纳秒序倒排（R9：同秒事件不被字符串序抹掉），再转对外形态
    raw.sort_by_key(|e| std::cmp::Reverse(e.time_nano));
    let events = raw
        .into_iter()
        .map(|e| {
            let ts = chrono::DateTime::from_timestamp(
                e.time_nano.div_euclid(1_000_000_000),
                e.time_nano.rem_euclid(1_000_000_000) as u32,
            )
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
            AppEventInfo {
                event_type: classify_event(&e.action),
                reason: e.action.clone(),
                message: format!("{} {}", e.action, e.container_name),
                timestamp: ts,
                object: e.container_name,
                count: 1,
            }
        })
        .collect();
    Ok((events, truncated))
}

/// 事件动作 → Warning/Normal（与 k8s 语义对齐：die/oom/kill/restart 是异常信号）。
fn classify_event(action: &str) -> String {
    match action {
        "die" | "oom" | "kill" | "restart" | "health_status" | "rename" => "Warning",
        _ => "Normal",
    }
    .to_string()
}

/// docker 规格：HostConfig + Mounts。**0 = 不限制 → None**。
fn docker_spec(inspect: Option<&ContainerInspectResponse>) -> PodResourcesSpec {
    let Some(inspect) = inspect else {
        return PodResourcesSpec::default();
    };
    let host_config = inspect.host_config.as_ref();
    let cpu_limit = host_config.and_then(|h| {
        if h.nano_cpus.unwrap_or(0) > 0 {
            Some(h.nano_cpus.unwrap_or_default() as f64 / 1e9)
        } else {
            let quota = h.cpu_quota.unwrap_or(0);
            let period = h.cpu_period.unwrap_or(0);
            if quota > 0 && period > 0 {
                Some(quota as f64 / period as f64)
            } else {
                None
            }
        }
    });
    // Memory == 0 = 不限制（不是 0MB）——这里最容易踩的语义坑
    let mem_limit = host_config
        .and_then(|h| h.memory.filter(|m| *m > 0))
        .map(|m| m / 1_048_576);
    let volumes = inspect
        .mounts
        .as_ref()
        .map(|mounts| {
            mounts
                .iter()
                .map(|m| PodVolumeSpec {
                    volume_type: "docker_mount".to_string(),
                    name: m
                        .destination
                        .as_deref()
                        .unwrap_or("")
                        .trim_start_matches('/')
                        .replace('/', "_"),
                    mount_path: m.destination.clone(),
                    capacity_mb: None, // docker 无配额概念
                    host_path: m.source.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    PodResourcesSpec {
        cpu_request_cores: None, // docker 没有 request 概念
        cpu_limit_cores: cpu_limit,
        mem_request_mb: None,
        mem_limit_mb: mem_limit,
        ephemeral_storage_limit_mb: None, // docker：writable-layer 大小可取（size=true），默认关
        volumes,
        containers: Vec::new(), // docker 单容器
    }
}

/// docker 健康。`events` 是该容器**未截断**的全量事件（倒序）。
///
/// 语义（R6/R7 修正）：
/// - **重启数** = die→start "恢复对" 数：一次策略重启（die,restart,start）
///   或手工 restart（die,start）各计 1；纯 stop（die 后没有 start）不计。
/// - **最近一次 OOM**：state.OOMKilled（当前状态）或**最近一条**终止类
///   事件（die/oom/kill 里最新的）是 oom；历史 OOM 不升格（R7）。
/// - **exit 137 不再推断 OOMKilled**：SIGKILL 不只来自 OOM（R7）。
fn docker_health(state: Option<&ContainerState>, events: &[AppEventInfo]) -> PodHealth {
    let restart_count = count_recovery_cycles(events);
    // 最近一条终止类事件（events 已倒序）
    let last_termination_event = events
        .iter()
        .find(|e| matches!(e.reason.as_str(), "die" | "oom" | "kill"));
    let last_event_is_oom = last_termination_event.is_some_and(|e| e.reason == "oom");
    let oom_killed_last = state.and_then(|s| s.oom_killed).unwrap_or(false) || last_event_is_oom;
    let exit_code = state.and_then(|s| s.exit_code).map(|c| c as i32);
    let last_terminate_reason = if oom_killed_last {
        Some("OOMKilled".to_string())
    } else {
        exit_code
            .filter(|code| *code != 0)
            .map(|code| format!("Error({code})"))
    };
    PodHealth {
        restart_count,
        restart_source: "docker_events".to_string(),
        last_terminate_reason,
        last_exit_code: exit_code,
        oom_killed_last,
        waiting_reason: None, // docker 无 CrashLoopBackOff 概念
        detail: state
            .and_then(|s| s.error.clone())
            .filter(|e| !e.is_empty()),
    }
}

/// 统计 die→start 恢复对（events 倒序输入，从旧往新扫）。
fn count_recovery_cycles(events: &[AppEventInfo]) -> u32 {
    let mut count = 0u32;
    let mut pending_die = false;
    for event in events.iter().rev() {
        // 升序扫描：die 置位，紧随其后的 start（无论中间有无 restart/kill）
        // 记一次恢复并复位；孤立的 die（容器停着没起来）不计
        match event.reason.as_str() {
            "die" | "oom" | "kill" => pending_die = true,
            "start" if pending_die => {
                count += 1;
                pending_die = false;
            }
            _ => {}
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_memory_means_unlimited_not_zero_mb() {
        let with = |memory: Option<i64>| ContainerInspectResponse {
            host_config: Some(bollard::models::HostConfig {
                memory,
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec = docker_spec(Some(&with(Some(0))));
        assert_eq!(spec.mem_limit_mb, None, "0 = 不限制，不是 0MB");
        let spec = docker_spec(Some(&with(Some(6 * 1_048_576 * 1024))));
        assert_eq!(spec.mem_limit_mb, Some(6144));
    }

    #[test]
    fn nano_cpus_converts_to_cores() {
        let response = ContainerInspectResponse {
            host_config: Some(bollard::models::HostConfig {
                nano_cpus: Some(1_500_000_000), // 1.5 核
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec = docker_spec(Some(&response));
        assert_eq!(spec.cpu_limit_cores, Some(1.5));
    }

    /// R6：一次策略重启（die,restart,start）只计 1，不是 2；
    /// 纯 stop（die 后无 start）不计。
    #[test]
    fn restart_counts_recovery_cycles_not_events() {
        let mk = |reason: &str, ts: &str| AppEventInfo {
            event_type: "Warning".into(),
            reason: reason.into(),
            message: format!("{reason} c"),
            timestamp: ts.into(),
            object: "c".into(),
            count: 1,
        };
        // 倒序输入（新→旧）：start,restart,die = 一次策略重启
        let policy_restart = vec![mk("start", "3"), mk("restart", "2"), mk("die", "1")];
        assert_eq!(
            count_recovery_cycles(&policy_restart),
            1,
            "die→(restart)→start 是一次重启"
        );
        // 手工 restart：die→start
        let manual = vec![mk("start", "2"), mk("die", "1")];
        assert_eq!(count_recovery_cycles(&manual), 1);
        // 纯 stop：die 后没起来
        let stopped = vec![mk("die", "1")];
        assert_eq!(count_recovery_cycles(&stopped), 0, "正常停止不是重启");
        // 两轮重启
        let twice = vec![
            mk("start", "5"),
            mk("die", "4"),
            mk("start", "3"),
            mk("die", "2"),
        ];
        assert_eq!(count_recovery_cycles(&twice), 2);
    }

    /// R7：历史 OOM 不升格为最近 OOM；最近的普通 die 在前时 oom=false。
    #[test]
    fn historical_oom_does_not_become_last_oom() {
        let mk = |reason: &str, ts: &str| AppEventInfo {
            event_type: "Warning".into(),
            reason: reason.into(),
            message: reason.into(),
            timestamp: ts.into(),
            object: "c".into(),
            count: 1,
        };
        // 倒序：最新是普通 die，更早有 oom
        let events = vec![
            mk("die", "2026-11-13T10:00:00Z"),
            mk("oom", "2026-11-13T09:00:00Z"),
        ];
        let state = ContainerState {
            oom_killed: Some(false),
            exit_code: Some(0),
            ..Default::default()
        };
        let health = docker_health(Some(&state), &events);
        assert!(
            !health.oom_killed_last,
            "历史 OOM + 最近普通退出 → 不是最近 OOM"
        );
        assert_eq!(health.last_terminate_reason, None);
    }

    /// R7：最近终止事件是 oom → oom_killed_last=true 且 reason=OOMKilled。
    #[test]
    fn recent_oom_event_sets_last_oom() {
        let mk = |reason: &str, ts: &str| AppEventInfo {
            event_type: "Warning".into(),
            reason: reason.into(),
            message: reason.into(),
            timestamp: ts.into(),
            object: "c".into(),
            count: 1,
        };
        let events = vec![
            mk("oom", "2026-11-13T10:00:00Z"),
            mk("die", "2026-11-13T09:00:00Z"),
        ];
        let state = ContainerState {
            oom_killed: Some(false),
            exit_code: Some(137),
            ..Default::default()
        };
        let health = docker_health(Some(&state), &events);
        assert!(health.oom_killed_last);
        assert_eq!(health.last_terminate_reason.as_deref(), Some("OOMKilled"));
    }

    /// R7：exit 137 单独出现不推断 OOM（SIGKILL 不只来自 OOM）。
    #[test]
    fn exit_137_alone_is_not_oom() {
        let state = ContainerState {
            oom_killed: Some(false),
            exit_code: Some(137),
            ..Default::default()
        };
        let health = docker_health(Some(&state), &[]);
        assert!(!health.oom_killed_last, "137 ≠ OOM");
        assert_eq!(health.last_terminate_reason.as_deref(), Some("Error(137)"));
    }

    #[test]
    fn classify_event_marks_die_oom_as_warning() {
        assert_eq!(classify_event("die"), "Warning");
        assert_eq!(classify_event("oom"), "Warning");
        assert_eq!(classify_event("start"), "Normal");
        assert_eq!(classify_event("pull"), "Normal");
    }
}
