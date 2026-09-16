//! Kubernetes 私有观察模块（kube-runtime 接入批次 A，specs/kube-runtime-adoption）。
//!
//! 仅承载 Kubernetes 流消费、总截止时间与错误分类；**不含任何写操作能力**
//! （不创建/删除/patch、不形成通用协调器）。业务判定（Ready 语义、Builder
//! 双资源校验）留在各业务模块，经纯分类器回调注入。
//!
//! 不变量（spec KR01–KR06）：
//! - 总 deadline 覆盖初始 LIST、连接、重连、退避与最后复核，嵌套不重置预算；
//! - 取消仅终止本观察（drop 流）；HTTP 等待者取消不取消已受理共享操作；
//! - 403/不可恢复错误及时失败（保留 API 分类）；断连/410/正常 EOF 由
//!   kube-runtime watcher 按其协议续接（锁定版本 4.2 自带 RV 恢复与退避
//!   重连——不自行实现 RV 协议），均在原预算内恢复；
//! - 意外整体流结束是观察失败，不当作业务成功，不永久悬挂；
//! - watch 是观察：观察超时/断连不授权释放操作租约或重新创建。

#[cfg(feature = "kubernetes")]
use std::time::Instant;

#[cfg(feature = "kubernetes")]
use futures_util::StreamExt as _;
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::Pod;
#[cfg(feature = "kubernetes")]
use kube::runtime::watcher;

/// R09：暂态错误重试退避（200ms 起、指数 ×2、2s 封顶）——覆盖 kube-runtime
/// 4.2 watcher 的立即恢复（其源码注释明确恢复发生在 next poll，需调用方
/// 用退避机制加延迟）。退避等待受统一 deadline 与取消控制（KR03/KR04）。
#[cfg(feature = "kubernetes")]
const BACKOFF_INITIAL: std::time::Duration = std::time::Duration::from_millis(200);
#[cfg(feature = "kubernetes")]
const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(2);

/// 单次观察的结构化结果错误（保留 API 分类，不退化成字符串识别）。
#[cfg(feature = "kubernetes")]
#[derive(Debug)]
pub(crate) enum ObservationError {
    /// 总预算耗尽（附最后观察到的暂态原因，可能为空）。
    Deadline { last_transient: Option<String> },
    /// 调用方取消（本观察的流已释放；不影响已受理共享操作）。
    Cancelled,
    /// 不可恢复错误：403/认证失败/资源类型不支持等（附 API code）。
    Fatal { code: Option<u16>, message: String },
    /// 意外整体流结束（watcher 终止且无业务结论）。
    StreamEnded { message: String },
}

#[cfg(feature = "kubernetes")]
impl std::fmt::Display for ObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObservationError::Deadline { last_transient } => match last_transient {
                Some(reason) => write!(
                    f,
                    "observation deadline exceeded (last transient: {reason})"
                ),
                None => write!(f, "observation deadline exceeded"),
            },
            ObservationError::Cancelled => write!(f, "observation cancelled"),
            ObservationError::Fatal { code, message } => match code {
                Some(code) => write!(f, "unrecoverable apiserver error ({code}): {message}"),
                None => write!(f, "unrecoverable observation error: {message}"),
            },
            ObservationError::StreamEnded { message } => {
                write!(f, "watch stream ended without a verdict: {message}")
            }
        }
    }
}

#[cfg(feature = "kubernetes")]
impl ObservationError {
    /// API 分类码提取（watcher::Error 各源；保留 API code，传输错误无码）。
    /// R09 扩展：401/403 与端点拒绝（404/405/415）不可恢复；解码/请求构建
    /// 协议错误不可恢复（重试同果）；429/5xx/传输断连为暂态（外层退避）。
    /// 410（RV 过期）由 watcher 自行 re-list 恢复，不在此拦。
    fn from_watcher(error: &watcher::Error) -> Option<Self> {
        let source = match error {
            watcher::Error::InitialListFailed(source)
            | watcher::Error::WatchStartFailed(source)
            | watcher::Error::WatchFailed(source) => source,
            watcher::Error::NoResourceVersion => {
                // 协议违规：响应不满足 watch 契约——重试同果，快速失败
                return Some(ObservationError::Fatal {
                    code: None,
                    message: "watch response lacked resourceVersion (protocol violation)".into(),
                });
            }
            watcher::Error::WatchError(status) => {
                if status.code == 401 || status.code == 403 {
                    return Some(ObservationError::Fatal {
                        code: Some(status.code),
                        message: status.message.clone(),
                    });
                }
                // 429/5xx 等 watch 流内错误：暂态，交外层退避
                return None;
            }
        };
        match source {
            kube::Error::Api(ae) if ae.code == 401 || ae.code == 403 => {
                Some(ObservationError::Fatal {
                    code: Some(ae.code),
                    message: ae.message.clone(),
                })
            }
            kube::Error::Api(ae) if matches!(ae.code, 404 | 405 | 415) => {
                Some(ObservationError::Fatal {
                    code: Some(ae.code),
                    message: format!(
                        "resource endpoint rejected the observation ({}): {}",
                        ae.code, ae.message
                    ),
                })
            }
            kube::Error::SerdeError(error) => Some(ObservationError::Fatal {
                code: None,
                message: format!("response decode failed (protocol error): {error}"),
            }),
            kube::Error::BuildRequest(error) => Some(ObservationError::Fatal {
                code: None,
                message: format!("request build failed (protocol error): {error}"),
            }),
            _ => None,
        }
    }
}

