//! 部署等待循环的确定性失败信号（计划 v3.2 批 1）。
//!
//! 数据源为结构化 Pod 观察（[`container_runtime_api::PodObservation`]）——
//! K8s 原生字段直读，不解析自由文本。全部判据是**提前终止策略**而非
//! "不可能自愈证明"：触发后等待立即返回带诊断的 Backend 错误，操作经
//! 既有错误分支进入 `RecoveryRequired` 并保留租约（不做 fail_confirmed /
//! mark_completed / 自动解锁——观察时刻不构成写入静默证明）。
//!
//! 身份前置（计划 1a）：仅 `matches_target_template == true` 的观察参与
//! 分类——owner 链归属本次 Deployment UID ∧ pod template 携带本次操作
//! 令牌；防抖（同 pod_uid 连续轮）只处理观察抖动，不替代身份核验。

use container_runtime_api::{PodFailureObservation, PodObservation};

/// 失败信号阈值（全局配置；次数不带时间单位）。
#[derive(Debug, Clone)]
pub(crate) struct FailureSignalThresholds {
    /// CrashLoop 判定的重启次数下限（默认 3）
    pub crash_restart: u32,
    /// OOM 重启风暴判定的重启次数下限（默认 2；K8s 不暴露分原因计数，
    /// 只能声称"重启风暴且最近一次退出为 OOMKilled"）
    pub oom_restart: u32,
}

impl Default for FailureSignalThresholds {
    fn default() -> Self {
        Self {
            crash_restart: 3,
            oom_restart: 2,
        }
    }
}

/// 确定性故障分类（提前终止策略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FailureClassification {
    /// 从未 Ready 的反复崩溃退避
    CrashLoopBackOff { restarts: u32 },
    /// 重启风暴且最近一次退出为 OOMKilled
    OomRestartStorm { restarts: u32 },
    /// 镜像拉取永久失败（NotFound/Unauthorized/denied——网络/DNS/registry
    /// 暂时故障不在此列，交给无进展看门狗）
    PermanentImagePull,
}

impl FailureClassification {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::CrashLoopBackOff { restarts } => {
                format!("CrashLoopBackOff (never ready, restart_count={restarts})")
            }
            Self::OomRestartStorm { restarts } => {
                format!("restart storm with latest exit OOMKilled (restart_count={restarts})")
            }
            Self::PermanentImagePull => "permanent image pull failure".to_string(),
        }
    }
}

/// 拉取失败 message 的永久性判据：仅这三类（K8s 语义上不会自愈）；
/// 限流/网络/DNS 等 message 不匹配 → 归看门狗。
fn permanent_pull_failure(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("notfound") || lowered.contains("unauthorized") || lowered.contains("denied")
}

/// 单轮观察的纯分类函数（判据矩阵单测入口）。
///
/// `never_ready`：本等待期内从未观察到 Ready（Ready 过一次的容器崩溃属
/// 运行期故障域，不是部署确定性失败）。
pub(crate) fn classify_failure(
    observation: &PodFailureObservation,
    never_ready: bool,
    thresholds: &FailureSignalThresholds,
) -> Option<FailureClassification> {
    if !observation.matches_target_template || !never_ready {
        return None;
    }
    if observation.restart_count == 0 {
        return None;
    }
    if observation.container_waiting_reason.as_deref() == Some("CrashLoopBackOff")
        && observation.restart_count >= thresholds.crash_restart
    {
        return Some(FailureClassification::CrashLoopBackOff {
            restarts: observation.restart_count,
        });
    }
    if observation.container_waiting_reason.as_deref() == Some("ImagePullBackOff")
        && let Some(message) = observation.container_waiting_message.as_deref()
        && permanent_pull_failure(message)
    {
        return Some(FailureClassification::PermanentImagePull);
    }
    if observation.restart_count >= thresholds.oom_restart
        && observation
            .container_last_exit
            .as_ref()
            .map(|exit| exit.reason.as_deref() == Some("OOMKilled"))
            .unwrap_or(false)
    {
        return Some(FailureClassification::OomRestartStorm {
            restarts: observation.restart_count,
        });
    }
    None
}

/// 跨轮防抖跟踪器：同一 pod_uid（且身份匹配）连续 ≥2 轮呈现同类故障
/// 且 restart_count 不减才触发。pod 更换即重置（换代期旧 pod 的故障
/// 不可能借新等待的防抖窗口）。
#[derive(Default)]
pub(crate) struct FailureSignalTracker {
    last: Option<(String, FailureClassification, u32)>,
    consecutive: u32,
    ever_ready: bool,
}

