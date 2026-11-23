//! `/computer/pod/overview` 的运行时观测类型（方案：docs/pod-overview.md）。
//!
//! 命名空间级 Pod 概览：规格（申请了多少）、健康（重启/终止/等待原因）、
//! 实际用量（usage，metrics-server / docker stats）、最近事件。
//!
//! 语义红线（与既有接口同一套原则）：
//! - **`None` ≠ `0`**：取不到就是 `None`，绝不填 0（0 会被读成"空载/没重启"）；
//! - 健康段**以「终止」为主语**，不以 OOM 为主语：`last_terminate_reason`
//!   对任何原因的终止都有值（`OOMKilled` / `Error` / `Completed`），
//!   是否 OOM 就是它等于 `"OOMKilled"`；**不要**用 `last_exit_code == 137`
//!   推断 OOM（liveness SIGKILL 同样给 137）；
//! - 只保留**最近一次**终止（k8s `lastState` 只有一个槽位），历史次数不可得；
//! - docker 的 `restart_count` 只统计策略重启（实测：手工 `docker restart`
//!   不计数），以事件里的 die→start 恢复对为准，`restart_source` 必须如实标注；
//! - 零值时间（`0001-01-01` / epoch）一律按"未知"处理，返回 `None`。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::AppEventInfo;

/// 采样时间戳，**秒精度**。
///
/// 与 `last_termination.finished_at` / `started_at`（都来自 k8s/Docker 的
/// 秒级时间）保持一致——若这里用 `Utc::now()` 会输出 9 位小数，调用方
/// 解析同一份响应里的时间字段就得处理两种精度。
pub fn sampled_now() -> DateTime<Utc> {
    let now = Utc::now();
    DateTime::from_timestamp(now.timestamp(), 0).unwrap_or(now)
}

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
    /// 物理身份（k8s: Pod UID）。与 `/computer/pod/health` 的 `pod_uid`
    /// **同一个值**——用它判断"容器是否被删除重建"，重建后此值会变、
    /// `restart_count` 归零。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod_uid: Option<String>,
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
    /// 实际用量段（采集不到为 None，不填 0）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<PodUsage>,
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
    /// 业务场景。可选值：`WebAgentRunner` / `ComputerAgentRunner` /
    /// `ComputerNormalProject` / `Userapp` / `UserappBuilder`。
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

/// 单个容器的健康快照（`/computer/pod/health` 的载荷）。
///
/// 与 `PodHealth` 的区别：这是**专供轮询**的轻量视图——只查一个容器、
/// 携带"最近一次终止"的完整证据（含**时间**与**pod_uid**），不返回
/// Pod 列表/事件/spec。
///
/// 语义红线（与 overview 一致）：
/// - `last_termination` 为 `None` = 从未终止过（**不是**空对象）；
/// - 不提供 `last_oom_at` 这类"只在 OOM 时有值"的顶层字段：终止时间在
///   `last_termination.finished_at`，任何原因都有值；
/// - `is_oom` 由 `reason == "OOMKilled"` 判定，**绝不用 exit_code==137
///   推断**（实测 137 也来自 liveness SIGKILL）；
/// - `pod_uid` 变化 = 容器被删除重建（计数归零），前端据此识别换代。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ContainerHealth {
    /// **稳定的容器标识**（k8s: workload 名，如
    /// `rcoder-computer-agent-runner-6`；docker: 容器名）。
    ///
    /// 用它与 `/computer/pod/resources`、`/computer/pod/overview` 的
    /// `workload.name` 对齐——**跨容器重建不变**，适合做业务侧的关联键。
    /// 想看具体 Pod（排障 / kubectl logs）用 `pod_name`。
    pub container_name: String,
    /// 实际 Pod 名（k8s: `metadata.name`，如
    /// `rcoder-computer-agent-runner-6-0`；docker: 与 `container_name` 相同）。
    ///
    /// 与 `/computer/pod/overview` 的 `items[].name` **同一个值**——两接口
    /// 用它可以互相定位。容器重建后序号可能变化，**不要**拿它做长期关联键。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod_name: Option<String>,
    /// 物理身份（k8s: Pod UID；docker: container ID）。
    /// 变化即代表"换了一个容器"，历史计数不再可比。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod_uid: Option<String>,
    /// 容器是否存活（k8s：phase/state；docker：State.Running）。
    /// **容器死了这个接口照样能查**——数据源在运行时侧，不在容器内。
    pub alive: bool,
    /// 运行阶段（k8s: Pod phase；docker: running/exited/…）
    pub phase: String,
    /// 主容器是否 ready
    pub ready: bool,
    /// 累计重启次数（含非 OOM）
    pub restart_count: u32,
    /// **最近一次终止**的完整信息（任何原因：OOM / 应用崩溃 / 正常结束）。
    /// `None` = 从未终止过。
    ///
    /// 字段设计刻意**不以 OOM 为主语**：容器重启大多源于 OOM，但并不都是
    /// ——把 `oom_killed_last` / `last_oom_at` 这类字段摆在顶层，会让调用方
    /// 误以为"非 OOM 时没有信息"。真正的通用事实是"最近一次怎么终止的"，
    /// 是否 OOM 只是 `last_termination.reason` 的一个取值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_termination: Option<LastTermination>,
    /// 本次容器**启动时间**（k8s: `state.running.startedAt`；docker:
    /// `State.StartedAt`）。与 `last_termination.finished_at` 相邻比较可算出
    /// 这次重启花了多久；容器当前不在运行时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// 当前等待原因（CrashLoopBackOff 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<String>,
    /// 可读详情（k8s: back-off 描述；docker: State.Error 等启动/运行错误）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 本次采样时间（前端据此判断数据新鲜度）
    pub sampled_at: DateTime<Utc>,
    /// 数据来源："kubernetes" / "docker"
    pub source: String,
}

