//! `/computer/pod/overview` 的 docker compose 实现（方案：docs/pod-overview.md P1）。
//!
//! 一次请求 = 1 次 `list_containers`（**复用既有实现**——身份真源是内存缓存
//! 的槽位还原，与 `/pod/list` 完全同源）+ N 次 inspect（并发 8）+ events(timebox)。
//!
//! 语义要点（与 k8s 实现对齐，来源不同）：
//! - **重启次数以 events 里 restart/die 计数为准**：`inspect.RestartCount`
//!   只统计重启策略（实测手工 `docker restart` 不计数），仅作参考不动它；
//! - **OOM**：`state.OOMKilled` 只反映"当前这次状态"；最近一次终止是否
//!   OOM 用 events 里最近一条 `oom` 事件判定；
//! - **`HostConfig.Memory == 0` = 不限制 → `mem_limit_mb = None`**（0 不是 0MB）；
//! - **events 是长连接**：`since=1h` 回放后继续推，必须 timebox（300ms）收口，
//!   否则请求挂死——这是 docker events 与 k8s list events 最大的语义差异。

use std::collections::HashMap;

use bollard::models::{ContainerInspectResponse, ContainerState};
use bollard::query_parameters::{EventsOptionsBuilder, InspectContainerOptionsBuilder};
use container_runtime_api::AppEventInfo;
use container_runtime_api::pod_overview::{
    PodHealth, PodIdentity, PodOverviewDegraded, PodOverviewItem, PodOverviewSnapshot,
    PodResourcesSpec, PodVolumeSpec, group_events_by_pod,
};
use futures_util::StreamExt;
use tracing::warn;