/// 观察判定（plan §2 纯判定模型）：由同步分类函数对完整观察对象产出。
#[cfg(feature = "kubernetes")]
#[derive(Debug)]
pub(crate) enum Verdict<T> {
    /// 尚无可判定状态，继续观察。
    Pending,
    /// 观察完成（业务语义由调用方的 T 承载）。
    Complete(T),
    /// 明确拒绝（业务失败分类，携带原因）。
    Rejected(String),
}

/// Pod 就绪等待的纯状态分类器（KR01：保留既有 Ready/Succeeded 成功与
/// Failed/CrashLoopBackOff/ImagePullBackOff 失败语义——逐字对齐
/// `wait_for_pod_ready` 原轮询实现的判定，抽纯函数供 watch 与测试共用）。
#[cfg(feature = "kubernetes")]
pub(crate) fn classify_pod_readiness(pod: &Pod) -> Verdict<()> {
    // phase 终态与 waiting reason 判定（与原实现同序：phase 先、reason 后）
    if let Some(status) = &pod.status {
        match status.phase.as_deref() {
            Some("Failed") => {
                let reason = waiting_reason(pod).unwrap_or_else(|| "unknown".to_string());
                return Verdict::Rejected(format!(
                    "pod entered terminal state: Failed, reason: {reason}"
                ));
            }
            Some("Succeeded") => return Verdict::Complete(()),
            _ => {}
        }
    }
    if let Some(reason) = waiting_reason(pod) {
        match reason.as_str() {
            "CrashLoopBackOff" => {
                return Verdict::Rejected("pod is in CrashLoopBackOff state".into());
            }
            "ImagePullBackOff" => {
                return Verdict::Rejected("pod failed to pull image (ImagePullBackOff)".into());
            }
            _ => {}
        }
    }
    // Ready condition（readinessProbe 真实通过；Running 不足）
    if let Some(status) = &pod.status
        && let Some(conditions) = &status.conditions
        && conditions
            .iter()
            .any(|c| c.type_ == "Ready" && c.status == "True")
    {
        return Verdict::Complete(());
    }
    Verdict::Pending
}

/// 容器 waiting reason 提取（CrashLoopBackOff/ImagePullBackOff 诊断源）。
#[cfg(feature = "kubernetes")]
fn waiting_reason(pod: &Pod) -> Option<String> {
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find_map(|cs| {
            cs.state
                .as_ref()
                .and_then(|state| state.waiting.as_ref())
                .and_then(|waiting| waiting.reason.clone())
        })
}

