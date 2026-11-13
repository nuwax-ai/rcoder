//! `/computer/pod/overview` 的运行时观测类型（方案：docs/pod-overview.md）。
//!
//! 命名空间级 Pod 概览：规格（申请了多少）、健康（重启/OOM/等待原因）、
//! 最近事件。P1 不含 usage（实际用量）——那是 P2 的 metrics/gRPC 双源合并。
//!
//! 语义红线（与既有接口同一套原则）：
//! - **`None` ≠ `0`**：取不到就是 `None`，绝不填 0（0 会被读成"空载/没重启"）；
//! - `oom_killed_last` 只表示**最近一次**终止是否 OOM，不是历史次数；
//!   历史只能从近 1h 事件里数（k8s Events 默认保留 ~1h）；
//! - docker 的 `restart_count` 只统计重启策略触发的重启（实测：
//!   手工 `docker restart` 不计数），重启次数以事件里的 restart/die 计数为准，
//!   `restart_source` 必须如实标注。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::AppEventInfo;

/// 概览过滤参数。所有字段可选；`search` 是大小写不敏感的子串匹配，
/// 命中 `name` 或 `identity` 任一字段即保留。
///
/// 过滤永远发生在**缓存快照**上（服务端内存），不触发新的上游调用。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PodOverviewFilter {
    /// 关键字子串匹配（pod 名 / user_id / app_id / project_id / pod_id）
    pub search: Option<String>,
    /// 按 `RuntimeContainerInfo.service_type` 过滤
    pub service_type: Option<shared_types::ServiceType>,
    pub user_id: Option<String>,
    pub app_id: Option<String>,
    pub project_id: Option<String>,
    /// k8s：透传给 list 的 label selector；docker：映射为 label 过滤
    pub label_selector: Option<String>,
}

/// 单个 Pod 的观测结果。字段分四段：身份 / 规格 / 健康 / 事件，
/// 各段独立可缺省（采集失败降级，不失败整条记录）。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodOverviewItem {
    /// Pod/容器名（k8s: metadata.name；docker: 容器名）
    pub name: String,
    /// 所属 workload（k8s：ownerReference 一跳；docker：恒 None，容器名即 workload）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload: Option<PodWorkload>,
    /// 业务身份（结构化，不从名字反解；非 rcoder 管理的 Pod 为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<PodIdentity>,
    /// k8s Pod phase（Running/Pending/…）；docker 用 State.Status（running/exited/…）
    pub phase: String,
    /// 主容器是否 ready（docker：running 即 ready）
    pub ready: bool,
    /// 调度节点（k8s）；docker 为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Pod IP
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    /// 规格段：主容器（agent/app/rcoder 顺序匹配，缺省第一个）的申请值
    pub spec: PodResourcesSpec,
    /// 健康段
    pub health: PodHealth,
    /// 最近事件（倒序，最多 `events_limit` 条；采集失败为空数组——
    /// 与"没有事件"的区分靠 `PodOverviewSnapshot::degraded`）
    #[serde(default)]
    pub events: Vec<AppEventInfo>,
}

/// workload 归属（k8s ownerReference 一跳：StatefulSet/Deployment/…）。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodWorkload {
    pub kind: String,
    pub name: String,
}

/// 结构化业务身份（与 `RuntimeContainerInfo` 的槽位语义一致）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_type: Option<shared_types::ServiceType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
}

/// 资源规格（申请值）。全部可缺省：没设 request/limit 就是 None，
/// docker `HostConfig.Memory == 0`（不限制）也是 None——**0 不是 0MB**。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodResourcesSpec {
    /// CPU 申请（核）。k8s: requests["cpu"]；docker: NanoCpus/1e9
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_request_cores: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit_cores: Option<f64>,
    /// 内存申请（MB，1MB=1048576B）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_request_mb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_limit_mb: Option<i64>,
    /// 临时盘配额（MB）：k8s ephemeral-storage limit / emptyDir.sizeLimit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_storage_limit_mb: Option<i64>,
    /// 卷（PVC 配额；docker 只有宿主目录映射，无配额概念）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<PodVolumeSpec>,
    /// 多容器：逐容器的 request/limit（sidecar 隐藏逻辑在上层做）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<ContainerResourcesSpec>,
}

/// 单个容器的规格（多容器 Pod 拆分行用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ContainerResourcesSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_request_cores: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit_cores: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_request_mb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_limit_mb: Option<i64>,
}

/// 卷信息。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodVolumeSpec {
    /// pvc / host_path / empty_dir / docker_mount
    #[serde(rename = "type")]
    pub volume_type: String,
    pub name: String,
    /// 挂载路径（docker 的 Destination；k8s 主容器的 mountPath）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount_path: Option<String>,
    /// 容量（MB）。k8s PVC 的 status.capacity；docker 恒 None（无配额）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_mb: Option<i64>,
    /// docker：宿主机源路径（Source）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_path: Option<String>,
}

/// 健康段。数值语义见模块头注释。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodHealth {
    /// 重启次数。k8s：containerStatuses.restartCount；
    /// docker：事件里 restart/die 计数（RestartCount 只统计策略重启，仅参考）
    pub restart_count: u32,
    /// 重启计数的来源："container_status"（k8s）| "docker_events"（docker）
    pub restart_source: String,
    /// 最近一次终止原因（"OOMKilled" / "Error" / …；无终止为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_terminate_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    /// 最近一次终止是否 OOM
    #[serde(default)]
    pub oom_killed_last: bool,
    /// k8s waiting 原因（CrashLoopBackOff / ContainerCreating / …）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<String>,
    /// 可读详情
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 一次概览采集的结果：Pod 列表 + 分区降级说明。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodOverviewSnapshot {
    pub pods: Vec<PodOverviewItem>,
    /// 分区级降级（如 events 列取失败）；空 = 全部成功。
    /// **"空数组"与"取不到"必须可区分**——靠这里，不靠猜。
    pub degraded: Vec<PodOverviewDegraded>,
}

