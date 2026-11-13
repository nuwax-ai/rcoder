//! `/computer/pod/overview` 的 k8s 实现（方案：docs/pod-overview.md P1）。
//!
//! 一次请求 = 3 次 list（Pods / PVC / Events）+ 0 次 gRPC（usage 是 P2）：
//! - Pods：**无 label 过滤**（与 `list_containers_inner` 的
//!   `RUNTIME_MANAGED_LABEL` 不同——概览要看全部，含 mysql/redis 等非托管组件）；
//! - PVC：一次 list 建 `name -> capacity` 索引，填 `volumes[].capacity_mb`；
//! - Events：`fieldSelector=involvedObject.kind=Pod` 服务端收窄后按 Pod 归组。
//!
//! 任一分区失败 → `degraded` 标注 + 该分区缺省，**不失败整个快照**；
//! Pods 列表失败 → 整体 Err（没有列表，这个接口没有意义）。

use std::collections::HashMap;

use container_runtime_api::AppEventInfo;
use container_runtime_api::pod_overview::{
    PodHealth, PodIdentity, PodOverviewDegraded, PodOverviewItem, PodOverviewSnapshot,
    PodResourcesSpec, PodVolumeSpec, PodWorkload, group_events_by_pod,
};
use k8s_openapi::api::core::v1::{Event, PersistentVolumeClaim, Pod};
use kube::api::{Api, ListParams};
use tracing::warn;

use crate::runtime::k8s_agent_query::pod_container_diagnostic;

