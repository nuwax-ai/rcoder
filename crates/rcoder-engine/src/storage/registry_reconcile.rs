//! 契约三：对账处置表（注册表零包袱方案 §2 契约三）。
//!
//! 对账输入 = 注册表绑定（container_id=Pod UID、workload_uid、workload 名、
//! IP hint）× live 解析结果（[`PodResolution`](docker_manager) 或 Docker
//! inspect 结果）。处置判定为**纯函数**（逐行配测试）；写侧经既有注册/
//! 代次机制（prepare_info 的 generation+predecessor 事务），本模块不直接
//! 授权任何写——生命周期写入约束：注册表 CAS 成功 ≠ UserApp 生命周期
//! 操作授权，对账与 stop/restart/recovery 并发时按生命周期屏障允许的
//! 规则更新（观察状态更新/注册换代/显式恢复三类路径各自允许的记录清单
//! 由调用方应用）。
//!
//! 保守面：`Unknown`/`WorkloadReplaced`/`Unowned` 永不退休、不清绑定、
//! 不放行清理或接管（处置表末三行语义）。
use shared_types::ServiceType;

/// 注册表侧绑定身份（对账输入；从 ContainerBasicInfo 投影）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisteredBinding {
    /// 注册的物理 UID（K8s Pod UID / Docker 容器 ID）。
    pub physical_uid: String,
    /// 注册的 workload UID（K8s STS UID；Docker 恒 None）。
    pub workload_uid: Option<String>,
    /// workload 名（container_name，契约一语义）。
    pub workload_name: String,
    /// 注册的 IP hint。
    pub ip: String,
}

/// live 观察结论（对 [`docker_manager` 解析结果]与本模块解耦的投影——
/// Docker 侧以 inspect 结果填同一形状）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveObservation {
    /// 找到存活实例：物理 UID / workload UID / IP / 是否就绪。
    Present {
        physical_uid: String,
        workload_uid: Option<String>,
        ip: String,
        running: bool,
    },
    /// workload 对象仍在、Pod 暂缺（重建中）。
    // 处置表行4/5/6/8 的判定面：K8s 解析器（k8s_resolution::PodResolution）
    // 到本投影的映射随处置执行器接入（跨运行时 trait 尚无该形状）；逐行
    // 测试已锁定语义，先保留 API 面。
    #[allow(dead_code)]
    WorkloadWithoutPod,
    #[allow(dead_code)]
    /// workload 与 Pod 均确认不存在（404 证据）。
    Absent { evidence: String },
    #[allow(dead_code)]
    /// 多候选身份冲突。
    Conflict { reason: String },
    /// API 错误/观察不完整。
    Unknown { reason: String },
    /// Docker 专属：容器 Exited（观察停态）。
    Exited,
}

/// 处置表逐行产物。除 `RefreshHint`/`ObserveGenerationChange` 外均为
/// **非写处置**——保守观察或不动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// 行1：同物理 UID，仅地址/状态变化 → 调用方经既有注册路径 CAS 更新
    /// hint（generation 不变，revision 递增）。
    RefreshHint { ip: String, running: bool },
    /// 行2/行7（物理 ID 变化）：同 workload_uid、新 Pod UID → §1.2 换代
    /// 事务（新 generation + predecessor 捕获，获准 projects/sessions
    /// 原子重绑）——由注册/业务操作路径执行，对账只产出判定。
    ObserveGenerationChange {
        new_physical_uid: String,
        workload_uid: Option<String>,
    },
    /// 行3：同名 workload UID 变化 → 冲突/显式恢复，不自动接管。
    WorkloadReplaced { observed_workload_uid: String },
    /// 行4：Pod 暂缺、workload 期望运行 → 重建中，不判停。
    Rebuilding,
    /// 行5/行6：replicas=0 Pod 消失，或绑定双缺（workload 与 Pod 均不在）
    /// → 收敛**观察停态**，不删历史/卷、不自动重建；恢复走显式恢复或
    /// RecoveryRequired 围栏。
    ObservedStopped { evidence: String },
    /// 行7（Docker Exited）：观察停态。
    ObservedStoppedDocker,
    /// 行8：Unknown/Conflict → 不退休、不清绑定、不放行清理或接管。
    Unknown { reason: String },
}