impl super::DockerRuntime {
    /// 采集本 daemon 全部容器的概览（P1：spec + health + events，无 usage）。
    pub(crate) async fn pod_overview_inner(
        &self,
        events_limit: usize,
    ) -> container_runtime_api::ContainerRuntimeResult<PodOverviewSnapshot> {
        let mut degraded = Vec::new();

        // 1) 列表 + 身份（复用既有 fetch_containers：内存缓存槽位还原，
        //    与 /pod/list 同源。失败即整体失败——没有列表这个查询没有意义）
        let containers = self.fetch_containers().await?;

        // 2) events（先取，后面按容器归组；失败降级）
        let events_by_container = match collect_recent_events(self, "1h", 300).await {
            Ok(events) => group_events_by_pod(&events, events_limit),
            Err(e) => {
                warn!("[POD_OVERVIEW] docker events degraded: {e}");
                degraded.push(PodOverviewDegraded {
                    section: "events".to_string(),
                    reason: "docker_events_failed".to_string(),
                    detail: Some(e),
                });
                HashMap::new()
            }
        };

        // 3) 逐容器 inspect（并发 8；单个失败跳过——容器恰好在列表与检查之间被删）
        let names: Vec<String> = containers
            .iter()
            .map(|c| c.container_name.clone())
            .collect();
        let inspections = inspect_all(self, &names).await;

        // 4) 组装
        let mut items = Vec::with_capacity(containers.len());
        for info in &containers {
            let name = info.container_name.clone();
            let inspect = inspections.get(&name);
            let identity = info.service_type.map(|service_type| {
                let slots = container_runtime_api::slots_from_identifier(
                    &service_type,
                    info.project_id.as_deref().unwrap_or(""),
                );
                PodIdentity {
                    service_type: Some(service_type),
                    user_id: slots.user_id,
                    project_id: slots.project_id,
                    app_id: slots.app_id,
                    pod_id: slots.pod_id,
                }
            });
            let state = inspect.and_then(|i| i.state.as_ref());
            let phase = state
                .map(|s| format!("{:?}", s.status))
                .unwrap_or_else(|| "unknown".to_string())
                .to_lowercase();
            let running = state.and_then(|s| s.running).unwrap_or(false);
            let events = events_by_container.get(&name).cloned().unwrap_or_default();

            items.push(PodOverviewItem {
                name: name.clone(),
                workload: None, // docker：容器名即 workload 身份
                identity,
                phase,
                ready: running,
                node: None,
                ip: Some(info.container_ip.clone()).filter(|ip| !ip.is_empty()),
                created_at: Some(info.created_at),
                spec: docker_spec(inspect),
                health: docker_health(state, &events),
                events,
            });
        }

        Ok(PodOverviewSnapshot {
            pods: items,
            degraded,
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

/// 收集最近 `since` 的 docker 事件，`timebox_ms` 后强制收口。
///
/// **必须 timebox**：`docker events` 是无限流，`since` 只决定回放起点，
/// 回放完会继续推送实时事件——没有 timebox 这个请求永远不返回。
async fn collect_recent_events(
    runtime: &super::DockerRuntime,
    since: &str,
    timebox_ms: u64,
) -> Result<Vec<AppEventInfo>, String> {
    let client = runtime.inner.get_docker_client();
    let options = EventsOptionsBuilder::new().since(since).build();
    let mut stream = client.events(Some(options));
    let mut events = Vec::new();
    let deadline = tokio::time::Duration::from_millis(timebox_ms);
    let collected = tokio::time::timeout(deadline, async {
        while let Some(item) = stream.next().await {
            let Ok(message) = item else { break };
            let Some(actor) = message.actor.as_ref() else {
                continue;
            };
            let Some(attributes) = actor.attributes.as_ref() else {
                continue;
            };
            // 只关心容器事件（镜像/网络/卷事件的 actor.id 不是容器）
            if !attributes.contains_key("container") && !attributes.contains_key("name") {
                continue;
            }
            let Some(container_name) = attributes.get("name") else {
                continue;
            };
            let action = message.action.clone().unwrap_or_default();
            events.push(AppEventInfo {
                event_type: classify_event(&action),
                reason: action.clone(),
                message: format!("{action} {container_name}"),
                timestamp: message.time.unwrap_or_default().to_string(),
                object: container_name.clone(),
                count: 1,
            });
            // 防御性上限：异常 busy 的 daemon 不至于把内存吃爆
            if events.len() >= 2000 {
                break;
            }
        }
    })
    .await;
    match collected {
        Ok(()) => Ok(events),
        Err(_elapsed) => Ok(events), // timebox 到点：用已收到的部分（正常路径）
    }
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
        // NanoCpus：1e9 = 1 核；为 0 时尝试 CpuQuota/CpuPeriod
        if h.nano_cpus.unwrap_or(0) > 0 {
            Some(h.nano_cpus.unwrap() as f64 / 1e9)
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

/// docker 健康：重启以 events 计数；OOM 看状态 + 最近 oom 事件。
fn docker_health(state: Option<&ContainerState>, events: &[AppEventInfo]) -> PodHealth {
    let restart_from_events = events
        .iter()
        .filter(|e| e.reason == "restart" || e.reason == "die")
        .count() as u32;
    let last_oom = events.iter().any(|e| e.reason == "oom");
    let oom_killed_last = state.and_then(|s| s.oom_killed).unwrap_or(false) || last_oom;
    let exit_code = state.and_then(|s| s.exit_code).map(|c| c as i32);
    let last_terminate_reason = exit_code.filter(|code| *code != 0).map(|code| {
        if code == 137 {
            "OOMKilled".to_string()
        } else {
            format!("Error({code})")
        }
    });
    PodHealth {
        restart_count: restart_from_events,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_memory_means_unlimited_not_zero_mb() {
        // HostConfig.Memory == 0 → mem_limit None（不是 0MB）
        let with = |memory: i64| ContainerInspectResponse {
            host_config: Some(bollard::models::HostConfig {
                memory: Some(memory),
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec = docker_spec(Some(&with(0)));
        assert_eq!(spec.mem_limit_mb, None, "0 = 不限制，不是 0MB");
        let spec = docker_spec(Some(&with(6 * 1_048_576 * 1024)));
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

    #[test]
    fn oom_exit_code_maps_to_oomkilled_reason() {
        let with_exit = |code: i64| ContainerState {
            exit_code: Some(code),
            ..Default::default()
        };
        let health = docker_health(Some(&with_exit(137)), &[]);
        assert_eq!(health.last_terminate_reason.as_deref(), Some("OOMKilled"));
        let health = docker_health(Some(&with_exit(1)), &[]);
        assert_eq!(health.last_terminate_reason.as_deref(), Some("Error(1)"));
    }

    #[test]
    fn restart_count_comes_from_events_not_restartcount() {
        // 手工 restart 不计入 RestartCount（实测），重启次数以 events 为准
        let events = vec![
            AppEventInfo {
                event_type: "Warning".into(),
                reason: "die".into(),
                message: "die x".into(),
                timestamp: "3".into(),
                object: "c".into(),
                count: 1,
            },
            AppEventInfo {
                event_type: "Warning".into(),
                reason: "restart".into(),
                message: "restart x".into(),
                timestamp: "4".into(),
                object: "c".into(),
                count: 1,
            },
            AppEventInfo {
                event_type: "Normal".into(),
                reason: "start".into(),
                message: "start x".into(),
                timestamp: "5".into(),
                object: "c".into(),
                count: 1,
            },
        ];
        let health = docker_health(None, &events);
        assert_eq!(health.restart_count, 2, "die + restart 计数，start 不计");
        assert_eq!(health.restart_source, "docker_events");
    }

    #[test]
    fn classify_event_marks_die_oom_as_warning() {
        assert_eq!(classify_event("die"), "Warning");
        assert_eq!(classify_event("oom"), "Warning");
        assert_eq!(classify_event("start"), "Normal");
        assert_eq!(classify_event("pull"), "Normal");
    }
}