impl super::KubernetesRuntime {
    /// 采集命名空间内全部 Pod 的概览（P1：spec + health + events，无 usage）。
    pub(crate) async fn pod_overview_inner(
        &self,
        events_limit: usize,
    ) -> container_runtime_api::ContainerRuntimeResult<PodOverviewSnapshot> {
        let mut degraded = Vec::new();

        // 1) Pods（权威列表；失败即整体失败）
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_list = pods.list(&ListParams::default()).await.map_err(|e| {
            container_runtime_api::ContainerRuntimeError::K8sError(format!(
                "list pods for overview: {e}"
            ))
        })?;

        // 2) PVC 索引（分区降级）
        let pvc_api: Api<PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let pvc_capacity: HashMap<String, i64> = match pvc_api.list(&ListParams::default()).await {
            Ok(list) => list
                .items
                .into_iter()
                .filter_map(|pvc| {
                    let name = pvc.metadata.name?;
                    let raw = pvc
                        .status
                        .as_ref()?
                        .capacity
                        .as_ref()?
                        .get("storage")?
                        .0
                        .clone();
                    let bytes = shared_types::parse_memory_quantity(&raw)?;
                    Some((name, (bytes / 1_048_576) as i64))
                })
                .collect(),
            Err(e) => {
                warn!("[POD_OVERVIEW] pvc list degraded: {e}");
                degraded.push(PodOverviewDegraded {
                    section: "pvc".to_string(),
                    reason: "pvc_list_failed".to_string(),
                    detail: Some(e.to_string()),
                });
                HashMap::new()
            }
        };

        // 3) Events（服务端收窄 + 分区降级）
        let event_api: Api<Event> = Api::namespaced(self.client.clone(), &self.namespace);
        let events_by_pod: HashMap<String, Vec<AppEventInfo>> = match event_api
            .list(&ListParams {
                field_selector: Some("involvedObject.kind=Pod".to_string()),
                ..Default::default()
            })
            .await
        {
            Ok(list) => {
                let mut infos: Vec<AppEventInfo> = list
                    .items
                    .into_iter()
                    .filter_map(|event| {
                        let object = event.involved_object.name.clone()?;
                        // 只保留 Warning/异常事件会丢上下文；全量返回，
                        // 让前端决定展示密度（Normal 事件多为 Pulled/Started 噪音，
                        // 按 reason 排序时 Warning 优先）。
                        Some(AppEventInfo {
                            event_type: event.type_.clone().unwrap_or_else(|| "Normal".into()),
                            reason: event.reason.clone().unwrap_or_default(),
                            message: event.message.clone().unwrap_or_default(),
                            timestamp: event
                                .last_timestamp
                                .as_ref()
                                .map(|t| t.0.to_string())
                                .or_else(|| event.event_time.as_ref().map(|t| t.0.to_string()))
                                .unwrap_or_default(),
                            object,
                            count: event.count.unwrap_or(1),
                        })
                    })
                    .collect();
                // group_events_by_pod 的契约：输入倒序（截断到 limit 时保留的
                // 才是"最近"的）。k8s list 返回任意顺序，必须显式排。
                // **按解析后的时间值排，不能按字符串**：lastTimestamp 的
                // ":00Z" 与 eventTime 的 ":00.1Z" 同秒时后者更新，但 'Z' 的
                // 字典序会让前者靠前（留旧弃新）。解析失败的无时间事件排最后。
                infos.sort_by_key(|e| std::cmp::Reverse(event_sort_key(e)));
                group_events_by_pod(&infos, events_limit)
            }
            Err(e) => {
                warn!("[POD_OVERVIEW] events list degraded: {e}");
                degraded.push(PodOverviewDegraded {
                    section: "events".to_string(),
                    reason: "events_list_failed".to_string(),
                    detail: Some(e.to_string()),
                });
                HashMap::new()
            }
        };

        // 4) 组装
        let mut items = Vec::with_capacity(pod_list.items.len());
        for pod in pod_list.items {
            let Some(name) = pod.metadata.name.clone() else {
                continue;
            };
            let identity = pod_identity_from_labels(&pod);
            let phase = pod
                .status
                .as_ref()
                .and_then(|s| s.phase.clone())
                .unwrap_or_else(|| "Unknown".to_string());
            let (ready, health) = pod_health_from_status(&pod);
            items.push(PodOverviewItem {
                name: name.clone(),
                workload: workload_from_owner(&pod),
                identity,
                phase,
                ready,
                node: pod.spec.as_ref().and_then(|s| s.node_name.clone()),
                ip: pod.status.as_ref().and_then(|s| s.pod_ip.clone()),
                created_at: pod.metadata.creation_timestamp.as_ref().and_then(|ts| {
                    chrono::DateTime::from_timestamp(
                        ts.0.as_second(),
                        ts.0.subsec_nanosecond() as u32,
                    )
                }),
                spec: pod_spec(&pod, &pvc_capacity),
                health,
                events: events_by_pod.get(&name).cloned().unwrap_or_default(),
            });
        }

        Ok(PodOverviewSnapshot {
            pods: items,
            degraded,
            source: "kubernetes".to_string(),
            namespace: Some(self.namespace.clone()),
        })
    }
}

/// 从 Pod labels 还原业务身份（复用既有的标签直读，不从名字反解）。
fn pod_identity_from_labels(pod: &Pod) -> Option<PodIdentity> {
    use crate::runtime::k8s_service::container_identity_from_labels;
    let labels = pod.metadata.labels.as_ref()?;
    let (service_type, slots) = container_identity_from_labels(labels);
    Some(PodIdentity {
        service_type,
        user_id: slots.user_id,
        project_id: slots.project_id,
        app_id: slots.app_id,
        pod_id: slots.pod_id,
    })
}

/// ownerReference 一跳 → workload 归属。
fn workload_from_owner(pod: &Pod) -> Option<PodWorkload> {
    let owner = pod.metadata.owner_references.as_ref()?.first()?;
    Some(PodWorkload {
        kind: owner.kind.clone(),
        name: owner.name.clone(),
    })
}