/// 处置表判定（纯函数，逐行测试见 tests）。
///
/// service_type 只影响文档语义注释；K8s/Docker 的差异经
/// [`LiveObservation`] 形状表达（Exited 仅 Docker 投影产生）。
pub(crate) fn disposition(
    registered: &RegisteredBinding,
    live: &LiveObservation,
    _service_type: &ServiceType,
) -> Disposition {
    match live {
        LiveObservation::Present {
            physical_uid,
            workload_uid,
            ip,
            running,
        } => {
            let same_physical = *physical_uid == registered.physical_uid;
            let workload_matches =
                match (workload_uid.as_deref(), registered.workload_uid.as_deref()) {
                    // 双方都有 workload UID：必须一致才是"同 workload"。
                    (Some(observed), Some(registered_uid)) => observed == registered_uid,
                    // 任一方缺失（Docker/bare-pod/Deployment 族）：退化为物理与
                    // 名字判定——名字由发现层 selector 保证。
                    _ => true,
                };
            if same_physical {
                // 行1：同物理 UID——地址/状态变化走 hint 刷新。
                Disposition::RefreshHint {
                    ip: ip.clone(),
                    running: *running,
                }
            } else if workload_matches {
                // 行2/行7：同 workload 新物理 ID——换代观察。
                Disposition::ObserveGenerationChange {
                    new_physical_uid: physical_uid.clone(),
                    workload_uid: workload_uid.clone(),
                }
            } else {
                // 行3：同名 workload 已被替换——显式恢复，不自动接管。
                Disposition::WorkloadReplaced {
                    observed_workload_uid: workload_uid.clone().unwrap_or_default(),
                }
            }
        }
        LiveObservation::WorkloadWithoutPod => Disposition::Rebuilding,
        LiveObservation::Absent { evidence } => Disposition::ObservedStopped {
            evidence: evidence.clone(),
        },
        LiveObservation::Exited => Disposition::ObservedStoppedDocker,
        LiveObservation::Conflict { reason } => Disposition::Unknown {
            reason: format!("live candidates conflict: {reason}"),
        },
        LiveObservation::Unknown { reason } => Disposition::Unknown {
            reason: reason.clone(),
        },
    }
}

/// tombstone/无 PG 归属守卫（行9）：对账不复活、不导入、不删除——调用方
/// 在处置执行前调用；命中即整行跳过（返回 None 语义）。
pub(crate) fn owned_by_registry(has_pg_ownership: bool, tombstoned: bool) -> bool {
    !tombstoned && has_pg_ownership
}

// ---------------------------------------------------------------------------
// 周期补偿观察（契约三触发点之一）。
// ---------------------------------------------------------------------------

use std::sync::Weak;
use tokio::{sync::broadcast, task::JoinHandle};

use crate::app_state::AppState;

/// 单轮对账条目上限（有界扫描；下一轮继续覆盖）。
const RECONCILE_PAGE_LIMIT: usize = 64;
/// 对账周期（补偿观察，非故障恢复通道）。
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// 周期对账观察任务：按注册表绑定核对 live 状态并按处置表留痕。
///
/// **观察面-only**（生命周期写入约束）：hint 刷新与换代重绑的写侧由注册/
/// 业务操作路径经 prepare_info 的 generation+predecessor 代次事务执行——
/// 后台对账不越权写注册表，只产出可审计的处置结论（异常形态 warn 留痕，
/// 供显式恢复与人工归因）。持有 `Weak<AppState>`：关机广播后自行退出，
/// 不阻塞 R02 关机门。
pub(crate) fn start_registry_reconcile(
    state: Weak<AppState>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = interval.tick() => {
                    let Some(state) = state.upgrade() else { break };
                    if let Err(error) = reconcile_once(&state).await {
                        tracing::warn!(%error, "Registry reconcile sweep failed");
                    }
                }
            }
        }
        tracing::info!("Registry reconcile observer drained and exited");
    })
}