/// 单个分区降级说明。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodOverviewDegraded {
    /// 失败的分区：events / pvc / …
    pub section: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ── 纯函数：事件按 Pod 归组（k8s/docker 共用语义）────────────────────────

/// 把事件按 `involved_object`（k8s）/ `Actor`/容器名（docker）归到 Pod 名下。
/// 输入已按时间倒序；输出每组保持倒序并截断到 `limit`。
pub fn group_events_by_pod(
    events: &[AppEventInfo],
    limit: usize,
) -> std::collections::HashMap<String, Vec<AppEventInfo>> {
    let mut grouped: std::collections::HashMap<String, Vec<AppEventInfo>> =
        std::collections::HashMap::new();
    for event in events {
        let bucket = grouped.entry(event.object.clone()).or_default();
        if bucket.len() < limit {
            bucket.push(event.clone());
        }
    }
    grouped
}

/// `search` 关键字匹配：大小写不敏感子串，命中 name 或 identity 任一槽位。
pub fn matches_search(observation: &PodOverviewItem, search: &str) -> bool {
    let needle = search.to_lowercase();
    if observation.name.to_lowercase().contains(&needle) {
        return true;
    }
    let Some(identity) = observation.identity.as_ref() else {
        return false;
    };
    [
        identity.user_id.as_deref(),
        identity.app_id.as_deref(),
        identity.project_id.as_deref(),
        identity.pod_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_lowercase().contains(&needle))
}

/// 在快照上应用过滤（内存操作，不触发上游调用）。
pub fn apply_filter<'a>(
    snapshot: &'a PodOverviewSnapshot,
    filter: &PodOverviewFilter,
) -> Vec<&'a PodOverviewItem> {
    snapshot
        .pods
        .iter()
        .filter(|pod| {
            if let Some(want) = filter.service_type.as_ref() {
                let matches = pod
                    .identity
                    .as_ref()
                    .and_then(|identity| identity.service_type.as_ref())
                    .is_some_and(|have| have == want);
                if !matches {
                    return false;
                }
            }
            if let Some(user) = filter.user_id.as_deref() {
                let matches = pod
                    .identity
                    .as_ref()
                    .and_then(|identity| identity.user_id.as_deref())
                    .is_some_and(|have| have == user);
                if !matches {
                    return false;
                }
            }
            if let Some(app) = filter.app_id.as_deref() {
                let matches = pod
                    .identity
                    .as_ref()
                    .and_then(|identity| identity.app_id.as_deref())
                    .is_some_and(|have| have == app);
                if !matches {
                    return false;
                }
            }
            if let Some(project) = filter.project_id.as_deref() {
                let matches = pod
                    .identity
                    .as_ref()
                    .and_then(|identity| identity.project_id.as_deref())
                    .is_some_and(|have| have == project);
                if !matches {
                    return false;
                }
            }
            if let Some(search) = filter.search.as_deref().filter(|search| !search.is_empty())
                && !matches_search(pod, search)
            {
                return false;
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(name: &str, user: Option<&str>, app: Option<&str>) -> PodOverviewItem {
        PodOverviewItem {
            name: name.to_string(),
            workload: None,
            identity: Some(PodIdentity {
                user_id: user.map(str::to_string),
                app_id: app.map(str::to_string),
                ..Default::default()
            }),
            phase: "Running".to_string(),
            ready: true,
            node: None,
            ip: None,
            created_at: None,
            spec: Default::default(),
            health: Default::default(),
            events: Vec::new(),
        }
    }

    #[test]
    fn search_matches_name_and_identity_case_insensitively() {
        let pod = obs("rcoder-computer-agent-runner-6-0", Some("6"), None);
        assert!(matches_search(&pod, "RUNNER-6"));
        assert!(matches_search(&pod, "computer"));
        // identity 槽位也要命中
        assert!(matches_search(&pod, "6"));
        assert!(!matches_search(&pod, "999"));
        let app_pod = obs("rcoder-app-builder-132-0", None, Some("132"));
        assert!(matches_search(&app_pod, "132"));
    }

    #[test]
    fn filter_combines_service_type_and_search() {
        let snapshot = PodOverviewSnapshot {
            pods: vec![
                obs("a-computer-1", Some("6"), None),
                obs("b-builder-1", None, Some("132")),
            ],
            degraded: Vec::new(),
        };
        let filter = PodOverviewFilter {
            search: Some("builder".to_string()),
            ..Default::default()
        };
        let result = apply_filter(&snapshot, &filter);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "b-builder-1");
    }

    #[test]
    fn group_events_truncates_per_pod() {
        let mk = |object: &str, seq: usize| AppEventInfo {
            event_type: "Warning".to_string(),
            reason: "BackOff".to_string(),
            message: format!("e{seq}"),
            timestamp: format!("2026-11-13T10:00:{seq:02}Z"),
            object: object.to_string(),
            count: 1,
        };
        let events = vec![
            mk("pod-a", 1),
            mk("pod-a", 2),
            mk("pod-a", 3),
            mk("pod-b", 4),
        ];
        let grouped = group_events_by_pod(&events, 2);
        assert_eq!(grouped["pod-a"].len(), 2, "截断到 limit");
        assert_eq!(grouped["pod-b"].len(), 1);
        assert_eq!(grouped["pod-a"][0].message, "e1", "保持输入顺序（倒序）");
    }
}