/// ready + 健康（复用抽取后的纯函数 `pod_container_diagnostic`）。
///
/// 选取规则（复审 R8 修正）：
/// 1. **主容器名先从 spec.containers 选**（`agent` > `app` > `rcoder` > 第一个），
///    再按名字到 containerStatuses 里查——两个列表顺序可能不同，直接
///    `statuses.first()` 会把健康 sidecar 当主容器；
/// 2. **阻塞中的 init 容器优先**：init 还在重启/CrashLoop 时，Pod 的真实
///    状态就是 init 的状态（这正是 Init:CrashLoopBackOff 要排查的根因）；
///    正常完成的 init 不覆盖主容器健康。
fn pod_health_from_status(pod: &Pod) -> (bool, PodHealth) {
    let status = pod.status.as_ref();
    let main_name = main_container_name_from_spec(pod);
    let statuses = status.and_then(|s| s.container_statuses.as_ref());
    let init_statuses = status.and_then(|s| s.init_container_statuses.as_ref());

    // ① 阻塞 init 的三种形态都要抓到：
    //    a) waiting 有原因（CrashLoopBackOff / PodInitializing 后的异常重启中）；
    //    b) last_state.terminated（重启过——上次终止无论成败都说明 init 没一次过）；
    //    c) state.terminated 且 reason != "Completed"（restartPolicy=Never 下
    //       init 失败直接 Failed：无 waiting、无 last_state，不看这条会漏诊）。
    //    正常完成的 init（state.terminated.reason == "Completed"）不算阻塞。
    let blocking_init = init_statuses.and_then(|inits| {
        inits.iter().find(|cs| {
            let waiting = cs
                .state
                .as_ref()
                .and_then(|st| st.waiting.as_ref())
                .and_then(|w| w.reason.as_deref())
                .is_some_and(|r| !r.is_empty());
            let restarted = cs
                .last_state
                .as_ref()
                .and_then(|st| st.terminated.as_ref())
                .is_some();
            let failed_terminated = cs
                .state
                .as_ref()
                .and_then(|st| st.terminated.as_ref())
                .is_some_and(|t| t.reason.as_deref() != Some("Completed"));
            waiting || restarted || failed_terminated
        })
    });
    if let Some(cs) = blocking_init {
        let diagnostic = pod_container_diagnostic(cs);
        let oom_killed_last = diagnostic.is_oom();
        let ready = false; // init 未完成 → pod 必然未就绪
        return (
            ready,
            PodHealth {
                restart_count: diagnostic.restart_count,
                restart_source: "container_status".to_string(),
                last_terminate_reason: diagnostic.last_terminate_reason,
                last_exit_code: diagnostic.last_exit_code,
                oom_killed_last,
                waiting_reason: diagnostic.waiting_reason,
                detail: Some(format!(
                    "init container {}: {}",
                    cs.name,
                    diagnostic.detail.unwrap_or_default()
                )),
            },
        );
    }

    // ② 主容器：spec 选名 → 按名查状态
    let Some(statuses) = statuses else {
        return (
            false,
            PodHealth {
                restart_source: "container_status".to_string(),
                detail: Some("container statuses not available yet".to_string()),
                ..Default::default()
            },
        );
    };
    let main = main_name
        .as_deref()
        .and_then(|name| statuses.iter().find(|c| c.name == name))
        .or_else(|| statuses.first());
    let Some(cs) = main else {
        return (
            false,
            PodHealth {
                restart_source: "container_status".to_string(),
                detail: Some("no container statuses".to_string()),
                ..Default::default()
            },
        );
    };
    let diagnostic = pod_container_diagnostic(cs);
    let oom_killed_last = diagnostic.is_oom();
    let ready = cs.ready;
    (
        ready,
        PodHealth {
            restart_count: diagnostic.restart_count,
            restart_source: "container_status".to_string(),
            last_terminate_reason: diagnostic.last_terminate_reason,
            last_exit_code: diagnostic.last_exit_code,
            oom_killed_last,
            waiting_reason: diagnostic.waiting_reason,
            detail: diagnostic.detail,
        },
    )
}

/// 主容器名（从 **spec** 选，与状态列表解耦）：`agent` > `app` > `rcoder` > 第一个。
fn main_container_name_from_spec(pod: &Pod) -> Option<String> {
    let spec = pod.spec.as_ref()?;
    for name in ["agent", "app", "rcoder"] {
        if let Some(c) = spec.containers.iter().find(|c| c.name == name) {
            return Some(c.name.clone());
        }
    }
    spec.containers.first().map(|c| c.name.clone())
}