/// 对单个固定名称 Pod 的有界观察：watcher 流（field_selector 锁名）+
/// 总 deadline + 取消边界；分类回调注入业务判定。
///
/// 返回 `Complete(T)` / `Rejected(reason)` 由分类器给出；错误见
/// [`ObservationError`]。**不做任何写操作**；删除事件（对象消失）在
/// readiness 语义下继续等待（STS 控制器可重建同名 Pod），不判定成功。
#[cfg(feature = "kubernetes")]
pub(crate) async fn await_pod_verdict<T: Clone>(
    api: &kube::Api<Pod>,
    pod_name: &str,
    deadline: Instant,
    cancel: tokio_util::sync::CancellationToken,
    classify: impl Fn(&Pod) -> Verdict<T>,
) -> Result<Verdict<T>, ObservationError> {
    let config = watcher::Config::default()
        .fields(&format!("metadata.name={pod_name}"))
        // 连接轮换上限（非业务总预算——总预算由 deadline 承载）
        .timeout(290);
    let stream = watcher(api.clone(), config);
    tokio::pin!(stream);
    let mut last_transient: Option<String> = None;
    // R09 退避状态：暂态错误后先等再消费下一事件（事件在退避窗内到达则
    // 立即消费——退避约束的是错误后的重新轮询节奏，不丢已到事件）。
    let mut pending_backoff: Option<std::time::Duration> = None;
    let mut next_backoff = BACKOFF_INITIAL;
    loop {
        // 预算先行（KR03：嵌套等待共享同一 deadline，不重置）
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| ObservationError::Deadline {
                last_transient: last_transient.clone(),
            })?;
        if let Some(backoff) = pending_backoff.take() {
            // B07：退避窗口内**只等待**——不 poll watcher（kube-runtime 4.2
            // watcher 恢复发生在 next poll；窗口内 select stream.next() 会
            // 立即触发重连，退避形同虚设）。等待受取消/deadline 约束。
            let wait = std::cmp::min(backoff, remaining);
            tokio::select! {
                () = cancel.cancelled() => return Err(ObservationError::Cancelled),
                _ = tokio::time::sleep(wait) => {}
            }
            continue;
        }
        let event = tokio::select! {
            () = cancel.cancelled() => return Err(ObservationError::Cancelled),
            _ = tokio::time::sleep(remaining) => {
                return Err(ObservationError::Deadline { last_transient });
            }
            event = stream.next() => event,
        };
        let Some(event) = event else {
            // watcher 流意外终止（非正常 EOF——正常 EOF 由 watcher 内部续接）
            return Err(ObservationError::StreamEnded {
                message: "watcher terminated before a verdict".into(),
            });
        };
        match event {
            Ok(watcher::Event::Apply(pod)) | Ok(watcher::Event::InitApply(pod)) => {
                match classify(&pod) {
                    Verdict::Complete(value) => return Ok(Verdict::Complete(value)),
                    Verdict::Rejected(reason) => return Ok(Verdict::Rejected(reason)),
                    Verdict::Pending => {}
                }
            }
            Ok(watcher::Event::Delete(_)) => {
                // 对象消失：readiness 语义下继续等（可被重建）；记暂态原因
                last_transient = Some("observed object deletion; awaiting recreation".into());
            }
            Ok(watcher::Event::Init) | Ok(watcher::Event::InitDone) => {}
            Err(error) => {
                if let Some(fatal) = ObservationError::from_watcher(&error) {
                    return Err(fatal);
                }
                // R09 暂态（传输断连/429/服务端 5xx）：指数退避后再消费
                //（kube-runtime 4.2 watcher 恢复是立即的——不包退避会热循环）；
                // 记录最后原因供超时诊断（KR05：原预算内恢复，不重置 deadline）。
                last_transient = Some(error.to_string());
                pending_backoff = Some(next_backoff);
                next_backoff = std::cmp::min(
                    std::time::Duration::from_secs_f64(next_backoff.as_secs_f64() * 2.0),
                    BACKOFF_MAX,
                );
            }
        }
    }
}

