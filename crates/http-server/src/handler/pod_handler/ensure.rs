use super::ensure_flow;
use super::helpers::*;
use super::*;
use axum::response::IntoResponse;

/// 启动/确保容器存在（幂等）
///
/// 根据 user_id 和 project_id 启动或获取已存在的容器。
/// 仅启动容器，不启动 Agent 服务。
#[utoipa::path(
    post,
    path = "/computer/pod/ensure",
    request_body(content = EnsurePodRequest, description = "启动容器请求"),
    responses(
        (status = 200, description = "容器已存在或普通容器已同步创建；UserApp 只表示计算资源已请求运行，不代表业务 Ready", body = HttpResult<EnsurePodResponse>),
        (status = 202, description = "已有 UserApp 计算资源需要物理启动；返回持久操作 ID 和查询地址，不等待业务服务 Ready。受理后有界等待（≤15s）物理工作负载对象出现：creation_observed=true 表示创建已观察；false 表示仍在创建中（已受理，非失败），凭 operation_id 查询/重试", body = HttpResult<crate::userapp_builder::compute_control::ComputeOperationView>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_ensure",
    summary = "启动/确保容器存在（幂等）",
    description = "根据 user_id 和 project_id 启动或获取已存在的容器，仅启动容器不启动 Agent 服务。UserApp 已有控制器但计算资源停止时走高优先级物理控制操作，Kubernetes 启动时将 Pod 模板更新为当前平台镜像并保留 PVC；HTTP 202 返回 operation_id，业务 Ready 单独查询。已运行的容器直接返回，不因 ensure 升级镜像。Stop/Restart 正在执行时快失败，不内部排队。"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_ensure(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<EnsurePodRequest>,
) -> Result<axum::response::Response, AppError> {
    let locale = shared_types::current_request_locale();

    // 0. userApp 分派（app_id 存在即短路 agent 流程）
    match parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    ) {
        Ok(AppTarget::NotApp) => {}
        Ok(AppTarget::Dev(app_id)) => {
            return ensure_userapp_dev(&state, app_id, request.user_id.as_str()).await;
        }
        Ok(AppTarget::Prod(app_id)) => {
            return ensure_userapp_prod(&state, locale, app_id, request.user_id.as_str()).await;
        }
        Err(e) => {
            error!("[POD_ENSURE] invalid app target: {}", e);
            return Ok(invalid_app_target_response::<EnsurePodResponse>(locale, &e).into_response());
        }
    }

    ensure_ordinary_pod(&state, &request, locale)
        .await
        .map(IntoResponse::into_response)
}

pub(crate) async fn ensure_ordinary_pod(
    state: &Arc<AppState>,
    request: &EnsurePodRequest,
    locale: &'static str,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    // 1. 验证参数
    if let Some(resp) = validate_pod_ids::<EnsurePodResponse>(
        &request.user_id,
        &request.project_id,
        locale,
        "POD_ENSURE",
    ) {
        return Ok(resp);
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits
        && let Err(e) = validate_resource_limits(limits)
    {
        error!("[POD_ENSURE] resources update failed: {}", e);
        return Ok(HttpResult::<EnsurePodResponse>::error_with_message(
            shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
            locale,
            &e,
        ));
    }

    // 1.2 解析 service_type
    let service_type = match parse_service_type(request.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_ENSURE] invalid service_type: {}", e);
            return Ok(HttpResult::<EnsurePodResponse>::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    };

    // 1.3 根据 service_type 确定容器标识符
    let container_identifier = container_identifier_for_service(
        &service_type,
        &request.user_id,
        &request.project_id,
        request.pod_id.as_deref(),
    )?;

    info!(
        "[POD_ENSURE] Ensuring container exists: user_id={}, project_id={}, service_type={}, container_identifier={}",
        request.user_id, request.project_id, service_type, container_identifier
    );

    // === 并发保护：检查是否有其他请求正在创建同一用户的容器 ===
    // 使用原子标记（DashMap）避免并发请求互相干扰，无死锁风险
    if let Some(response) = ensure_flow::wait_for_concurrent_creation(
        state,
        request,
        &service_type,
        &container_identifier,
    )
    .await
    {
        return response;
    }

    // 2. 🔍 实时查询 runtime 检查容器是否存在（不依赖缓存），未运行的旧容器
    // 连同 SSE/gRPC 连接一并清理
    let need_create =
        ensure_flow::resolve_need_create(state, &container_identifier, &service_type).await?;

    // 3. 获取或创建容器（带重试机制 + 标记）
    let (container_info, created) = if need_create {
        let info =
            ensure_flow::create_with_retry(state, request, &service_type, &container_identifier)
                .await?;
        (info, true)
    } else {
        ensure_flow::get_existing_with_sync(state, request, &service_type, &container_identifier)
            .await?
    };

    // 4/5/6. 注册 VNC backend + 更新存储记录 + 构建响应
    let message = if created {
        "Container created successfully, can access virtual desktop via VNC (Agent service not started)".to_string()
    } else {
        "Container already exists, can access virtual desktop via VNC directly".to_string()
    };
    persist_and_respond(
        state,
        request,
        &service_type,
        &container_info,
        created,
        message,
    )
}

// ============================================================================
// userApp 分派实现（app_id/app_stage）
// ============================================================================

/// ensure 的 userApp dev 分支：探活自愈版 ensure（注册脏值/死容器重建），
/// created 由探活版判定（复用=false / 重建或新建=true）。
async fn ensure_userapp_dev(
    state: &Arc<AppState>,
    app_id: String,
    _user_id: &str,
) -> Result<axum::response::Response, AppError> {
    match crate::userapp_builder::inspect_builder_compute(state, &app_id)
        .await
        .map_err(|error| crate::userapp_builder::control_error(&error))?
    {
        crate::userapp_builder::BuilderComputeState::Running(info) => {
            return Ok(HttpResult::success(EnsurePodResponse {
                created: false,
                container_info: PodContainerInfo {
                    container_id: info.container_id,
                    status: info.status,
                },
                message: "UserApp dev 计算容器正在运行；业务服务状态请单独查询".into(),
            })
            .into_response());
        }
        crate::userapp_builder::BuilderComputeState::StoppedRetained => {
            let operation = crate::userapp_builder::compute_control::submit_physical_start(
                state,
                app_id,
                shared_types::UserAppOperationScope::Dev,
                shared_types::UserAppControlRequest {
                    lifecycle_id: None,
                    request_id: None,
                },
            )
            .await
            .map_err(|error| crate::userapp_builder::control_error(&error))?;
            let operation_id = operation.operation_id.clone();
            return Ok((
                axum::http::StatusCode::ACCEPTED,
                HttpResult::success(operation).with_operation_id(operation_id),
            )
                .into_response());
        }
        crate::userapp_builder::BuilderComputeState::StoppedRemoved => {
            // Docker builders use AutoRemove. Their physical object is
            // gone, so there is no captured UID to restart in place. A
            // completed Stop has already drained the old business executor;
            // explicit ensure may create a new container on the same mounts.
            let info = crate::userapp_builder::ensure_userapp_builder(state, &app_id)
                .await
                .map_err(|error| crate::userapp_builder::control_error(&error))?;
            return Ok(HttpResult::success(EnsurePodResponse {
                created: true,
                container_info: PodContainerInfo {
                    container_id: info.container_id,
                    status: info.status,
                },
                message: "UserApp dev 计算容器已使用原工作区重建".into(),
            })
            .into_response());
        }
        crate::userapp_builder::BuilderComputeState::Missing => {}
    }
    let (info, created) = crate::userapp_builder::ensure_userapp_builder_probed(state, &app_id)
        .await
        .map_err(|e| {
            error!("[POD_ENSURE] ensure userapp dev container failed: app_id={app_id}: {e:#}");
            crate::userapp_builder::control_error(&e)
        })?;
    info!(
        "[POD_ENSURE] userapp dev container ready: app_id={app_id}, container={}, ip={}",
        info.container_name, info.container_ip
    );
    Ok(HttpResult::success(EnsurePodResponse {
        created,
        container_info: PodContainerInfo {
            container_id: info.container_id.clone(),
            status: info.status.clone(),
        },
        message: "Userapp dev 容器已就绪（虚拟终端/文件服务经反向代理访问）".to_string(),
    })
    .into_response())
}

/// Existing prod compute is an explicit physical control, independent of
/// application readiness and the ordinary deployment slot. Missing compute
/// still follows the initial empty-container creation path.
async fn ensure_userapp_prod(
    state: &Arc<AppState>,
    locale: &str,
    app_id: String,
    _user_id: &str,
) -> Result<axum::response::Response, AppError> {
    let runtime = match state.app_service.get_app(&app_id).await {
        Ok(runtime) => runtime,
        Err(app_manager::AppOperationError::NotFound(_)) => {
            return ensure_userapp_prod_created(state, locale, app_id)
                .await
                .map(IntoResponse::into_response);
        }
        Err(e) if retained_without_compute(&e) => {
            // 保留的 Active 记录 + 物理工作负载缺失：fetch_runtime_status_or_err
            // 的显式分类（防瞬时 API 故障被误判为应用不存在→404→Java 误重建）。
            // 按本函数文档语义（Missing compute still follows the initial
            // empty-container creation path）走初始空容器创建路径；其余查询
            // 故障仍落入下方失败分支，不触发创建。
            info!(
                "[POD_ENSURE] retained app without physical workload, following creation path: app_id={app_id}"
            );
            return ensure_userapp_prod_created(state, locale, app_id)
                .await
                .map(IntoResponse::into_response);
        }
        Err(e) => {
            // API Server 不可达/RBAC 拒绝等查询故障：语义=查询失败而非应用不存在，
            // 与唤醒路径的 Timeout/Failed 同码（Backend），不触发创建
            error!("[POD_ENSURE] query userapp prod app failed: app_id={app_id}: {e:#}");
            return Ok(HttpResult::<EnsurePodResponse>::error_with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                locale,
                &format!("query userapp prod app failed: {e:#}"),
            )
            .into_response());
        }
    };
    if runtime.replicas > 0 && runtime.phase != "Error" {
        // The physical controller already asks for a Pod. Do not wait on
        // application readiness or an unrelated deploy, but do not race an
        // active Stop/Restart or report a stopped intent as running.
        state
            .userapp_store
            .check_compute_access(&app_id, shared_types::UserAppOperationScope::Prod, false)
            .await
            .map_err(|error| {
                let error: anyhow::Error = error.into();
                crate::userapp_builder::control_error(&error)
            })?;
        return Ok(HttpResult::success(EnsurePodResponse {
            created: false,
            container_info: PodContainerInfo {
                container_id: app_id,
                status: runtime.phase,
            },
            message: "UserApp prod 计算资源已请求运行；业务服务状态请单独查询".into(),
        })
        .into_response());
    }
    let operation = crate::userapp_builder::compute_control::submit_physical_start(
        state,
        app_id.clone(),
        shared_types::UserAppOperationScope::Prod,
        shared_types::UserAppControlRequest {
            lifecycle_id: None,
            request_id: None,
        },
    )
    .await
    .map_err(|error| crate::userapp_builder::control_error(&error))?;
    let operation_id = operation.operation_id.clone();
    // 受理后有界等待"物理创建已观察"（工作负载对象存在即算，不等就绪/健康）：
    // 消除"受理≠开始"的观察空洞——成功返回时物理对象已在，调用方后续查询
    // 不会再命中"记录在但查无负载"的瞬时窗口。预算耗尽不虚报失败（操作
    // 仍在进行），返回受理成功并注明未观察；操作快速终态失败则提前返回。
    let (mut operation, creation_observed) =
        wait_creation_observed(state, app_id, operation, CREATE_OBSERVE_BUDGET).await;
    operation.creation_observed = Some(creation_observed);
    Ok((
        axum::http::StatusCode::ACCEPTED,
        HttpResult::success(operation).with_operation_id(operation_id),
    )
        .into_response())
}

/// pod/ensure 物理创建观察预算（工作负载对象出现即返回；不等就绪）。
const CREATE_OBSERVE_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);
const CREATE_OBSERVE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// 保留记录 + 物理工作负载缺失的显式分类（Diagnostic code=CONTAINER_NOT_FOUND）。
/// 仅这一码代表"确证无负载"；其余查询故障语义仍是失败，不触发创建。
pub(crate) fn retained_without_compute(error: &app_manager::AppOperationError) -> bool {
    matches!(
        error,
        app_manager::AppOperationError::Diagnostic(detail)
            if detail.code.as_ref() == shared_types::ERR_CONTAINER_NOT_FOUND
    )
}