/// 主容器规格 + 逐容器规格 + 卷。
fn pod_spec(pod: &Pod, pvc_capacity: &HashMap<String, i64>) -> PodResourcesSpec {
    use shared_types::{parse_cpu_quantity, parse_memory_quantity};
    let Some(spec) = pod.spec.as_ref() else {
        return PodResourcesSpec::default();
    };

    let containers: Vec<ContainerResourcesSpecLocal> = spec
        .containers
        .iter()
        .map(|c| {
            let (requests, limits) = c
                .resources
                .as_ref()
                .map(|r| (r.requests.clone(), r.limits.clone()))
                .unwrap_or((None, None));
            ContainerResourcesSpecLocal {
                name: c.name.clone(),
                cpu_request: requests
                    .as_ref()
                    .and_then(|m| m.get("cpu"))
                    .and_then(|q| parse_cpu_quantity(&q.0)),
                cpu_limit: limits
                    .as_ref()
                    .and_then(|m| m.get("cpu"))
                    .and_then(|q| parse_cpu_quantity(&q.0)),
                mem_request: requests
                    .as_ref()
                    .and_then(|m| m.get("memory"))
                    .and_then(|q| parse_memory_quantity(&q.0))
                    .map(|b| (b / 1_048_576) as i64),
                mem_limit: limits
                    .as_ref()
                    .and_then(|m| m.get("memory"))
                    .and_then(|q| parse_memory_quantity(&q.0))
                    .map(|b| (b / 1_048_576) as i64),
                ephemeral_storage_limit: limits
                    .as_ref()
                    .and_then(|m| m.get("ephemeral-storage"))
                    .and_then(|q| parse_memory_quantity(&q.0))
                    .map(|b| (b / 1_048_576) as i64),
            }
        })
        .collect();

    let main = main_container_resources(&containers, spec);

    // 卷：PVC 配额（容量来自索引）；emptyDir.sizeLimit
    let mut volumes = Vec::new();
    if let Some(pod_volumes) = &spec.volumes {
        for volume in pod_volumes {
            if let Some(pvc) = &volume.persistent_volume_claim {
                volumes.push(PodVolumeSpec {
                    volume_type: "pvc".to_string(),
                    name: volume.name.clone(),
                    mount_path: None, // mountPath 在 container.volumeMounts，主容器有值时填
                    capacity_mb: pvc_capacity.get(&pvc.claim_name).copied(),
                    host_path: None,
                });
            } else if let Some(empty_dir) = &volume.empty_dir {
                let capacity = empty_dir
                    .size_limit
                    .as_ref()
                    .and_then(|q| parse_memory_quantity(&q.0))
                    .map(|b| (b / 1_048_576) as i64);
                volumes.push(PodVolumeSpec {
                    volume_type: "empty_dir".to_string(),
                    name: volume.name.clone(),
                    mount_path: None,
                    capacity_mb: capacity,
                    host_path: None,
                });
            }
        }
    }
    // 主容器的 volumeMounts → mount_path
    if let Some(main_name) = main.map(|m| m.name.clone())
        && let Some(container) = spec.containers.iter().find(|c| c.name == main_name)
        && let Some(mounts) = &container.volume_mounts
    {
        for mount in mounts {
            if let Some(vol) = volumes.iter_mut().find(|v| v.name == mount.name) {
                vol.mount_path = Some(mount.mount_path.clone());
            }
        }
    }

    let (cpu_request, cpu_limit, mem_request, mem_limit) = main
        .map(|m| (m.cpu_request, m.cpu_limit, m.mem_request, m.mem_limit))
        .unwrap_or((None, None, None, None));

    // ephemeral-storage：主容器 limits（k8s 无 requests 语义的常用法）
    let ephemeral_storage_limit_mb = main.and_then(|m| m.ephemeral_storage_limit);

    PodResourcesSpec {
        cpu_request_cores: cpu_request,
        cpu_limit_cores: cpu_limit,
        mem_request_mb: mem_request,
        mem_limit_mb: mem_limit,
        ephemeral_storage_limit_mb,
        volumes,
        containers: containers
            .into_iter()
            .map(
                |c| container_runtime_api::pod_overview::ContainerResourcesSpec {
                    name: c.name,
                    cpu_request_cores: c.cpu_request,
                    cpu_limit_cores: c.cpu_limit,
                    mem_request_mb: c.mem_request,
                    mem_limit_mb: c.mem_limit,
                },
            )
            .collect(),
    }
}