impl FailureSignalTracker {
    /// 消费一轮观察，返回达标的确定性故障分类（未达标 None）。
    /// 任何身份匹配的 pod 出现 Ready → `ever_ready` 永久置位（本等待期
    /// 不再产生部署确定性失败分类）。
    pub(crate) fn observe(
        &mut self,
        observations: &[PodObservation],
        thresholds: &FailureSignalThresholds,
    ) -> Option<FailureClassification> {
        let mut matched: Option<(&PodFailureObservation, FailureClassification)> = None;
        for observation in observations {
            let PodObservation::Observed(observation) = observation else {
                continue;
            };
            if observation.matches_target_template && observation.ready {
                self.ever_ready = true;
            }
            if let Some(classification) =
                classify_failure(observation, !self.ever_ready, thresholds)
                && matched.is_none()
            {
                matched = Some((observation, classification));
            }
        }
        let Some((observation, classification)) = matched else {
            self.last = None;
            self.consecutive = 0;
            return None;
        };
        let candidate = (
            observation.pod_uid.clone(),
            classification,
            observation.restart_count,
        );
        match &self.last {
            Some((uid, last_classification, last_restarts))
                if *uid == candidate.0
                    && *last_classification == candidate.1
                    && candidate.2 >= *last_restarts =>
            {
                self.consecutive += 1;
            }
            _ => {
                self.last = Some(candidate.clone());
                self.consecutive = 1;
            }
        }
        if self.consecutive >= 2 {
            Some(candidate.1)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use container_runtime_api::ContainerExit;

    fn observation(
        waiting_reason: Option<&str>,
        waiting_message: Option<&str>,
        last_exit: Option<ContainerExit>,
        restarts: u32,
        ready: bool,
    ) -> PodFailureObservation {
        PodFailureObservation {
            deployment_uid: "deploy-uid".into(),
            matches_target_template: true,
            pod_uid: "pod-1".into(),
            pod_phase: if ready { "Running" } else { "Pending" }.into(),
            scheduled: Some(true),
            scheduling_reason: None,
            container_waiting_reason: waiting_reason.map(str::to_string),
            container_waiting_message: waiting_message.map(str::to_string),
            container_last_exit: last_exit,
            restart_count: restarts,
            ready,
        }
    }

    fn thresholds() -> FailureSignalThresholds {
        FailureSignalThresholds {
            crash_restart: 3,
            oom_restart: 2,
        }
    }

    #[test]
    fn crashloop_requires_threshold_and_never_ready() {
        let mut tracker = FailureSignalTracker::default();
        let under = observation(Some("CrashLoopBackOff"), None, None, 2, false);
        assert_eq!(classify_failure(&under, true, &thresholds()), None);
        let at = observation(Some("CrashLoopBackOff"), None, None, 3, false);
        assert_eq!(
            classify_failure(&at, true, &thresholds()),
            Some(FailureClassification::CrashLoopBackOff { restarts: 3 })
        );
        // 曾经 Ready → 不再是部署确定性失败
        assert_eq!(classify_failure(&at, false, &thresholds()), None);
        // 单轮观察不触发（防抖）
        assert_eq!(
            tracker.observe(&[PodObservation::Observed(at.clone())], &thresholds()),
            None
        );
        // 第二轮同 pod 同分类 → 触发
        assert_eq!(
            tracker.observe(&[PodObservation::Observed(at)], &thresholds()),
            Some(FailureClassification::CrashLoopBackOff { restarts: 3 })
        );
    }

    #[test]
    fn debounce_resets_when_pod_or_classification_changes() {
        let mut tracker = FailureSignalTracker::default();
        let first = observation(Some("CrashLoopBackOff"), None, None, 3, false);
        assert_eq!(
            tracker.observe(&[PodObservation::Observed(first)], &thresholds()),
            None
        );
        // pod 更换（换代）→ 基线重置
        let mut replacement = observation(Some("CrashLoopBackOff"), None, None, 3, false);
        replacement.pod_uid = "pod-2".into();
        assert_eq!(
            tracker.observe(&[PodObservation::Observed(replacement)], &thresholds()),
            None
        );
        // restart 回落同样不续期（新基线）
        let mut lower = observation(Some("CrashLoopBackOff"), None, None, 2, false);
        lower.pod_uid = "pod-2".into();
        assert_eq!(
            tracker.observe(&[PodObservation::Observed(lower)], &thresholds()),
            None
        );
    }

    #[test]
    fn ever_ready_permanently_disables_classification() {
        let mut tracker = FailureSignalTracker::default();
        let ready_then_crash = observation(None, None, None, 0, true);
        tracker.observe(&[PodObservation::Observed(ready_then_crash)], &thresholds());
        let crash = observation(Some("CrashLoopBackOff"), None, None, 5, false);
        assert_eq!(
            tracker.observe(&[PodObservation::Observed(crash)], &thresholds()),
            None,
            "Ready 过一次的崩溃属运行期故障域"
        );
    }

    #[test]
    fn non_matching_template_never_classifies() {
        let mut foreign = observation(Some("CrashLoopBackOff"), None, None, 5, false);
        foreign.matches_target_template = false;
        let mut tracker = FailureSignalTracker::default();
        assert_eq!(
            tracker.observe(&[PodObservation::Observed(foreign)], &thresholds()),
            None
        );
        assert_eq!(
            tracker.observe(
                &[PodObservation::Observed(observation(
                    Some("CrashLoopBackOff"),
                    None,
                    None,
                    5,
                    false
                ))],
                &thresholds()
            ),
            None,
            "第一轮仍不触发"
        );
    }

    #[test]
    fn rate_limited_or_transient_pull_is_not_permanent() {
        let transient = observation(
            Some("ImagePullBackOff"),
            Some("toomanyrequests: You have reached your pull rate limit"),
            None,
            3,
            false,
        );
        assert_eq!(classify_failure(&transient, true, &thresholds()), None);
        let network = observation(
            Some("ImagePullBackOff"),
            Some("dial tcp: i/o timeout"),
            None,
            3,
            false,
        );
        assert_eq!(classify_failure(&network, true, &thresholds()), None);
        let not_found = observation(
            Some("ImagePullBackOff"),
            Some("pull access denied, repository does not exist or may require authorization"),
            None,
            1,
            false,
        );
        assert_eq!(
            classify_failure(&not_found, true, &thresholds()),
            Some(FailureClassification::PermanentImagePull)
        );
    }

    #[test]
    fn oom_requires_latest_exit_oomkilled_and_threshold() {
        let oom_exit = ContainerExit {
            code: 137,
            reason: Some("OOMKilled".into()),
        };
        let other_exit = ContainerExit {
            code: 1,
            reason: Some("Error".into()),
        };
        // 更早一次 OOM + 其他原因重启、当前 last_exit 非 OOM → 不产生 **OOM 分类**
        // （重启风暴达 CrashLoop 阈值时按 CrashLoop 分类——语义正确，不是 OOM 冒名）
        let latest_not_oom = observation(
            Some("CrashLoopBackOff"),
            None,
            Some(other_exit.clone()),
            4,
            false,
        );
        assert_eq!(
            classify_failure(&latest_not_oom, true, &thresholds()),
            Some(FailureClassification::CrashLoopBackOff { restarts: 4 })
        );
        // 低于 CrashLoop 阈值 + last_exit 非 OOM → 完全不触发
        let below_both = observation(Some("CrashLoopBackOff"), None, Some(other_exit), 2, false);
        assert_eq!(classify_failure(&below_both, true, &thresholds()), None);
        let latest_oom = observation(
            Some("CrashLoopBackOff"),
            None,
            Some(oom_exit.clone()),
            2,
            false,
        );
        assert_eq!(
            classify_failure(&latest_oom, true, &thresholds()),
            Some(FailureClassification::OomRestartStorm { restarts: 2 })
        );
        let single_oom = observation(Some("CrashLoopBackOff"), None, Some(oom_exit), 1, false);
        assert_eq!(classify_failure(&single_oom, true, &thresholds()), None);
    }

    #[test]
    fn uncertain_or_no_pod_observations_are_inert() {
        let mut tracker = FailureSignalTracker::default();
        assert_eq!(
            tracker.observe(
                &[PodObservation::UncertainIdentity("chain broken".into())],
                &thresholds()
            ),
            None
        );
        assert_eq!(
            tracker.observe(&[PodObservation::NoPod], &thresholds()),
            None
        );
    }
}