/// 受理后有界等待物理工作负载对象出现（`get_app` Ok 即算）。同时刷新操作
/// 状态：failed/superseded 快速终态提前返回（调用方凭 view.state 判断）；
/// 预算耗尽返回 (最新 view, false)——已受理的操作仍在进行，不构成失败。
async fn wait_creation_observed(
    state: &Arc<AppState>,
    app_id: String,
    operation: crate::userapp_builder::compute_control::ComputeOperationView,
    budget: std::time::Duration,
) -> (
    crate::userapp_builder::compute_control::ComputeOperationView,
    bool,
) {
    let app_probe_id = app_id.clone();
    let control_probe_id = app_id.clone();
    observe_creation_with(
        operation,
        budget,
        app_id,
        move || {
            let app_id = app_probe_id.clone();
            async move { state.app_service.get_app(&app_id).await.is_ok() }
        },
        move |operation_id: String| {
            let app_id = control_probe_id.clone();
            async move {
                state
                    .userapp_store
                    .get_compute_control(&app_id, &operation_id)
                    .await
                    .ok()
                    .flatten()
            }
        },
    )
    .await
}

/// 观察循环核心（与状态源解耦——R4 边界测试注入受控慢读取）。
/// R4：单轮读取钳制到共享 deadline——预算是硬边界，慢读越界（含迟到的
/// 成功读取）一律按"已受理但观察未完成"收场，不得越过预算后报告已观察。
async fn observe_creation_with<AppFut, CtlFut>(
    mut operation: crate::userapp_builder::compute_control::ComputeOperationView,
    budget: std::time::Duration,
    app_id: String,
    get_app: impl Fn() -> AppFut,
    get_control: impl Fn(String) -> CtlFut,
) -> (
    crate::userapp_builder::compute_control::ComputeOperationView,
    bool,
)
where
    AppFut: Future<Output = bool>,
    CtlFut: Future<Output = Option<shared_types::ComputeControlRecord>>,
{
    use crate::userapp_builder::compute_control::ComputeOperationView;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        // timeout_at 先轮询内部 future，立即 Ready 时即便 deadline 已过仍可
        // 返回 Ok。开始读取前及成功读取后都检查，避免把迟到值视作预算内观察。
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                app_id,
                operation_id = %operation.operation_id,
                "creation observation budget exhausted before reading the app record"
            );
            return (operation, false);
        }
        // 单轮 get_app 钳制：超剩余预算 → 观察未完成（保留原 view 返回）。
        let app_ok = match tokio::time::timeout_at(deadline, get_app()).await {
            Ok(result) if tokio::time::Instant::now() < deadline => result,
            Ok(_) | Err(_) => {
                tracing::warn!(
                    app_id,
                    operation_id = %operation.operation_id,
                    "creation observation budget exhausted while reading the app record; accepted operation remains in flight"
                );
                return (operation, false);
            }
        };
        if app_ok {
            let record =
                tokio::time::timeout_at(deadline, get_control(operation.operation_id.clone()))
                    .await;
            match record {
                Ok(Some(record)) if tokio::time::Instant::now() < deadline => {
                    operation = ComputeOperationView::from(record);
                }
                Ok(None) if tokio::time::Instant::now() < deadline => {}
                // 预算耗尽于操作视图刷新：工作负载对象已观察到——创建
                // 可观察成立，返回当前视图（留痕：观察到的视图可能滞后）。
                Ok(_) | Err(_) => {
                    tracing::warn!(
                        app_id,
                        operation_id = %operation.operation_id,
                        "creation observation budget exhausted while refreshing the operation view; returning the last confirmed view"
                    );
                    return (operation, true);
                }
            }
            return (operation, true);
        }
        let record =
            tokio::time::timeout_at(deadline, get_control(operation.operation_id.clone())).await;
        match record {
            Ok(Some(record)) if tokio::time::Instant::now() < deadline => {
                let view = ComputeOperationView::from(record);
                let terminal_failure = matches!(
                    view.state,
                    shared_types::ComputeControlState::Failed
                        | shared_types::ComputeControlState::Superseded
                );
                operation = view;
                if terminal_failure {
                    return (operation, false);
                }
            }
            Ok(None) if tokio::time::Instant::now() < deadline => {}
            // 预算耗尽于状态刷新：观察未完成（留痕：已受理但对象未观察到）。
            Ok(_) | Err(_) => {
                tracing::warn!(
                    app_id,
                    operation_id = %operation.operation_id,
                    "creation observation budget exhausted while reading runtime/store; accepted operation remains in flight"
                );
                return (operation, false);
            }
        }
        if tokio::time::Instant::now() + CREATE_OBSERVE_INTERVAL >= deadline {
            return (operation, false);
        }
        tokio::time::sleep(CREATE_OBSERVE_INTERVAL).await;
    }
}