/// 内部中间结构：带 ephemeral-storage（对外类型不含它——PVC/emptyDir 才是
/// 用户关心的磁盘语义，ephemeral 只在顶层聚合）。
struct ContainerResourcesSpecLocal {
    name: String,
    cpu_request: Option<f64>,
    cpu_limit: Option<f64>,
    mem_request: Option<i64>,
    mem_limit: Option<i64>,
    ephemeral_storage_limit: Option<i64>,
}

/// 主容器选取（与 main_container_status 同一顺序）。
fn main_container_resources<'a>(
    containers: &'a [ContainerResourcesSpecLocal],
    spec: &'a k8s_openapi::api::core::v1::PodSpec,
) -> Option<&'a ContainerResourcesSpecLocal> {
    let main_name = ["agent", "app", "rcoder"]
        .into_iter()
        .find(|name| spec.containers.iter().any(|c| c.name == *name))
        .or_else(|| spec.containers.first().map(|c| c.name.as_str()))?;
    containers.iter().find(|c| c.name == main_name)
}

/// 事件排序键：解析 RFC3339 为时间值；解析失败 → 最小值（排最后）。
fn event_sort_key(event: &AppEventInfo) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(&event.timestamp)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
}

#[cfg(test)]
mod overview_tests {
    use super::*;

