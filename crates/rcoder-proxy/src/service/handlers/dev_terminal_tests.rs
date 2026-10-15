//! F1 dev 定位反例测试（T2 必测）：fail-closed、权威地址优先、查询失败
//! 不借 ensure 绕过、类型化失败映射。
//!
//! 历史缺陷对照（修复前可暴露错误的反例）：
//! - 旧实现在 dev 依赖未注入的装配窗口以"注册表候选 + TCP 探测"放行——
//!   候选 IP 可被其他应用的 builder 复用（端口探测通过≠身份正确），
//!   造成跨应用误转发；本测试组断言未注入即 503，且地址唯一来源是
//!   权威 `locate_dev_builder`。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use shared_types::{ContainerBasicInfo, DevBuilderInstance, DevEnsureError, UserappDevEnsure};

use super::dev_terminal::find_dev_container;

type Slot = arc_swap::ArcSwapOption<Arc<dyn UserappDevEnsure>>;

fn slot_with(mock: Arc<MockEnsure>) -> Slot {
    Slot::from(Some(Arc::new(mock as Arc<dyn UserappDevEnsure>)))
}

/// 逐次出队的 mock：locate/ensure 各自的返回序列 + 调用计数。
struct MockEnsure {
    locate: Mutex<VecDeque<Result<Option<DevBuilderInstance>, DevEnsureError>>>,
    ensure: Mutex<VecDeque<Result<ContainerBasicInfo, DevEnsureError>>>,
    locate_calls: std::sync::atomic::AtomicUsize,
    ensure_calls: std::sync::atomic::AtomicUsize,
}