/// prod 空容器预创建子分支：应用共享（无 owner 解析），复用 start 无 url
/// 三态链（不存在 → 创建空容器；deploy_controlled 的锁/幂等编排全程兜底，
/// 并发 ensure 由其 operation 锁收敛）。
async fn ensure_userapp_prod_created(
    state: &Arc<AppState>,
    locale: &str,
    app_id: String,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    let request = app_manager::models::StartAppRequest::default();
    match state.app_service.start_app_enhanced(&app_id, request).await {
        Ok(result) => {
            info!(
                "[POD_ENSURE] userapp prod empty container created: app_id={app_id}, status={:?}, phase={:?}",
                result.runtime.status, result.runtime.phase
            );
            Ok(HttpResult::success(EnsurePodResponse {
                created: true,
                container_info: PodContainerInfo {
                    container_id: app_id.clone(),
                    status: format!("{:?}", result.runtime.status),
                },
                message: "Userapp 生产容器已预创建（应用未部署，PG/终端服务可用）".to_string(),
            }))
        }
        Err(e) => {
            error!(
                "[POD_ENSURE] create userapp prod empty container failed: app_id={app_id}: {e:#}"
            );
            Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                locale,
                &format!("create userapp prod container failed: {e:#}"),
            ))
        }
    }
}

