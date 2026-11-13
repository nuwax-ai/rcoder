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
                let infos: Vec<AppEventInfo> = list
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

/// ready + 健康（复用抽取后的纯函数 `pod_container_diagnostic`，主容器名自适应）。
fn pod_health_from_status(pod: &Pod) -> (bool, PodHealth) {
    let statuses = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref());
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
    let main = main_container_status(statuses).cloned();
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
    let diagnostic = pod_container_diagnostic(&cs);
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

/// 主容器：`agent` > `app` > `rcoder` > 第一个（与方案 §6.2 一致）。
fn main_container_status<'a>(
    statuses: &'a [k8s_openapi::api::core::v1::ContainerStatus],
) -> Option<&'a k8s_openapi::api::core::v1::ContainerStatus> {
    for name in ["agent", "app", "rcoder"] {
        if let Some(cs) = statuses.iter().find(|c| c.name == name) {
            return Some(cs);
        }
    }
    statuses.first()
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
    if let Some(main_name) = main.map(|m| m.name.clone()) {
        if let Some(container) = spec.containers.iter().find(|c| c.name == main_name) {
            if let Some(mounts) = &container.volume_mounts {
                for mount in mounts {
                    if let Some(vol) = volumes.iter_mut().find(|v| v.name == mount.name) {
                        vol.mount_path = Some(mount.mount_path.clone());
                    }
                }
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