    /// 构造最小 Pod（spec 容器列表 + 可选 init/主容器状态）。
    fn pod_with(
        spec_names: &[&str],
        statuses: &[(&str, bool, u32, &str)], // (name, ready, restarts, last_reason)
        init_statuses: &[(&str, &str)],       // (name, waiting_reason)
    ) -> Pod {
        let containers = spec_names
            .iter()
            .map(|name| k8s_openapi::api::core::v1::Container {
                name: name.to_string(),
                ..Default::default()
            })
            .collect();
        let mk_cs = |name: &str, ready: bool, restarts: u32, reason: &str| {
            let last_state =
                (!reason.is_empty()).then(|| k8s_openapi::api::core::v1::ContainerState {
                    terminated: Some(k8s_openapi::api::core::v1::ContainerStateTerminated {
                        reason: Some(reason.to_string()),
                        exit_code: 137,
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            k8s_openapi::api::core::v1::ContainerStatus {
                name: name.to_string(),
                ready,
                restart_count: restarts as i32,
                last_state,
                ..Default::default()
            }
        };
        let main = statuses
            .iter()
            .map(|(name, ready, restarts, reason)| mk_cs(name, *ready, *restarts, reason))
            .collect();
        let inits = init_statuses
            .iter()
            .map(|(name, waiting)| {
                let state =
                    (!waiting.is_empty()).then(|| k8s_openapi::api::core::v1::ContainerState {
                        waiting: Some(k8s_openapi::api::core::v1::ContainerStateWaiting {
                            reason: Some(waiting.to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                let mut cs = mk_cs(name, false, 6, "OOMKilled");
                cs.state = state;
                cs
            })
            .collect();
        Pod {
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                containers,
                ..Default::default()
            }),
            status: Some(k8s_openapi::api::core::v1::PodStatus {
                phase: Some("Pending".to_string()),
                container_statuses: Some(main),
                init_container_statuses: Some(inits),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// R8：阻塞 init 容器（重启 6 次 + OOMKilled + CrashLoopBackOff）必须
    /// 反映到健康——这正是 Init:CrashLoopBackOff 要排查的根因。
    #[test]
    fn blocking_init_container_surfaces_in_health() {
        let pod = pod_with(
            &["app"],
            &[("app", false, 0, "")],
            &[("init-db", "CrashLoopBackOff")],
        );
        let (ready, health) = pod_health_from_status(&pod);
        assert!(!ready);
        assert_eq!(health.restart_count, 6, "init 的重启数要浮出来");
        assert!(health.oom_killed_last, "init 的 OOMKilled 要浮出来");
        assert_eq!(health.waiting_reason.as_deref(), Some("CrashLoopBackOff"));
        assert!(
            health.detail.as_deref().is_some_and(|d| d.contains("init")),
            "detail 要注明是 init 容器: {:?}",
            health.detail
        );
    }

    /// init 在 restartPolicy=Never 下失败：state.terminated(reason=Error)、
    /// 无 waiting、无 last_state——第三种阻塞形态，不看 state.terminated
    /// 会漏诊（pod Failed 的根因正是这个 init）。
    #[test]
    fn failed_init_without_restart_still_surfaces() {
        let mut pod = pod_with(&["app"], &[("app", false, 0, "")], &[]);
        // 构造 init：state.terminated(reason=Error)，无 waiting / last_state
        let init_cs = k8s_openapi::api::core::v1::ContainerStatus {
            name: "init-never".to_string(),
            ready: false,
            restart_count: 0,
            state: Some(k8s_openapi::api::core::v1::ContainerState {
                terminated: Some(k8s_openapi::api::core::v1::ContainerStateTerminated {
                    reason: Some("Error".to_string()),
                    exit_code: 1,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        if let Some(status) = pod.status.as_mut() {
            status.init_container_statuses = Some(vec![init_cs]);
        }
        let (ready, health) = pod_health_from_status(&pod);
        assert!(!ready);
        assert!(
            health.detail.as_deref().is_some_and(|d| d.contains("init")),
            "根因要归到 init 容器: {:?}",
            health.detail
        );
    }

    /// R8：spec 与 containerStatuses 顺序不同时，主容器按 **spec** 选——
    /// 不能把 statuses.first()（健康 sidecar）当主容器。
    #[test]
    fn main_container_follows_spec_order_not_status_order() {
        // spec: [db, sidecar]；statuses: [sidecar(ready), db(OOM+重启6)]
        let pod = pod_with(
            &["db", "sidecar"],
            &[("sidecar", true, 0, ""), ("db", false, 6, "OOMKilled")],
            &[],
        );
        let (ready, health) = pod_health_from_status(&pod);
        assert!(!ready, "主容器 db 未就绪，不能报 ready");
        assert_eq!(health.restart_count, 6);
        assert!(health.oom_killed_last);
    }

    /// 正常完成的 init（无 waiting、无异常 lastState）不覆盖主容器健康。
    #[test]
    fn completed_init_does_not_override_main() {
        let mut pod = pod_with(&["app"], &[("app", true, 0, "")], &[("init-ok", "")]);
        // 把 init 的异常 lastState 清掉（模拟成功完成）
        if let Some(status) = pod.status.as_mut()
            && let Some(inits) = status.init_container_statuses.as_mut()
        {
            inits[0].last_state = None;
            inits[0].restart_count = 0;
        }
        let (ready, health) = pod_health_from_status(&pod);
        assert!(ready, "主容器 ready");
        assert_eq!(health.restart_count, 0);
        assert!(health.detail.is_none() || !health.detail.unwrap().contains("init"));
    }

    /// R9：":00Z" 与 ":00.1Z" 同秒——字符串字典序会把 Z 排在数字前
    /// （留旧弃新），按解析后的时间值排才正确。
    #[test]
    fn subsecond_timestamp_sorts_by_parsed_value() {
        let mk = |ts: &str| AppEventInfo {
            event_type: "Normal".into(),
            reason: "Started".into(),
            message: String::new(),
            timestamp: ts.into(),
            object: "p".into(),
            count: 1,
        };
        let mut events = [mk("2026-11-13T10:00:00Z"), mk("2026-11-13T10:00:00.1Z")];
        events.sort_by_key(|e| std::cmp::Reverse(event_sort_key(e)));
        assert_eq!(
            events[0].timestamp, "2026-11-13T10:00:00.1Z",
            "同秒的 .1Z 更新，必须排前面"
        );
    }
}