#[cfg(test)]
mod creation_budget_tests {
    use super::*;
    use shared_types::{
        ComputeControlAction, ComputeControlRecord, ComputeControlState, UserAppOperationScope,
    };

    fn record(state: ComputeControlState, operation_id: &str) -> ComputeControlRecord {
        ComputeControlRecord {
            created_at: chrono::Utc::now(),
            app_id: "app-budget".to_string(),
            lifecycle_id: "lifecycle".to_string(),
            request_id: "request".to_string(),
            scope: UserAppOperationScope::Prod,
            operation_id: operation_id.to_string(),
            request_fingerprint: "fingerprint".to_string(),
            generation: 1,
            revision: 1,
            action: ComputeControlAction::Restart,
            state,
            executor_id: None,
            stage: "observed".to_string(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
            lease: None,
            interrupted_operations: Vec::new(),
        }
    }

    fn view(
        state: ComputeControlState,
    ) -> crate::userapp_builder::compute_control::ComputeOperationView {
        crate::userapp_builder::compute_control::ComputeOperationView::from(record(
            state,
            "op-budget",
        ))
    }

    #[tokio::test]
    async fn zero_budget_never_polls_or_reports_workload() {
        let app_reads = std::cell::Cell::new(0);
        let (result, observed) = observe_creation_with(
            view(ComputeControlState::Running),
            std::time::Duration::ZERO,
            "app-budget".to_string(),
            || {
                app_reads.set(app_reads.get() + 1);
                std::future::ready(true)
            },
            |_operation_id: String| async { None },
        )
        .await;
        assert!(!observed, "耗尽预算不得宣称已观察");
        assert_eq!(app_reads.get(), 0, "耗尽预算不得开始新的观察");
        assert_eq!(result.operation_id, "op-budget");
        assert_eq!(result.state, ComputeControlState::Running);
    }

    // timeout_at 会先轮询内部 future；不 yield 的读取可在 deadline 后返回
    // Ready，因此必须在返回成功值后再次核验绝对 deadline。
    #[tokio::test]
    async fn late_ready_app_read_never_reports_observed() {
        let (_result, observed) = observe_creation_with(
            view(ComputeControlState::Running),
            std::time::Duration::from_millis(20),
            "app-budget".to_string(),
            || async {
                std::thread::sleep(std::time::Duration::from_millis(80));
                true
            },
            |_operation_id: String| async { None },
        )
        .await;
        assert!(!observed, "未让出执行权的迟到成功同样不能越过预算");
    }

    #[tokio::test]
    async fn late_ready_control_record_does_not_replace_confirmed_view() {
        let (result, observed) = observe_creation_with(
            view(ComputeControlState::Running),
            std::time::Duration::from_millis(20),
            "app-budget".to_string(),
            || async { true },
            |_operation_id: String| async {
                std::thread::sleep(std::time::Duration::from_millis(80));
                let mut late = record(ComputeControlState::Failed, "op-budget");
                late.revision = 2;
                Some(late)
            },
        )
        .await;
        assert!(observed, "工作负载在预算内已观察到");
        assert_eq!(result.state, ComputeControlState::Running);
        assert_eq!(result.revision, 1, "预算外读取不能替换最后确认的视图");
    }

    /// R4 边界反例（审查数字复现）：20ms 预算、80ms 慢 runtime 读取——
    /// 观察必须在预算内结束且不报告已观察（迟到的成功读取不算数）。
    #[tokio::test]
    async fn slow_app_read_never_crosses_the_budget() {
        let started = Instant::now();
        let (_result, observed) = observe_creation_with(
            view(ComputeControlState::Running),
            std::time::Duration::from_millis(20),
            "app-budget".to_string(),
            || async {
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                true // 迟到的成功
            },
            |_operation_id: String| async { None },
        )
        .await;
        assert!(!observed, "迟到成功不得越过预算报告已观察");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(60),
            "观察应在预算附近结束（实测 {:?}）",
            started.elapsed()
        );
    }