/// Builder 控制的双资源观察事件（批次 B）：STS 与 Pod 两条流共用一个
/// deadline/cancel/退避边界；`PodAbsent` 表达 stop 语义下的 Pod 消失完成
/// （watcher Delete 事件；readiness 单对象观察里它是"等待重建"，这里由
/// 业务判定决定含义——观察层只投递事实）。
#[cfg(feature = "kubernetes")]
pub(crate) enum BuilderWatchEvent<'a> {
    Sts(&'a k8s_openapi::api::apps::v1::StatefulSet),
    Pod(&'a Pod),
    /// Pod 被删除（Delete 事件——仅投递，语义由分类闭包决定）。
    PodAbsent,
}

#[cfg(feature = "kubernetes")]
impl ObservationError {
    fn from_conflict(message: String) -> Self {
        ObservationError::Fatal {
            code: None,
            message,
        }
    }
}

/// STS + Pod 双流观察（批次 B，plan §3）：
/// - 两条 watcher 流（各自 field_selector 锁名）在同一 future 的
///   deadline/cancel 边界内消费；任一流暂态错误共享同一退避（B07 语义）；
/// - 分类闭包产出 `Ok(Verdict)` 或 `Err(String)`（身份/replicas 冲突 →
///   Fatal 快速失败，绝不视为暂态重试）；
/// - 完成候选的**最后 GET 复核**由调用方执行（本函数不提供跨对象事务，
///   也不授权任何写/租约释放——KR06）。
#[cfg(feature = "kubernetes")]
pub(crate) async fn await_builder_verdict<T: Clone>(
    sts_api: &kube::Api<k8s_openapi::api::apps::v1::StatefulSet>,
    sts_name: &str,
    pod_api: &kube::Api<Pod>,
    pod_name: &str,
    deadline: Instant,
    cancel: tokio_util::sync::CancellationToken,
    mut classify: impl FnMut(BuilderWatchEvent<'_>) -> Result<Verdict<T>, String>,
) -> Result<Verdict<T>, ObservationError> {
    let sts_config = watcher::Config::default()
        .fields(&format!("metadata.name={sts_name}"))
        .timeout(290);
    let pod_config = watcher::Config::default()
        .fields(&format!("metadata.name={pod_name}"))
        .timeout(290);
    let sts_stream = watcher(sts_api.clone(), sts_config);
    let pod_stream = watcher(pod_api.clone(), pod_config);
    tokio::pin!(sts_stream);
    tokio::pin!(pod_stream);
    let mut last_transient: Option<String> = None;
    let mut pending_backoff: Option<std::time::Duration> = None;
    let mut next_backoff = BACKOFF_INITIAL;
    // K01：跟踪 pod 流的 Init 周期——初始 LIST（或 410 后重列举）快照为空时
    // watcher 只发 Init+InitDone（没有任何 InitApply/Delete），PodAbsent 完成
    // 候选永远不会产生，已成功停止会被误判到超时。快照结束仍无 pod → 投递
    // PodAbsent（与 Delete 同一候选路径，授权仍由调用方的最后 GET 复核链完成）。
    let mut pod_in_init = false;
    let mut pod_init_seen_apply = false;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| ObservationError::Deadline {
                last_transient: last_transient.clone(),
            })?;
        if let Some(backoff) = pending_backoff.take() {
            // B07 同源纪律：退避窗口只等待，不 poll 任何一条流
            let wait = std::cmp::min(backoff, remaining);
            tokio::select! {
                () = cancel.cancelled() => return Err(ObservationError::Cancelled),
                _ = tokio::time::sleep(wait) => {}
            }
            continue;
        }
        let event = tokio::select! {
            () = cancel.cancelled() => return Err(ObservationError::Cancelled),
            _ = tokio::time::sleep(remaining) => {
                return Err(ObservationError::Deadline { last_transient });
            }
            event = sts_stream.next() => event.map(|inner| inner.map(wrap_sts)),
            event = pod_stream.next() => event.map(|inner| inner.map(|raw| match raw {
                watcher::Event::Init => {
                    pod_in_init = true;
                    pod_init_seen_apply = false;
                    None
                }
                watcher::Event::InitApply(_) => {
                    pod_init_seen_apply = true;
                    wrap_pod(raw)
                }
                watcher::Event::InitDone => {
                    let empty_snapshot = pod_in_init && !pod_init_seen_apply;
                    pod_in_init = false;
                    if empty_snapshot {
                        Some(BuilderWatchEventOwned::PodAbsent)
                    } else {
                        None
                    }
                }
                other => wrap_pod(other),
            })),
        };
        let Some(event) = event else {
            return Err(ObservationError::StreamEnded {
                message: "builder workload watcher terminated before a verdict".into(),
            });
        };
        match event {
            Ok(Some(wrapped)) => match classify(wrapped.as_ref()) {
                Ok(Verdict::Complete(value)) => return Ok(Verdict::Complete(value)),
                Ok(Verdict::Rejected(reason)) => return Ok(Verdict::Rejected(reason)),
                Ok(Verdict::Pending) => {}
                Err(conflict) => return Err(ObservationError::from_conflict(conflict)),
            },
            Ok(None) => {}
            Err(error) => {
                if let Some(fatal) = ObservationError::from_watcher(&error) {
                    return Err(fatal);
                }
                last_transient = Some(error.to_string());
                pending_backoff = Some(next_backoff);
                next_backoff = std::cmp::min(
                    std::time::Duration::from_secs_f64(next_backoff.as_secs_f64() * 2.0),
                    BACKOFF_MAX,
                );
            }
        }
    }
}