/// 最近一次终止/重启的证据（**不论原因**）。
///
/// 三个事实字段（`reason` / `exit_code` / `finished_at`）在非 OOM 时同样
/// 完整；`is_oom` 只是 `reason == "OOMKilled"` 的派生便捷位。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LastTermination {
    /// 终止原因：`OOMKilled` / `Error` / `Completed` / `ContainerCannotRun`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 退出码（137=SIGKILL 但**不等于** OOM）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 终止时刻（k8s `lastState.terminated.finishedAt`；docker die 事件时间）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// 是否 OOM（派生自 `reason == "OOMKilled"`）。
    ///
    /// 提供这个便捷位是为了**阻止调用方用 `exit_code == 137` 推断 OOM**：
    /// 137 同样来自 liveness 探针 SIGKILL 或人工 kill，只有 reason 能定性。
    pub is_oom: bool,
}

/// 实际用量段（P2：k8s metrics-server / agent gRPC / docker stats）。
///
/// **整段可缺省**：采集不到就是 `None`，绝不填 0——0 会被读成"空载"。
/// `source` 与 `window` 必须回显：agent gRPC 是实时值，metrics-server 是
/// 采样窗口内的平均值，两者不可直接对比。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodUsage {
    /// 数据来源："agent_grpc"（实时）| "metrics_server"（~15s 窗口）| "docker_stats"
    pub source: String,
    /// 采样窗口（metrics-server 的 `window`，如 "15s"；gRPC 为 None = 实时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// CPU 使用（核）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<f64>,
    /// CPU 使用率（%）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    /// `cpu_percent` 的分母口径："limit"（占上限）| "request"（占申请）| "none"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent_basis: Option<String>,
    /// 内存使用（MB）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_used_mb: Option<i64>,
    /// 内存限额（MB，用于算使用率；来源同 spec）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_limit_mb: Option<i64>,
    /// 进程数（仅 agent gRPC / docker stats 有）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u32>,
}

/// 健康段。数值语义见模块头注释。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodHealth {
    /// 重启次数。k8s：containerStatuses.restartCount；
    /// docker：事件里 restart/die 计数（RestartCount 只统计策略重启，仅参考）
    pub restart_count: u32,
    /// 重启计数的来源："container_status"（k8s）| "docker_events"（docker）
    pub restart_source: String,
    /// **最近一次终止**的完整信息（任何原因），与
    /// `/computer/pod/health` 的 `health.last_termination` **结构完全一致**
    /// —— 两个接口对同一概念用同一个形状，调用方只需一套解析逻辑。
    ///
    /// `None` = 从未终止过。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_termination: Option<LastTermination>,
    /// k8s waiting 原因（CrashLoopBackOff / ContainerCreating / …）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<String>,
    /// 可读详情
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 一次概览采集的结果：Pod 列表 + 分区降级说明 + 后端声明。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodOverviewSnapshot {
    pub pods: Vec<PodOverviewItem>,
    /// 分区级降级（如 events 列取失败）；空 = 全部成功。
    /// **"空数组"与"取不到"必须可区分**——靠这里，不靠猜。
    pub degraded: Vec<PodOverviewDegraded>,
    /// 后端形态："kubernetes" / "docker"。由**采集方自己声明**——
    /// 不能从"某个生命周期方法是否实现"推断（docker 运行时也实现了
    /// 命名域方法，返回 docker:<addr>，据此推断会把 docker 标成 k8s）。
    #[serde(default)]
    pub source: String,
    /// namespace（k8s；docker 为 None）。
    #[serde(default)]
    pub namespace: Option<String>,
}

/// 单个分区降级说明。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodOverviewDegraded {
    /// 失败的分区：events / pvc / …
    pub section: String,
    /// 机器可读的稳定标识（如 `docker_events_partial`、`pvc_list_failed`）。
    /// 客户端做程序化判断或本地化文案时以 `section` + `reason` 为准。
    pub reason: String,
    /// 人类可读的补充说明，**默认英文**（可能附带运行时的原始错误串）。
    /// 响应有服务端缓存、跨请求复用，所以这里不按请求语言渲染；需要本地化
    /// 文案的客户端请按 `reason` 自行映射。不要写入中文。
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
            pod_uid: None,
            phase: "Running".to_string(),
            ready: true,
            node: None,
            ip: None,
            created_at: None,
            spec: Default::default(),
            health: Default::default(),
            usage: None,
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
            source: "kubernetes".to_string(),
            namespace: None,
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
