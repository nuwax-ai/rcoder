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
    PodResourcesSpec, PodUsage, PodVolumeSpec, PodWorkload, group_events_by_pod, sampled_now,
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

        // 3.5) 实际用量：metrics.k8s.io（一次 list 覆盖全部 Pod）。
        //      失败只降级（usage=None + degraded），不失败整体。
        let usage_by_pod = match self.pod_metrics_usage().await {
            Ok(map) => map,
            Err(e) => {
                warn!("[POD_OVERVIEW] metrics degraded: {e}");
                degraded.push(PodOverviewDegraded {
                    section: "usage".to_string(),
                    reason: "metrics_unavailable".to_string(),
                    detail: Some(e),
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
                pod_uid: pod.metadata.uid.clone(),
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
                usage: usage_by_pod.get(&name).cloned(),
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

    // ① 阻塞 init：**当前执行状态优先**（F1）——先看 init 现在是否已经
    //    完成/就绪，历史 lastState 单独不能判阻塞：
    //    - state.terminated.reason == "Completed" → 已完成，不阻塞；
    //    - cs.ready == true → 运行中的 native sidecar 已就绪，不阻塞；
    //    - 否则（waiting 有原因 / 上次终止后未恢复 / 一次性失败）→ 阻塞，
    //      其诊断（重启数/OOM/waiting）浮出到 pod 健康。
    //    反例教训：完成过的 init 保留旧 lastState（此前 OOM）、ready 的
    //    sidecar 保留历史——只看历史会把健康 pod 判成未就绪。
    let blocking_init = init_statuses.and_then(|inits| {
        inits.iter().find(|cs| {
            let current_completed = cs
                .state
                .as_ref()
                .and_then(|st| st.terminated.as_ref())
                .is_some_and(|t| t.reason.as_deref() == Some("Completed"));
            if current_completed || cs.ready {
                return false;
            }
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
        let ready = false; // init 未完成 → pod 必然未就绪
        return (
            ready,
            PodHealth {
                restart_count: diagnostic.restart_count,
                restart_source: "container_status".to_string(),
                last_termination: diagnostic.last_terminate_reason.map(|reason| {
                    let is_oom = reason == "OOMKilled";
                    container_runtime_api::pod_overview::LastTermination {
                        reason: Some(reason),
                        exit_code: diagnostic.last_exit_code,
                        finished_at: terminated_finished_at(cs),
                        is_oom,
                    }
                }),
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
    // 主容器名已知但状态缺席：保持未就绪并说明——**不能借用别的容器**
    // （F2：明知主容器是 db，回退到健康 sidecar 会报 ready=true）。
    // 只在完全无法确定主名时才退到第一个状态。
    let main = match main_name.as_deref() {
        Some(name) => match statuses.iter().find(|c| c.name == name) {
            Some(cs) => Some(cs),
            None => {
                return (
                    false,
                    PodHealth {
                        restart_source: "container_status".to_string(),
                        detail: Some(format!("main container {name} status not available yet")),
                        ..Default::default()
                    },
                );
            }
        },
        None => statuses.first(),
    };
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
    let ready = cs.ready;
    (
        ready,
        PodHealth {
            restart_count: diagnostic.restart_count,
            restart_source: "container_status".to_string(),
            last_termination: diagnostic.last_terminate_reason.map(|reason| {
                let is_oom = reason == "OOMKilled";
                container_runtime_api::pod_overview::LastTermination {
                    reason: Some(reason),
                    exit_code: diagnostic.last_exit_code,
                    finished_at: terminated_finished_at(cs),
                    is_oom,
                }
            }),
            waiting_reason: diagnostic.waiting_reason,
            detail: diagnostic.detail,
        },
    )
}

/// 主容器名（从 **spec** 选，与状态列表解耦）：`agent` > `app` > `rcoder` > 第一个。
///
/// 状态侧（`pod_health_from_status`）与资源侧（`main_container_resources`）都
/// 走这一个函数：优先级各写一遍就会漂移，届时面板上的"主容器"在两处对不上。
fn main_container_name_from_spec(pod: &Pod) -> Option<String> {
    main_container_name(pod.spec.as_ref()?).map(str::to_string)
}

/// 主容器选取优先级（spec 侧唯一实现；按序第一个命中的即主容器）。
const MAIN_CONTAINER_PREFERENCE: [&str; 3] = ["agent", "app", "rcoder"];

/// 主容器名：按 [`MAIN_CONTAINER_PREFERENCE`] 取第一个存在的，都没有则取第一个容器。
fn main_container_name(spec: &k8s_openapi::api::core::v1::PodSpec) -> Option<&str> {
    MAIN_CONTAINER_PREFERENCE
        .into_iter()
        .find(|name| spec.containers.iter().any(|c| c.name == *name))
        .or_else(|| spec.containers.first().map(|c| c.name.as_str()))
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

/// 主容器选取（与状态侧同一函数，不再各写一遍优先级）。
fn main_container_resources<'a>(
    containers: &'a [ContainerResourcesSpecLocal],
    spec: &'a k8s_openapi::api::core::v1::PodSpec,
) -> Option<&'a ContainerResourcesSpecLocal> {
    let main_name = main_container_name(spec)?;
    containers.iter().find(|c| c.name == main_name)
}

/// 最近一次终止时间（当前 terminated 优先，否则 lastState）。
fn terminated_finished_at(
    cs: &k8s_openapi::api::core::v1::ContainerStatus,
) -> Option<chrono::DateTime<chrono::Utc>> {
    cs.state
        .as_ref()
        .and_then(|s| s.terminated.as_ref())
        .or_else(|| cs.last_state.as_ref().and_then(|s| s.terminated.as_ref()))
        .and_then(|t| t.finished_at.as_ref())
        .and_then(|t| chrono::DateTime::from_timestamp_millis(t.0.as_millisecond()))
        // k8s 未设置时可能给零值（0001-01-01）；只接受真实时间
        .filter(|dt| dt.timestamp() > 0)
}

/// 事件排序键：解析 RFC3339 为时间值；解析失败 → 最小值（排最后）。
fn event_sort_key(event: &AppEventInfo) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(&event.timestamp)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
}

impl super::KubernetesRuntime {
    /// 单容器健康（`/computer/pod/health`）。
    ///
    /// **只 GET 一个 Pod**：容器被 OOM 杀掉后这个接口照样能查——数据源是
    /// k8s API，不在容器内（这正是它和 `/resources/stream` 的本质区别）。
    ///
    /// `identifier` 是 Pod 名（k8s 下 RuntimeContainerInfo.container_name
    /// 即 Pod 名）。
    pub(crate) async fn pod_health_inner(
        &self,
        identifier: &str,
    ) -> container_runtime_api::ContainerRuntimeResult<Option<container_runtime_api::ContainerHealth>>
    {
        use container_runtime_api::{ContainerHealth, LastTermination};

        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        // 名字解析：`container_name` 在 k8s 下是 **workload 名**
        // （`rcoder-computer-agent-runner-6`），Pod 名带 StatefulSet 序号
        // （`...-6-0`）。三级解析，成本从低到高：
        //   ① 直接 GET（identifier 本身即 Pod 名时命中）
        //   ② GET `<identifier>-0`（sts 单副本；rcoder 全部 workload 的常态）
        //   ③ list + ownerReference 匹配（多副本/异常序号兜底）
        let pod = match Self::get_pod_or_404(&api, identifier).await {
            Ok(Some(pod)) => pod,
            Ok(None) => {
                let ordinal = format!("{identifier}-0");
                match Self::get_pod_or_404(&api, &ordinal).await {
                    Ok(Some(pod)) => pod,
                    Ok(None) => match Self::find_pod_by_owner(&api, identifier).await {
                        Ok(Some(pod)) => pod,
                        Ok(None) => return Ok(None),
                        Err(e) => return Err(e),
                    },
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };

        let status = pod.status.as_ref();
        let phase = status
            .and_then(|s| s.phase.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        // 主容器名从 spec 选（与 overview 同一规则：agent>app>rcoder>first）
        let main_name = main_container_name_from_spec(&pod).filter(|n| !n.is_empty());
        let cs = main_name
            .as_deref()
            .and_then(|name| {
                status
                    .and_then(|s| s.container_statuses.as_ref())
                    .and_then(|list| list.iter().find(|c| c.name == name))
            })
            .or_else(|| {
                status
                    .and_then(|s| s.container_statuses.as_ref())
                    .and_then(|list| list.first())
            });

        let Some(cs) = cs else {
            // Pod 存在但容器状态还没出来（刚调度）：返回"已知存在但无状态"，
            // 不伪装成 not found。
            return Ok(Some(ContainerHealth {
                container_name: identifier.to_string(),
                pod_name: pod.metadata.name.clone(),
                pod_uid: pod.metadata.uid.clone(),
                alive: false,
                phase,
                ready: false,
                restart_count: 0,
                last_termination: None,
                started_at: None,
                waiting_reason: Some("ContainerStatusPending".to_string()),
                detail: Some("container statuses not available yet".to_string()),
                sampled_at: sampled_now(),
                source: "kubernetes".to_string(),
            }));
        };

        let diagnostic = pod_container_diagnostic(cs);
        // 最近一次终止：当前 terminated 优先，否则 lastState（与本仓
        // pod_diagnostic 同一口径）。finishedAt 是"最近一次重启时刻"的
        // 权威来源。
        let terminated = cs
            .state
            .as_ref()
            .and_then(|s| s.terminated.as_ref())
            .or_else(|| cs.last_state.as_ref().and_then(|s| s.terminated.as_ref()));
        let finished_at = terminated
            .and_then(|t| t.finished_at.as_ref())
            .and_then(|t| chrono::DateTime::from_timestamp_millis(t.0.as_millisecond()));
        let is_oom = terminated
            .and_then(|t| t.reason.as_deref())
            .is_some_and(|r| r == "OOMKilled")
            || diagnostic.is_oom();
        let last = terminated.map(|t| LastTermination {
            reason: t.reason.clone(),
            exit_code: Some(t.exit_code),
            finished_at,
            is_oom,
        });
        // `cs` 本身就是该容器状态（上面已定位），直接用——再按名字回查是
        // 冗余的二次查找。
        let alive = cs
            .state
            .as_ref()
            .and_then(|st| st.running.as_ref())
            .is_some();

        // 本次启动时间：容器 running 时才有（与 last_termination.finished_at
        // 相邻比较可算出这次重启的耗时）
        let started_at = cs
            .state
            .as_ref()
            .and_then(|st| st.running.as_ref())
            .and_then(|r| r.started_at.as_ref())
            .and_then(|t| chrono::DateTime::from_timestamp_millis(t.0.as_millisecond()))
            .filter(|dt| dt.timestamp() > 0);

        Ok(Some(ContainerHealth {
            container_name: identifier.to_string(),
            pod_name: pod.metadata.name.clone(),
            pod_uid: pod.metadata.uid.clone(),
            alive,
            phase,
            ready: cs.ready,
            restart_count: diagnostic.restart_count,
            last_termination: last,
            started_at,
            waiting_reason: diagnostic.waiting_reason.clone(),
            detail: diagnostic.detail.clone(),
            sampled_at: sampled_now(),
            source: "kubernetes".to_string(),
        }))
    }

    /// 采集命名空间内全部 Pod 的实际用量（metrics.k8s.io）。
    ///
    /// 生态里**没有**维护良好的 `PodMetrics` typed crate（k8s-openapi 不含
    /// metrics 组），官方支持的路径是 `Api<DynamicObject>` + `ApiResource`
    /// ——仍走 kube 的完整客户端栈（TLS/鉴权/超时/错误类型），只把返回值
    /// 当动态对象解析要的两个字段，而不是手写 HTTP。
    ///
    /// 失败（未装 metrics-server / RBAC 不足）返回 Err，由调用方降级为
    /// `usage=None + degraded(usage)`，不影响其他分区。
    pub(crate) async fn pod_metrics_usage(&self) -> Result<HashMap<String, PodUsage>, String> {
        use kube::api::{ApiResource, DynamicObject, GroupVersionKind};
        use shared_types::{parse_cpu_quantity, parse_memory_quantity};

        let gvk = GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "PodMetrics");
        let resource = ApiResource::from_gvk(&gvk);
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.namespace, &resource);
        let list = api
            .list(&ListParams::default())
            .await
            .map_err(|e| format!("list pod metrics: {e}"))?;

        let mut out = HashMap::new();
        for item in list.items {
            let Some(name) = item.metadata.name.clone() else {
                continue;
            };
            // window 是"采样值而非瞬时值"的证据，必须回显
            let window = item
                .data
                .get("window")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let mut cpu_cores = 0.0f64;
            let mut mem_bytes = 0u64;
            let mut seen = false;
            if let Some(containers) = item.data.get("containers").and_then(|v| v.as_array()) {
                for container in containers {
                    let Some(usage) = container.get("usage") else {
                        continue;
                    };
                    if let Some(cpu) = usage.get("cpu").and_then(|v| v.as_str())
                        && let Some(cores) = parse_cpu_quantity(cpu)
                    {
                        cpu_cores += cores;
                        seen = true;
                    }
                    if let Some(mem) = usage.get("memory").and_then(|v| v.as_str())
                        && let Some(bytes) = parse_memory_quantity(mem)
                    {
                        mem_bytes = mem_bytes.saturating_add(bytes);
                        seen = true;
                    }
                }
            }
            if !seen {
                continue;
            }
            out.insert(
                name,
                PodUsage {
                    source: "metrics_server".to_string(),
                    window,
                    cpu_cores: Some(cpu_cores),
                    cpu_percent: None, // 由 handler 结合 limit/request 计算
                    cpu_percent_basis: None,
                    mem_used_mb: Some((mem_bytes / 1_048_576) as i64),
                    mem_limit_mb: None,  // 由 handler 从 spec 填
                    process_count: None, // metrics API 无 pids
                },
            );
        }
        Ok(out)
    }
}

impl super::KubernetesRuntime {
    /// GET 单个 Pod；404 返回 `Ok(None)`（不是错误）。
    async fn get_pod_or_404(
        api: &Api<Pod>,
        name: &str,
    ) -> container_runtime_api::ContainerRuntimeResult<Option<Pod>> {
        match api.get(name).await {
            Ok(pod) => Ok(Some(pod)),
            Err(kube::Error::Api(resp)) if resp.code == 404 => Ok(None),
            Err(e) => Err(container_runtime_api::ContainerRuntimeError::K8sError(
                format!("get pod {name}: {e}"),
            )),
        }
    }

    /// 兜底：按 ownerReference 名匹配（workload 名 → Pod）。
    /// 只在①②都 404 时走（异常路径），一次 list 可接受。
    async fn find_pod_by_owner(
        api: &Api<Pod>,
        workload: &str,
    ) -> container_runtime_api::ContainerRuntimeResult<Option<Pod>> {
        let list = api.list(&ListParams::default()).await.map_err(|e| {
            container_runtime_api::ContainerRuntimeError::K8sError(format!(
                "list pods for owner {workload}: {e}"
            ))
        })?;
        Ok(list.items.into_iter().find(|pod| {
            pod.metadata
                .owner_references
                .as_ref()
                .is_some_and(|refs| refs.iter().any(|r| r.name == workload))
        }))
    }
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
        assert_eq!(
            health
                .last_termination
                .as_ref()
                .and_then(|t| t.reason.as_deref()),
            Some("OOMKilled"),
            "init 的 OOMKilled 要浮出来"
        );
        assert_eq!(health.waiting_reason.as_deref(), Some("CrashLoopBackOff"));
        assert!(
            health.detail.as_deref().is_some_and(|d| d.contains("init")),
            "detail 要注明是 init 容器: {:?}",
            health.detail
        );
    }

    /// F1：已完成的 init 保留旧 lastState（此前 OOM）不得判阻塞——
    /// 主容器 Running/ready 必须原样透出。
    #[test]
    fn completed_init_with_history_does_not_block() {
        let mut pod = pod_with(&["app"], &[("app", true, 0, "")], &[("init-x", "")]);
        // init：当前 state.terminated=Completed/exit=0/ready=true，
        // lastState 保留旧 OOM（pod_with 的 mk_cs 会设 last_state）
        if let Some(status) = pod.status.as_mut()
            && let Some(inits) = status.init_container_statuses.as_mut()
        {
            inits[0].state = Some(k8s_openapi::api::core::v1::ContainerState {
                terminated: Some(k8s_openapi::api::core::v1::ContainerStateTerminated {
                    reason: Some("Completed".to_string()),
                    exit_code: 0,
                    ..Default::default()
                }),
                ..Default::default()
            });
            inits[0].ready = true;
        }
        let (ready, health) = pod_health_from_status(&pod);
        assert!(ready, "主容器 ready，完成 init 的历史不覆盖");
        assert_ne!(
            health
                .last_termination
                .as_ref()
                .and_then(|t| t.reason.as_deref()),
            Some("OOMKilled")
        );
        assert_eq!(health.restart_count, 0);
    }

    /// F1：ready 的 native sidecar（restartPolicy=Always init）保留旧 OOM
    /// lastState——同样不得判阻塞。
    #[test]
    fn ready_native_sidecar_with_history_does_not_block() {
        let mut pod = pod_with(&["app"], &[("app", true, 0, "")], &[("sidecar-init", "")]);
        // sidecar：当前 Running/ready=true，lastState 留旧 OOM（mk_cs 默认）
        if let Some(status) = pod.status.as_mut()
            && let Some(inits) = status.init_container_statuses.as_mut()
        {
            inits[0].ready = true;
            // state 留空 = Running（无 terminated/waiting）
        }
        let (ready, health) = pod_health_from_status(&pod);
        assert!(ready, "主容器 ready，就绪 sidecar 的历史不覆盖");
        assert_ne!(
            health
                .last_termination
                .as_ref()
                .and_then(|t| t.reason.as_deref()),
            Some("OOMKilled")
        );
    }

    /// F2：主容器名已知（spec 里有 db）但状态缺席——保持未就绪并说明，
    /// 不得借用健康 sidecar 报 ready=true。
    #[test]
    fn missing_main_status_does_not_borrow_sidecar() {
        let pod = pod_with(&["db", "sidecar"], &[("sidecar", true, 0, "")], &[]);
        let (ready, health) = pod_health_from_status(&pod);
        assert!(!ready, "主容器 db 无状态 → 未就绪");
        assert!(
            health.detail.as_deref().is_some_and(|d| d.contains("db")),
            "detail 要点名缺席的主容器: {:?}",
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
        assert_eq!(
            health
                .last_termination
                .as_ref()
                .and_then(|t| t.reason.as_deref()),
            Some("OOMKilled")
        );
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