async fn reconcile_once(state: &AppState) -> anyhow::Result<()> {
    use container_runtime_api::ContainerRuntimeStatus;
    use shared_types::ProjectStore as _;

    let mut checked = 0usize;
    for (project_id, info) in state.projects.iter() {
        if checked >= RECONCILE_PAGE_LIMIT {
            break;
        }
        let Some(basic) = info.container_info() else {
            continue;
        };
        let Some(service_type) = info.service_type() else {
            continue;
        };
        // 行9守卫：tombstone（retired/已删项目残留绑定）不复活、不导入、不删除。
        let tombstoned =
            info.persistence_identity().predecessor.is_some() && info.sessions().is_empty();
        if !owned_by_registry(true, tombstoned) {
            continue;
        }
        checked += 1;
        // 定位键与官方 identity 优先级一致（container_key：pod_id/user_id/project_id）
        let identifier = info.container_key().to_string();
        let live = match state
            .runtime
            .find_container(&identifier, &service_type)
            .await
        {
            // find 的族分流与兜底链见 docker_manager；None 在此保守归
            // Unknown（owner 对象缺席证明是 K8s 解析器内部能力，跨运行时
            // 不可假定——对账永不以 None 授权退休/清理）。
            Ok(Some(found)) => {
                let running = found.status == ContainerRuntimeStatus::Running;
                if running {
                    LiveObservation::Present {
                        physical_uid: found.container_id,
                        workload_uid: found.workload_uid,
                        ip: found.container_ip,
                        running: true,
                    }
                } else if shared_types::is_kubernetes_runtime() {
                    // K8s 未就绪仍是"在场未就绪"（Pending/Failed pod），行1 状态刷新。
                    LiveObservation::Present {
                        physical_uid: found.container_id,
                        workload_uid: found.workload_uid,
                        ip: found.container_ip,
                        running: false,
                    }
                } else {
                    // Docker 静止容器（Exited/Stopped）= 行7 观察停态。
                    LiveObservation::Exited
                }
            }
            Ok(None) => LiveObservation::Unknown {
                reason: "no live candidate found by runtime lookup".into(),
            },
            Err(error) => LiveObservation::Unknown {
                reason: format!("runtime lookup failed: {error}"),
            },
        };
        let binding = RegisteredBinding {
            physical_uid: basic.container_id.clone(),
            workload_uid: basic.workload_uid.clone(),
            workload_name: basic.container_name.clone(),
            ip: basic.container_ip.clone(),
        };
        match disposition(&binding, &live, &service_type) {
            Disposition::RefreshHint { .. } | Disposition::Rebuilding => {}
            Disposition::ObserveGenerationChange {
                new_physical_uid, ..
            } => {
                tracing::info!(
                    project_id = %project_id,
                    workload = %binding.workload_name,
                    registered_uid = %binding.physical_uid,
                    observed_uid = %new_physical_uid,
                    "Registry reconcile: workload generation changed; rebinding happens on the next admitted registration"
                );
            }
            Disposition::WorkloadReplaced {
                observed_workload_uid,
            } => {
                tracing::warn!(
                    project_id = %project_id,
                    workload = %binding.workload_name,
                    registered_workload_uid = ?binding.workload_uid,
                    observed_workload_uid = %observed_workload_uid,
                    "Registry reconcile: workload object replaced (same name); automatic takeover is forbidden"
                );
            }
            Disposition::ObservedStopped { evidence } => {
                tracing::info!(
                    project_id = %project_id,
                    workload = %binding.workload_name,
                    %evidence,
                    "Registry reconcile: bound compute observed stopped; history and volumes retained"
                );
            }
            Disposition::ObservedStoppedDocker => {
                tracing::info!(
                    project_id = %project_id,
                    workload = %binding.workload_name,
                    "Registry reconcile: container exited (observed stopped)"
                );
            }
            Disposition::Unknown { reason } => {
                tracing::warn!(
                    project_id = %project_id,
                    workload = %binding.workload_name,
                    %reason,
                    "Registry reconcile: live state unknown; no retirement or cleanup authorized"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(workload_uid: Option<&str>) -> RegisteredBinding {
        RegisteredBinding {
            physical_uid: "pod-old".into(),
            workload_uid: workload_uid.map(str::to_string),
            workload_name: "rcoder-web-proj-1".into(),
            ip: "10.0.0.1".into(),
        }
    }

    fn present(physical_uid: &str, workload_uid: Option<&str>, ip: &str) -> LiveObservation {
        LiveObservation::Present {
            physical_uid: physical_uid.into(),
            workload_uid: workload_uid.map(str::to_string),
            ip: ip.into(),
            running: true,
        }
    }

    fn sts() -> ServiceType {
        ServiceType::WebAgentRunner
    }

    /// 行1：同物理 UID 仅地址变化 → hint 刷新（不换代）。
    #[test]
    fn same_physical_uid_refreshes_hint() {
        let binding = registered(Some("sts-uid-1"));
        let live = present("pod-old", Some("sts-uid-1"), "10.0.0.9");
        assert_eq!(
            disposition(&binding, &live, &sts()),
            Disposition::RefreshHint {
                ip: "10.0.0.9".into(),
                running: true
            }
        );
    }

    /// 行2：同 workload_uid 新 Pod UID → 换代观察（事务由注册路径执行）。
    #[test]
    fn same_workload_new_pod_uid_is_generation_change() {
        let binding = registered(Some("sts-uid-1"));
        let live = present("pod-new", Some("sts-uid-1"), "10.0.0.2");
        assert_eq!(
            disposition(&binding, &live, &sts()),
            Disposition::ObserveGenerationChange {
                new_physical_uid: "pod-new".into(),
                workload_uid: Some("sts-uid-1".into())
            }
        );
    }

    /// 行3：同名 workload UID 变化 → 不自动接管（最危险分支）。
    #[test]
    fn replaced_workload_never_auto_taken_over() {
        let binding = registered(Some("sts-uid-1"));
        let live = present("pod-x", Some("sts-uid-2"), "10.0.0.3");
        assert_eq!(
            disposition(&binding, &live, &sts()),
            Disposition::WorkloadReplaced {
                observed_workload_uid: "sts-uid-2".into()
            }
        );
    }

    /// 行4：workload 在、Pod 暂缺 → 重建中，不判停。
    #[test]
    fn workload_without_pod_is_rebuilding() {
        assert_eq!(
            disposition(
                &registered(Some("sts-uid-1")),
                &LiveObservation::WorkloadWithoutPod,
                &sts()
            ),
            Disposition::Rebuilding
        );
    }

    /// 行5/6：双缺 → 观察停态（不删历史/卷、不自动重建）。
    #[test]
    fn confirmed_absence_converges_to_observed_stopped() {
        assert_eq!(
            disposition(
                &registered(Some("sts-uid-1")),
                &LiveObservation::Absent {
                    evidence: "statefulset rcoder-web-proj-1 not found".into()
                },
                &sts()
            ),
            Disposition::ObservedStopped {
                evidence: "statefulset rcoder-web-proj-1 not found".into()
            }
        );
    }

    /// 行7：Docker Exited → 观察停态；同容器名换物理 ID 走换代。
    #[test]
    fn docker_exited_and_id_change() {
        let binding = registered(None);
        assert_eq!(
            disposition(&binding, &LiveObservation::Exited, &sts()),
            Disposition::ObservedStoppedDocker
        );
        // Docker 无 workload UID（None）：物理 ID 变化退化为换代观察（容器名
        // 即 workload 身份，名字由发现层保证）。
        let live = present("docker-new-id", None, "172.17.0.5");
        assert_eq!(
            disposition(&binding, &live, &sts()),
            Disposition::ObserveGenerationChange {
                new_physical_uid: "docker-new-id".into(),
                workload_uid: None
            }
        );
    }

    /// 行8：API 错误/多候选冲突 → Unknown，不放行任何清理/接管。
    #[test]
    fn unknown_and_conflict_never_authorize_anything() {
        for live in [
            LiveObservation::Conflict {
                reason: "two workloads".into(),
            },
            LiveObservation::Unknown {
                reason: "api error".into(),
            },
        ] {
            assert!(
                matches!(
                    disposition(&registered(Some("sts-uid-1")), &live, &sts()),
                    Disposition::Unknown { .. }
                ),
                "conflict/unknown must stay unknown"
            );
        }
    }

    /// 行9：tombstone 命中或无 PG 归属 → 不复活、不导入、不删除。
    #[test]
    fn tombstone_or_unowned_rows_are_skipped() {
        assert!(
            !owned_by_registry(true, true),
            "tombstoned rows are not owned"
        );
        assert!(
            !owned_by_registry(false, false),
            "unowned rows are not owned"
        );
        assert!(owned_by_registry(true, false));
    }
}