#[cfg(feature = "kubernetes")]
fn wrap_sts(
    event: watcher::Event<k8s_openapi::api::apps::v1::StatefulSet>,
) -> Option<BuilderWatchEventOwned> {
    match event {
        watcher::Event::Apply(obj) | watcher::Event::InitApply(obj) => {
            Some(BuilderWatchEventOwned::Sts(Box::new(obj)))
        }
        watcher::Event::Delete(_) => None,
        watcher::Event::Init | watcher::Event::InitDone => None,
    }
}

#[cfg(feature = "kubernetes")]
fn wrap_pod(event: watcher::Event<Pod>) -> Option<BuilderWatchEventOwned> {
    match event {
        watcher::Event::Apply(obj) | watcher::Event::InitApply(obj) => {
            Some(BuilderWatchEventOwned::Pod(Box::new(obj)))
        }
        watcher::Event::Delete(_) => Some(BuilderWatchEventOwned::PodAbsent),
        watcher::Event::Init | watcher::Event::InitDone => None,
    }
}

/// 分类闭包入参的 owned 形态（select 分支需要拥有对象再借出）。
#[cfg(feature = "kubernetes")]
enum BuilderWatchEventOwned {
    Sts(Box<k8s_openapi::api::apps::v1::StatefulSet>),
    Pod(Box<Pod>),
    PodAbsent,
}

#[cfg(feature = "kubernetes")]
impl BuilderWatchEventOwned {
    fn as_ref(&self) -> BuilderWatchEvent<'_> {
        match self {
            BuilderWatchEventOwned::Sts(obj) => BuilderWatchEvent::Sts(obj.as_ref()),
            BuilderWatchEventOwned::Pod(obj) => BuilderWatchEvent::Pod(obj.as_ref()),
            BuilderWatchEventOwned::PodAbsent => BuilderWatchEvent::PodAbsent,
        }
    }
}

#[cfg(all(test, feature = "kubernetes"))]
#[path = "k8s_observation_tests.rs"]
mod observation_contract_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(phase: Option<&str>, ready: Option<bool>, reason: Option<&str>) -> Pod {
        let conditions = ready.map(|ready| {
            vec![k8s_openapi::api::core::v1::PodCondition {
                type_: "Ready".to_string(),
                status: if ready { "True".into() } else { "False".into() },
                ..Default::default()
            }]
        });
        let waiting = reason.map(|reason| k8s_openapi::api::core::v1::ContainerStateWaiting {
            reason: Some(reason.to_string()),
            ..Default::default()
        });
        let state = waiting.map(|waiting| k8s_openapi::api::core::v1::ContainerState {
            waiting: Some(waiting),
            ..Default::default()
        });
        let container_statuses = state.map(|state| {
            vec![k8s_openapi::api::core::v1::ContainerStatus {
                state: Some(state),
                ..Default::default()
            }]
        });
        Pod {
            status: Some(k8s_openapi::api::core::v1::PodStatus {
                phase: phase.map(str::to_string),
                conditions,
                container_statuses,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// KR01 语义锁：分类器逐态断言（成功/失败/继续等待三档，对齐原轮询判据）。
    #[test]
    fn classify_pod_readiness_matches_legacy_semantics() {
        // Ready=True → 成功
        assert!(matches!(
            classify_pod_readiness(&pod(Some("Running"), Some(true), None)),
            Verdict::Complete(())
        ));
        // Succeeded → 成功（run-to-completion 语义保留）
        assert!(matches!(
            classify_pod_readiness(&pod(Some("Succeeded"), None, None)),
            Verdict::Complete(())
        ));
        // Failed → 拒绝
        assert!(matches!(
            classify_pod_readiness(&pod(Some("Failed"), None, None)),
            Verdict::Rejected(_)
        ));
        // CrashLoopBackOff / ImagePullBackOff → 拒绝
        assert!(matches!(
            classify_pod_readiness(&pod(Some("Running"), None, Some("CrashLoopBackOff"))),
            Verdict::Rejected(_)
        ));
        assert!(matches!(
            classify_pod_readiness(&pod(Some("Pending"), None, Some("ImagePullBackOff"))),
            Verdict::Rejected(_)
        ));
        // Running 未 Ready / Pending / 其他 waiting reason → 继续等待
        assert!(matches!(
            classify_pod_readiness(&pod(Some("Running"), Some(false), None)),
            Verdict::Pending
        ));
        assert!(matches!(
            classify_pod_readiness(&pod(Some("Pending"), None, Some("ContainerCreating"))),
            Verdict::Pending
        ));
        assert!(matches!(
            classify_pod_readiness(&pod(None, None, None)),
            Verdict::Pending
        ));
    }
}