impl MockEnsure {
    fn new(
        locate: Vec<Result<Option<DevBuilderInstance>, DevEnsureError>>,
        ensure: Vec<Result<ContainerBasicInfo, DevEnsureError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            locate: Mutex::new(locate.into()),
            ensure: Mutex::new(ensure.into()),
            locate_calls: std::sync::atomic::AtomicUsize::new(0),
            ensure_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn locate_calls(&self) -> usize {
        self.locate_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn ensure_calls(&self) -> usize {
        self.ensure_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl UserappDevEnsure for MockEnsure {
    async fn locate_dev_builder(
        &self,
        app_id: &str,
    ) -> Result<Option<DevBuilderInstance>, DevEnsureError> {
        self.locate_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.locate
            .lock()
            .expect("locate queue lock")
            .pop_front()
            .unwrap_or_else(|| {
                Err(DevEnsureError::ObserveFailed {
                    app_id: app_id.to_string(),
                    detail: "mock exhausted".into(),
                })
            })
    }

    async fn ensure_dev_container(
        &self,
        _app_id: &str,
    ) -> Result<ContainerBasicInfo, DevEnsureError> {
        self.ensure_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.ensure
            .lock()
            .expect("ensure queue lock")
            .pop_front()
            .expect("ensure queue exhausted")
    }
}

fn instance(address: &str) -> Option<DevBuilderInstance> {
    Some(DevBuilderInstance {
        address: address.into(),
        container_id: "pod-uid-1".into(),
    })
}

fn ensured_info(ip: &str) -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: "pod-uid-2".into(),
        container_name: "rcoder-app-builder-1-0".into(),
        container_ip: ip.into(),
        internal_port: 9080,
        external_port: 0,
        project_id: "1".into(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: String::new(),
        workload_uid: None,
    }
}

fn status_of(error: &pingora_core::Error) -> Option<u16> {
    match error.etype() {
        pingora_core::ErrorType::HTTPStatus(code) => Some(*code),
        _ => None,
    }
}

fn absent(app_id: &str) -> DevEnsureError {
    DevEnsureError::BuilderAbsent {
        app_id: app_id.into(),
    }
}

/// 反例（V2-01）：依赖未注入（装配窗口）必须 fail-closed——503 暂不可用，
/// 不得回退注册表候选/端口探测（旧实现在此窗口以 `None => true` 放行）。
#[tokio::test]
async fn not_injected_fails_closed_with_503() {
    let slot = Slot::from(None);
    let error = find_dev_container(&slot, "238")
        .await
        .expect_err("must fail");
    assert_eq!(status_of(&error), Some(503));
}

/// 权威存在：直接返回权威地址，不触发 ensure（无副作用）。
#[tokio::test]
async fn authoritative_address_returned_without_ensure() {
    let mock = MockEnsure::new(vec![Ok(instance("10.42.1.142"))], vec![]);
    let slot = slot_with(mock.clone());
    let ip = find_dev_container(&slot, "238").await.expect("address");
    assert_eq!(ip, "10.42.1.142");
    assert_eq!(mock.ensure_calls(), 0);
}

/// 反例（RV-10）：权威地址与注册表残影/复用 IP 不一致时以权威值为准——
/// 注册表不再参与（旧实现可返回被 B 复用的 A 旧 IP）。
#[tokio::test]
async fn authoritative_address_supersedes_any_stale_candidate() {
    // 权威定位恒返回 A 的当前地址；即便"旧 IP 已被他应用占用且端口可连"，
    // 也无从进入本路径（无注册表入参）——回归守卫。
    let mock = MockEnsure::new(vec![Ok(instance("10.42.1.142"))], vec![]);
    let slot = slot_with(mock.clone());
    let ip = find_dev_container(&slot, "238").await.expect("address");
    assert_eq!(ip, "10.42.1.142");
    assert_eq!(mock.locate_calls(), 1);
}

/// 权威查询失败：重试一次后诚实失败（503），绝不借 ensure 绕过保护。
#[tokio::test]
async fn observe_failure_retries_once_then_fails_without_ensure() {
    let mock = MockEnsure::new(vec![Err(observe_failed()), Err(observe_failed())], vec![]);
    let slot = slot_with(mock.clone());
    let error = find_dev_container(&slot, "238")
        .await
        .expect_err("must fail");
    assert_eq!(status_of(&error), Some(503));
    assert_eq!(mock.locate_calls(), 2, "exactly one retry");
    assert_eq!(mock.ensure_calls(), 0, "must not bypass via ensure");
}

/// 查询失败后重试成功：返回地址（瞬时观测抖动可自愈）。
#[tokio::test]
async fn observe_failure_retry_may_recover() {
    let mock = MockEnsure::new(
        vec![Err(observe_failed()), Ok(instance("10.42.1.142"))],
        vec![],
    );
    let slot = slot_with(mock);
    let ip = find_dev_container(&slot, "238").await.expect("address");
    assert_eq!(ip, "10.42.1.142");
}

/// 仅 ObserveFailed 触发重试：NotReady/围栏类失败立即映射，不空转重试。
#[tokio::test]
async fn not_ready_maps_immediately_without_retry_or_ensure() {
    let not_ready = DevEnsureError::NotReady {
        app_id: "238".into(),
        detail: "pending".into(),
    };
    let mock = MockEnsure::new(vec![Err(not_ready)], vec![]);
    let slot = slot_with(mock.clone());
    let error = find_dev_container(&slot, "238")
        .await
        .expect_err("must fail");
    assert_eq!(status_of(&error), Some(503));
    assert_eq!(mock.locate_calls(), 1, "no retry for NotReady");
    assert_eq!(mock.ensure_calls(), 0);
}

/// 确认不存在 → 懒启动 ensure（使用语义）成功返回地址。
#[tokio::test]
async fn absent_falls_through_to_ensure() {
    let mock = MockEnsure::new(vec![Ok(None)], vec![Ok(ensured_info("10.42.1.180"))]);
    let slot = slot_with(mock.clone());
    let ip = find_dev_container(&slot, "238").await.expect("address");
    assert_eq!(ip, "10.42.1.180");
    assert_eq!(mock.ensure_calls(), 1);
}

/// ensure 类型化映射：BuilderAbsent → 404 指引（真实语义不吞）。
#[tokio::test]
async fn ensure_absent_maps_to_404() {
    let mock = MockEnsure::new(vec![Ok(None)], vec![Err(absent("238"))]);
    let slot = slot_with(mock);
    let error = find_dev_container(&slot, "238")
        .await
        .expect_err("must fail");
    assert_eq!(status_of(&error), Some(404));
}

/// ensure 类型化映射：围栏（在途操作）→ 503 带原因（旧实现统一吞成 404）。
#[tokio::test]
async fn ensure_in_flight_maps_to_503() {
    let in_flight = DevEnsureError::OperationInFlight {
        app_id: "238".into(),
        detail: "operation 3086a022 (kind=RestartBuilder, state=Running)".into(),
    };
    let mock = MockEnsure::new(vec![Ok(None)], vec![Err(in_flight)]);
    let slot = slot_with(mock);
    let error = find_dev_container(&slot, "238")
        .await
        .expect_err("must fail");
    assert_eq!(status_of(&error), Some(503));
}

/// 非法 app_id → 400（不触发任何回调）。
#[tokio::test]
async fn invalid_app_id_rejected_before_callbacks() {
    let mock = MockEnsure::new(vec![], vec![]);
    let slot = slot_with(mock.clone());
    let error = find_dev_container(&slot, "../escape")
        .await
        .expect_err("must fail");
    assert_eq!(status_of(&error), Some(400));
    assert_eq!(mock.locate_calls(), 0);
    assert_eq!(mock.ensure_calls(), 0);
}

fn observe_failed() -> DevEnsureError {
    DevEnsureError::ObserveFailed {
        app_id: "238".into(),
        detail: "runtime probe timeout".into(),
    }
}