    /// 慢 store 读取同受预算钳制：对象已观察到（get_app Ok）时，控制记录
    /// 刷新被钳制——返回已观察 + 原视图，不越过预算。
    #[tokio::test]
    async fn slow_control_refresh_is_clamped_to_budget() {
        let started = Instant::now();
        let (_result, observed) = observe_creation_with(
            view(ComputeControlState::Running),
            std::time::Duration::from_millis(20),
            "app-budget".to_string(),
            || async { true },
            |_operation_id: String| async {
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                None
            },
        )
        .await;
        assert!(observed, "get_app 已 Ok：创建可观察成立");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(60),
            "刷新钳制应在预算附近结束（实测 {:?}）",
            started.elapsed()
        );
    }

    /// 快速终态失败照旧提前返回（预算钳制不吞终态分类）。
    #[tokio::test]
    async fn terminal_failure_short_circuits() {
        let (_result, observed) = observe_creation_with(
            view(ComputeControlState::Running),
            std::time::Duration::from_secs(2),
            "app-budget".to_string(),
            || async { false },
            |_operation_id: String| async { Some(record(ComputeControlState::Failed, "op-x")) },
        )
        .await;
        assert!(!observed);
    }

    /// 正常路径：对象出现即观察成功。
    #[tokio::test]
    async fn observed_when_workload_object_appears() {
        let (_result, observed) = observe_creation_with(
            view(ComputeControlState::Running),
            std::time::Duration::from_secs(2),
            "app-budget".to_string(),
            || async { true },
            |_operation_id: String| async { Some(record(ComputeControlState::Succeeded, "op-x")) },
        )
        .await;
        assert!(observed);
    }
}
