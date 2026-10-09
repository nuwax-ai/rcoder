use super::*;

/// serve 核心逻辑：所有权分类 → 统一 owner 承载 / 旧版 worker 兼容 / 分派。
///
/// recovery v2（plan §5）：本进程成为统一 owner 时，公共管理监听（3010）
/// 先于一切业务副作用绑定，原生控制会话与业务编排同进程运行——围栏、
/// Stopped、journal 故障期间管理 API 保持可用。
pub(super) async fn serve_without_attach(args: &RuntimeArgs) -> Result<()> {
    anyhow::ensure!(
        !args.control_only || !crate::deploy::deploy_requested(),
        "control-only startup cannot execute an environment deployment"
    );
    let application_id = std::env::var("PROJECT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".to_string());
    let state_root =
        crate::runtime_kernel::RuntimeStore::resolve_root(&args.workspace, &application_id)?;
    let supervised_worker = runtime_supervisor::Worker::from_env(&state_root).await?;
    if let Some(worker) = supervised_worker {
        // 旧版 owner（进程模式二进制）派生的 worker 子进程：保持旧结构直接
        // 服务业务（升级窗口兼容；新 owner 不再派生 worker）。
        return serve_supervised_worker(args, state_root, worker).await;
    }
    for _ in 0..3 {
        match crate::supervision::supervise(args, true).await? {
            crate::supervision::SupervisionOutcome::Owner(session) => {
                return owner_serve(args, state_root, application_id, session, false).await;
            }
            crate::supervision::SupervisionOutcome::Dispatch => {
                if args.control_only {
                    match crate::owner_dispatch::reuse_management_owner(
                        &args.admin_addr,
                        &args.workspace,
                        &state_root,
                        &application_id,
                    )
                    .await?
                    {
                        crate::owner_dispatch::ManagementReuse::Ready => return Ok(()),
                        crate::owner_dispatch::ManagementReuse::NoOwner => continue,
                    }
                }
                // 显式非 control-only serve 保留原 Source Start 分派语义。
                // 锁被活 owner 持有：分派到其管理 API（R02）。
                match crate::owner_dispatch::dispatch_to_owner(
                    &args.admin_addr,
                    &args.workspace,
                    &state_root,
                    &application_id,
                )
                .await?
                {
                    crate::owner_dispatch::OwnerDispatch::Terminal(view) => {
                        return crate::owner_dispatch::describe_terminal(&view);
                    }
                    crate::owner_dispatch::OwnerDispatch::NoOwner => {
                        continue;
                    }
                }
            }
            crate::supervision::SupervisionOutcome::Legacy => {
                // serve() 已先行处理 attach；此处 Legacy 仅在 worker env 下可达，
                // 而该分支已在上方消费。
                anyhow::bail!("unclassified serve entry; refusing to start a second orchestrator")
            }
        }
    }
    anyhow::bail!("managed owner changed repeatedly during bootstrap; retry this request")
}

/// 统一 owner 服务形态：管理面（API + 原生控制）与业务会话同进程。
///
/// 顺序（plan §5.1）：锁 → 公共监听 bind（端口冲突 fail-fast 于业务副作用
/// 之前）→ 早期身份/端点发布 → 会话循环（围栏清障后逐代次运行业务）。
/// 业务会话失败按会话重启策略收敛；管理 API 与 Stop/Status 全程在线。
pub async fn owner_serve(
    args: &RuntimeArgs,
    state_root: std::path::PathBuf,
    application_id: String,
    session: std::sync::Arc<runtime_supervisor::OwnerSession>,
    run_mode: bool,
) -> Result<()> {
    let normalized = args.for_management()?;
    let args = &normalized;
    session.register_resident_application(&application_id)?;
    session.set_owner_cleanup(Arc::new(OwnerResidentCleanup {
        session: Arc::downgrade(&session),
        args: args.clone(),
        application_id: application_id.clone(),
    }))?;
    let api_listener = crate::api::bind_listener(&args.admin_addr).await?;
    let ready = RuntimeStatusService::default();
    let state = Arc::new(ServerState::new(ready));
    state.initialize_owner_token()?;
    state.mark_kernel_required();
    // R3：会话以 Reconciling 起步——镜像任务启动前的窗口内同步置位，
    // 避免部署状态查询在恢复完成前读到瞬时 200。
    state
        .business_recovery_active
        .store(true, std::sync::atomic::Ordering::Release);
    let api_state = state.clone();
    let api_workspace = args.workspace.clone();
    let api_log_dir = args.log_dir.clone();
    let api_pingap_bin = args.pingap_bin.clone();
    let api_app = crate::api::bound_router(api_workspace, api_log_dir, api_pingap_bin, api_state);
    let bound_api_addr = api_listener
        .local_addr()
        .context("read bound app-cli management address")?
        .to_string();
    let api_failure = Arc::new(std::sync::Mutex::new(None::<String>));
    let api_monitor_failure = api_failure.clone();
    let api_monitor_state = state.clone();
    let api_handle = tokio::spawn(async move {
        let result = axum::serve(api_listener, api_app)
            .await
            .context("serve app-cli management API");
        if let Err(error) = result {
            tracing::error!("app-cli management API failed: {error:#}");
            if let Ok(mut slot) = api_monitor_failure.lock() {
                *slot = Some(format!("{error:#}"));
            }
            api_monitor_state.trigger_cancel();
        }
    });

    // 早期运行内核装配（plan §5.1 step 3）：发布身份/能力与端点记录；业务
    // 恢复尚未发生。装配失败=管理降级（B05 语义）：API 保持在线，业务启动
    // 压制，具体原因可从身份/恢复端点与日志观察到。
    match assemble_runtime_kernel(&state, args).await {
        Ok(kernel) => {
            let credential_result = state
                .control_token()
                .context("owner control token missing")
                .and_then(|token| kernel.store().store_token(&token))
                .context("publish owner control credential");
            match credential_result {
                Ok(()) => {
                    state.set_runtime_kernel(kernel.clone());
                    // R3：管理面可用性与业务恢复解耦——API 已绑定、身份已
                    // 发布即开放管理查询（围栏/Stopped 驻留期间 deploy/status
                    // 以 idle 应答而非 503；file-server 的恢复预检不再被
                    // initializing 窗口卡死）。业务会话自身的恢复窗口仍经
                    // begin_business_session 重新置 initializing 门控新业务受理。
                    state.mark_initialized();
                    // R06 事件桥：编排 EVT 同步进入活跃操作的运行事件 journal +
                    // stdout 管道（dev 任务的 Done/阶段事件经此转发）——统一
                    // owner 路径同样必须安装，否则 file-server 侧等待终局事件
                    // 只能靠预算超时（app 11 式静默）。
                    {
                        let kernel = kernel.clone();
                        let event_state = state.clone();
                        crate::orchestration_events::install_bridge(Box::new(move |json| {
                            if let Some(operation_id) = event_state.current_runtime_operation()
                                && let Some(record) = bridge_event_fields(&json)
                            {
                                kernel.append_orchestration_event(
                                    &operation_id,
                                    &record.stage,
                                    record.service,
                                    &record.event_name,
                                    record.payload,
                                );
                            }
                        }));
                    }
                    let endpoint = crate::runtime_kernel::EndpointRecord {
                        protocol_version: kernel.identity().protocol_version,
                        application_id: kernel.identity().application_id.clone(),
                        workspace_id: kernel.identity().workspace_id.clone(),
                        runtime_instance_id: kernel.identity().runtime_instance_id.clone(),
                        address: bound_api_addr.clone(),
                    };
                    if let Err(endpoint_error) = kernel.store().store_endpoint(&endpoint) {
                        tracing::warn!(
                            "endpoint discovery record publish failed: {endpoint_error:#}"
                        );
                    }
                }
                Err(error) => {
                    tracing::error!(
                        %error,
                        "owner credential publication failed; business startup suppressed"
                    );
                }
            }
        }
        Err(error) => {
            tracing::error!(
                "runtime kernel unavailable ({error:#}); automatic business startup suppressed"
            );
        }
    }
    state.set_business_relaunch({
        let session = session.clone();
        move || session.request_relaunch()
    })?;
    state.set_native_control({
        let session = session.clone();
        move || {
            let snapshot = session.snapshot();
            snapshot
                .operation_id
                .filter(|_| {
                    matches!(
                        snapshot.phase,
                        runtime_supervisor::Phase::Stopping
                            | runtime_supervisor::Phase::CleanupPending
                            | runtime_supervisor::Phase::Reconciling
                    )
                })
                .map(|id| (id, snapshot.phase.to_string()))
        }
    })?;
    // R3：原生会话相位镜像——恢复活跃期（含围栏清理）阻塞 deploy/status，
    // 降级驻留（RecoveryRequired）不阻塞：证据可查、精确 Stop 可达。
    {
        let session = session.clone();
        let mirror_state = state.clone();
        tokio::spawn(async move {
            loop {
                let active = matches!(
                    session.snapshot().phase,
                    runtime_supervisor::Phase::Reconciling
                        | runtime_supervisor::Phase::Starting
                        | runtime_supervisor::Phase::Stopping
                        | runtime_supervisor::Phase::CleanupPending
                );
                mirror_state
                    .business_recovery_active
                    .store(active, std::sync::atomic::Ordering::Release);
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        });
    }

    // RV03：无业务驱动者时的 Stop 执行者。降级驻留（坏 journal/重启预算
    // 耗尽）期间受理的 HTTP Stop 派发进控制通道后没有任何会话消费——
    // 不能永远 Accepted。会话静止（无业务、无清理、围栏清空）即按
    // "已确认无执行"幂等收束该 ID；静止未达成则继续等待（清理确认是
    // Stop 终态的前置，不用重开损坏 journal 充当 Stop 执行）。
    {
        let session = session.clone();
        let watch_state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let Some(kernel) = watch_state.runtime_kernel() else {
                    continue;
                };
                let outstanding = match kernel.outstanding_stop_operations() {
                    Ok(outstanding) => outstanding,
                    Err(error) => {
                        tracing::debug!(%error, "stop watchdog could not scan operations");
                        continue;
                    }
                };
                if outstanding.is_empty() || !session.management_quiescent() {
                    continue;
                }
                for id in outstanding {
                    tracing::warn!(
                        operation_id = %id,
                        "settling admitted stop with no business driver (owner parked)"
                    );
                    if let Err(error) = watch_state
                        .finish_runtime_operation_by_id(
                            &id,
                            shared_types::RuntimeOperationState::Succeeded,
                            None,
                        )
                        .await
                    {
                        tracing::error!(%error, "persist parked stop settlement");
                    }
                }
            }
        });
    }

    let factory_args = args.clone();
    let factory_state = state.clone();
    let factory_state_root = state_root.clone();
    let factory_session = session.clone();
    let session_exit = session
        .run(Box::new(
            move |launch: runtime_supervisor::BusinessLaunch| {
                let args = factory_args.clone();
                let state = factory_state.clone();
                let state_root = factory_state_root.clone();
                let session = factory_session.clone();
                // R4：run 与 serve 消费同一 business_session（server_loop
                // 处理 control/deploy 通道）——API 受理的 Start/Stop/
                // Restart 在 run 形态下同样被消费；差异只在会话重启策略
                //（run 的 restart_on_exit=false，前台退出码语义）。
                let launch_generation = launch.generation.clone();
                let control =
                    factory_state.generation_control(&launch_generation, &args.log_dir)?;
                // RV05：业务会话根任务捕获本代次的不可变命令范围——
                // 进程级会话范围随后被换代/卸下时，会话内（含显式派生的
                // 执行子任务）仍向旧（已闭门）范围登记，不进入 Direct
                // 或新代次。
                let captured_scope = process_utils::command_context::CommandContext::for_work_root(
                    launch.work_root.clone(),
                );
                let end = tokio::spawn(async move {
                    let _ = &launch_generation;
                    captured_scope
                        .scope(async move {
                            business_session(args, state_root, state, session, launch, run_mode)
                                .await
                        })
                        .await
                        .map_err(|error| {
                            tracing::error!(
                                %error,
                                generation = %launch_generation,
                                "business session failed"
                            );
                            error
                        })
                        .map(|_| None::<i32>)
                });
                Ok(runtime_supervisor::BusinessRun {
                    control,
                    end: Box::pin(async move {
                        end.await
                            .map_err(|error| {
                                anyhow::anyhow!("business session task failed: {error}")
                            })
                            .and_then(|result| result)
                    }),
                })
            },
        ))
        .await;

    // Foreground completion and an abnormal owner-loop exit must also close
    // owner resources. Native Shutdown already ran this before its terminal;
    // the same physical cleanup is idempotent and remains evidence-based.
    if let Err(error) = crate::services::supervisor::resident::shutdown().await {
        state.begin_failure(
            format!("owner resident shutdown unconfirmed: {error:#}"),
            true,
        );
        return Err(error).context("owner resident cleanup remains protected");
    }

    api_handle.abort();
    if let Ok(slot) = api_failure.lock()
        && let Some(api_error) = slot.as_ref()
    {
        anyhow::bail!("app-cli management API terminated: {api_error}");
    }
    // RV02：owner 最终退出（Shutdown 终态/前台 run 结束）才清除 endpoint
    // 发现记录——业务会话结束但 owner 存活期间记录保持，跨会话 dispatch
    // 依赖它；进程随 run 返回退出，此刻清除是干净关停的一部分。
    if let Some(kernel) = state.runtime_kernel()
        && let Err(endpoint_error) = kernel.store().clear_endpoint()
    {
        tracing::warn!("endpoint discovery record clear failed: {endpoint_error:#}");
    }
    let exit = session_exit?;
    if exit != 0 {
        anyhow::bail!("unified owner exited with code {exit}");
    }
    Ok(())
}

struct OwnerResidentCleanup {
    session: std::sync::Weak<runtime_supervisor::OwnerSession>,
    args: RuntimeArgs,
    application_id: String,
}

#[async_trait::async_trait]
impl runtime_supervisor::OwnerCleanup for OwnerResidentCleanup {
    async fn shutdown(&self) -> Result<()> {
        let _publication = crate::proxy::compiler::publication_guard().await;
        crate::proxy::compiler::wait_for_file_replacements().await;
        if crate::services::supervisor::resident::owner_scope_root()?.is_none() {
            let session = self
                .session
                .upgrade()
                .context("resident owner session unavailable")?;
            let scope = session.resident_scope(&self.application_id).await?;
            crate::services::supervisor::resident::bind_owner(scope, &self.args)?;
        }
        if let Some(host) = SupervisordHost::detect().await? {
            let scope = crate::services::supervisor::resident::owner_scope()?
                .context("supervisord owner cleanup capability missing")?;
            let runtime_root = crate::proxy::compiler::runtime_root(&self.args.log_dir);
            host.shutdown_owner_entry(&runtime_root, &scope).await?;
        }
        crate::services::supervisor::resident::shutdown().await
    }
}

/// 业务会话失败时确保管理面离开 initializing（R3）。成功路径由
/// mark_initialized 幂等覆盖。
struct DegradeManagementOnDrop<'a>(&'a Arc<ServerState>);
impl Drop for DegradeManagementOnDrop<'_> {
    fn drop(&mut self) {
        if self.0.initializing() {
            self.0.mark_initialized();
            if matches!(self.0.phase(), crate::server::ServerPhase::Idle) {
                self.0.set_phase(crate::server::ServerPhase::Failed(
                    "business initialization failed; management degraded but queryable".into(),
                ));
            }
            tracing::warn!(
                "business session ended in initialization; management degraded to queryable"
            );
        }
    }
}

/// 统一 owner 的一个业务会话：会话状态 re-arm → journal → 启动恢复序列 →
/// 编排主循环，直到取消（Stop/Shutdown/信号）或自身失败。
///
/// 返回 Err 时由会话重启策略收敛（预算耗尽转 RecoveryRequired，管理面
/// 保留）；成功返回即本次会话干净收束。
///
/// RV01：任何出口（含 journal/恢复失败的 `?` 提前返回）都先记录内核执行
/// 交接——仅当会话到达过执行驱动（server_loop）才把槽位记录为它的交接
/// 集；驱动前失败的会话没有消费任何操作，槽位中的新受理保持原样等待
/// 下一个会话。驱动前失败且存在已受理待消费操作时唤醒有限自动重试；
/// 不能把同一旧操作的失败当成新用户请求清零重启预算。
async fn business_session(
    args: RuntimeArgs,
    state_root: std::path::PathBuf,
    state: Arc<ServerState>,
    session: std::sync::Arc<runtime_supervisor::OwnerSession>,
    launch: runtime_supervisor::BusinessLaunch,
    foreground: bool,
) -> Result<()> {
    let driver_reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result = business_session_inner(
        args,
        state_root,
        state.clone(),
        session.clone(),
        launch,
        foreground,
        driver_reached.clone(),
    )
    .await;
    let reached_driver = driver_reached.load(std::sync::atomic::Ordering::Acquire);
    if let Some(kernel) = state.runtime_kernel() {
        if let Err(error) = kernel.note_execution_session_ended(reached_driver).await {
            tracing::error!(%error, "record business session execution handover");
        }
        if !reached_driver && result.is_err() && !foreground && kernel.has_live_admission().await {
            tracing::warn!(
                "business session failed before the driver with admitted work pending; \
                 requesting retry within the existing restart budget"
            );
            session.request_automatic_relaunch();
        }
    }
    result
}

async fn business_session_inner(
    args: RuntimeArgs,
    state_root: std::path::PathBuf,
    state: Arc<ServerState>,
    session: std::sync::Arc<runtime_supervisor::OwnerSession>,
    launch: runtime_supervisor::BusinessLaunch,
    foreground: bool,
    driver_reached: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let ready_guard = session.ready_guard(&launch.generation);
    // R3：业务初始化失败的任何路径都不得把管理面永久留在 initializing
    //（deploy/status 503 → file-server 预检 90s 超时）——失败即降级开放
    // 管理查询（phase=Failed 承载原因），Stop/身份仍可用。
    let _degrade_on_error = DegradeManagementOnDrop(&state);
    if crate::services::supervisor::resident::owner_scope_root()?.is_none() {
        let application_id = state
            .runtime_kernel()
            .context("resident requires runtime identity")?
            .identity()
            .application_id
            .clone();
        let scope = session.resident_scope(&application_id).await?;
        crate::services::supervisor::resident::bind_owner(scope, &args)?;
    }
    // RV04：停止交接状态在 admission 线性化点内探测（durable intent +
    // operation_id；phase 由 launch 保留 Stopped 终态），不再用会话外
    // 的相位快照授权取消接力——Stop 落在旧快照与 token renew 之间不再
    // 被 renew 丢弃。
    state.begin_business_session(launch.generation.clone(), launch.fresh, {
        let session = session.clone();
        move || stop_handover_in_progress(&session)
    });
    let stop_intent = session.snapshot().intent == runtime_supervisor::Intent::Stopped;
    let fresh_launch = launch.fresh;
    // RV01：会话交接收束只针对上一会话记录的待交接集合（note_execution_
    // session_ended），在 journal 打开之前执行——业务初始化本身失败（坏
    // journal/迁移围栏）时，上一会话被中断的操作与 Stop 屏障也必须收束
    // （挂起 Stop 幂等成功），否则没有任何会话能到达内核恢复段。当前
    // 槽位中新受理的请求不受影响。
    if !fresh_launch && let Some(kernel) = state.runtime_kernel() {
        kernel
            .prepare_relaunch()
            .await
            .context("prepare kernel for business relaunch")?;
    }
    let mut journal = Journal::open_with_root(&args.workspace, state_root.clone())?;
    journal.attach_worker_generation(launch.generation.clone());
    if std::env::var(shared_types::APP_DEPLOY_GENERATION_ID).is_err()
        && let Some(receipt) = journal.receipt.as_ref()
    {
        state.set_generation(receipt.generation.clone());
    }
    *state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))? = Some(journal);
    let migrate = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
        .as_mut()
        .context("deployment journal missing")?
        .migrate_after_bind();
    migrate.context("migrate deployment journal after management bind")?;
    crate::deploy::cleanup_startup(&args.workspace).await?;

    let signal_state = state.clone();
    let signal_task = tokio::spawn(async move {
        tokio::select! {
            () = crate::supervisor::sigterm_watch() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        // Serialize with any already accepted blocking journal commit.
        let closer = signal_state.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || closer.close_admission()).await {
            signal_state.begin_failure(format!("shutdown admission task failed: {error}"), true);
            signal_state.close_admission();
        }
    });
    let (host, mut startup_error) = match SupervisordHost::detect().await {
        Ok(host) => (host, None),
        Err(error) => (None, Some(error)),
    };
    if startup_error.is_none() {
        startup_error = establish_startup_quiescence(&state, host.is_some(), async {
            if let Some(host) = host.as_ref() {
                // 同容器换 owner 的启动收束：残留业务组先经 standby 摘流再停
                //（同容器常驻 pingap 仍在服务旧路由——裸停会留裸 502 窗口）。
                let standby_root = crate::proxy::compiler::runtime_root(&args.log_dir);
                host.stop_all(Some(&standby_root)).await?;
            }
            crate::static_hosting::reconcile(&[], &args.workspace, false).await
        })
        .await
        .err();
    }
    let ownership_claimed = startup_error.is_none();
    // B05：内核装配先于启动决策。统一 owner 已在会话外完成装配——这里消费
    // 装配结果；未装配（状态根不可用）时写入口 fail-closed。
    let mut kernel_recovery_hold = false;
    if ownership_claimed {
        match state.runtime_kernel() {
            Some(kernel) => {
                let recovered = kernel
                    .recover()
                    .await
                    .context("recover runtime operation state")?;
                if !recovered.is_empty() {
                    tracing::warn!("runtime operations held for recovery: {:?}", recovered);
                }
                let mut saved = state
                    .journal
                    .lock()
                    .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
                    .as_ref()
                    .and_then(|journal| journal.receipt.clone());
                if let Some(receipt) = saved.as_ref().filter(|receipt| {
                    receipt.boundary == Boundary::Switching
                        && (receipt.generation == state.generation_value()
                            || !env_deploy_requested(&state))
                }) {
                    let reconciled = (|| -> Result<Receipt> {
                        anyhow::ensure!(
                            receipt.generation == state.generation_value(),
                            "switch generation mismatch"
                        );
                        let expected = receipt
                            .operation
                            .artifact_release_id
                            .as_deref()
                            .context("switch intent has no target artifact identity")?;
                        let workspace = resolved_execution_workspace(
                            &args.workspace,
                            receipt.request.execution_target,
                            &state,
                        )?;
                        if !workspace.try_exists()? {
                            let active = receipt
                                .active
                                .as_ref()
                                .context("previous active version missing")?;
                            anyhow::ensure!(
                                receipt.operation.recovery.is_none()
                                    && active
                                        .request
                                        .as_ref()
                                        .is_some_and(|request| request.execution_target
                                            == receipt.request.execution_target),
                                "previous execution binding is not confirmed for restoration"
                            );
                            crate::deploy::restore_previous_generation(
                                &workspace,
                                &active.artifact_release_id,
                            )?;
                        }
                        let release = crate::manifest::read_release_lock(&workspace)?;
                        let mut journal = state
                            .journal
                            .lock()
                            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
                        let journal = journal.as_mut().context("switch journal missing")?;
                        if release.release_id == expected {
                            journal.confirm_switched_artifact(receipt, expected)
                        } else {
                            journal.confirm_preserved_active(receipt, &release.release_id)
                        }
                    })();
                    match reconciled {
                        Ok(receipt) => saved = Some(receipt),
                        Err(error) => {
                            state.begin_runtime_recovery_hold();
                            tracing::error!(%error, "Switch outcome remains unconfirmed");
                        }
                    }
                }
                if let Some(mut receipt) = saved
                    && matches!(
                        receipt.boundary,
                        Boundary::StartupFailed
                            | Boundary::Active
                            | Boundary::Preparing
                            | Boundary::Activated
                    )
                {
                    let evidence = (|| -> Result<()> {
                        // Preparing is persisted before any activation. First
                        // deployment may legitimately have no old active code.
                        if receipt.boundary == Boundary::Preparing && receipt.active.is_none() {
                            return Ok(());
                        }
                        let active = receipt.active.as_ref().context("active artifact missing")?;
                        let target = active
                            .request
                            .as_ref()
                            .and_then(|request| request.execution_target);
                        let workspace =
                            resolved_execution_workspace(&args.workspace, target, &state)?;
                        let release = crate::manifest::read_release_lock(&workspace)?;
                        anyhow::ensure!(
                            release.release_id == active.artifact_release_id,
                            "startup recovery artifact changed"
                        );
                        Ok(())
                    })();
                    match evidence {
                        Ok(()) => {
                            if receipt.boundary == Boundary::Activated {
                                let eligible = match receipt.request.runtime_operation_id.as_ref() {
                                    Some(id) => kernel.get(id).await?.is_some_and(|operation|
                                        operation.state == shared_types::RuntimeOperationState::RecoveryRequired),
                                    None => true,
                                };
                                if eligible && receipt.generation == state.generation_value() {
                                    let normalized = state
                                        .journal
                                        .lock()
                                        .map_err(|_| {
                                            anyhow::anyhow!("deployment journal lock poisoned")
                                        })?
                                        .as_mut()
                                        .context("activation journal missing")?
                                        .confirm_interrupted_activation(&receipt);
                                    match normalized {
                                        Ok(updated) => receipt = updated,
                                        Err(error) => {
                                            state.begin_runtime_recovery_hold();
                                            tracing::error!(%error, "Activation reconciliation could not be persisted");
                                        }
                                    }
                                }
                            }
                            if let Err(error) = kernel.reconcile_quiesced_operation(&receipt).await
                            {
                                tracing::error!(%error, "Quiesced operation reconciliation remains blocked");
                            }
                        }
                        Err(error) => {
                            tracing::error!(%error, "Quiesced operation evidence is incomplete")
                        }
                    }
                }
                if let Err(error) = kernel.reconcile_quiesced_stop().await {
                    tracing::error!(%error, "Persisted stop reconciliation remains blocked");
                }
                // 兜底收敛：精确路径之后仍非终态的操作（含 reconcile 失败的
                // Stop）自动沉降为 Failed。当前本地执行已收束；其他容器的
                // 退役由平台处理，结果未提交不等于远端进程退出；不允许
                // 留下"等人工删除/修复文件"的用户不可恢复状态。
                match kernel.settle_unresolved_recoveries().await {
                    Ok(settled) if !settled.is_empty() => {
                        tracing::warn!(
                            count = settled.len(),
                            "startup settled interrupted operations; automatic business startup proceeds"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "Interrupted operations could not be settled automatically");
                    }
                }
                if kernel.recovery_protection_active() {
                    // 保护此刻只剩存储级故障（记录读不出且隔离失败）——自动
                    // 收敛不可信，保留压制。正常残留（未终态/损坏记录）已在
                    // 上方被收敛或隔离，不会到达此门。
                    kernel_recovery_hold = true;
                    tracing::error!(
                        "runtime state requires storage-level recovery; automatic business startup suppressed"
                    );
                }
            }
            None => {
                kernel_recovery_hold = true;
                tracing::error!(
                    "runtime kernel unavailable; automatic business startup suppressed"
                );
            }
        }
    }
    if stop_intent || args.control_only {
        // Old writers are stopped. Keep historical deployment/migration receipts
        // but let an explicit new operation repair the application.
        state
            .runtime_recovery_hold
            .fetch_and(1, std::sync::atomic::Ordering::AcqRel);
    }
    if stop_intent {
        // Only this authenticated successor writes business intent. The old
        // worker is gone; unknown migrations and operation histories remain.
        // 幂等且不覆盖更新的显式请求：desired 已是 Stopped 不再写——每次
        // 管理会话重开都推进 revision 会把已受理的 start/restart 在提交
        // 屏障误判 Superseded（容器矩阵 R3 实测：native stop 后的重开链
        // 写出无操作对应的 revision+1）；内核已有更新的受理意图时由该
        // 操作自身的 admit 语义管理 desired。
        if let Some(kernel) = state.runtime_kernel() {
            kernel
                .ensure_stopped_if_idle()
                .await
                .context("persist stopped intent without overwriting an admitted request")?;
        }
    }
    let explicit_run_replacement = foreground
        && !args.control_only
        && !env_deploy_requested(&state)
        && !automatic_deployment_recovery_allowed(&state)?;
    let mut explicit_run_operation_id = None;
    let mut first_request =
        if explicit_run_replacement && ownership_claimed && !kernel_recovery_hold {
            // A foreground run is an explicit request, unlike an automatic serve
            // restart. Route lost-journal recovery through the same operation path
            // as a CLI dispatch instead of guessing the old target/credentials.
            state.mark_initialized();
            explicit_run_operation_id = admit_explicit_run_replacement(&args, &state).await?;
            None
        } else if stop_intent {
            None
        } else if let Some(error) = startup_error.as_ref() {
            state.begin_failure(format!("startup shutdown unconfirmed: {error:#}"), true);
            None
        } else if kernel_recovery_hold {
            state.set_phase(ServerPhase::Idle);
            None
        } else {
            match initialize_startup(&args, &state).await {
                Ok(action) => action,
                Err(error) => {
                    tracing::error!(%error, "Deployment startup reconciliation failed");
                    state.ready.set_ready(false);
                    if state.runtime_recovery_hold_active() {
                        // Unknown identity/boundary is not a failed operation of
                        // this owner; preserve its original durable evidence.
                        state.begin_failure(
                            format!("startup recovery required: {error:#}"),
                            !state.source_replacement_hold_only(),
                        );
                    } else if let Err(persist_error) = state
                        .fail_operation(format!("deployment startup: {error:#}"), Boundary::Failed)
                    {
                        state.begin_failure(
                            format!("deployment startup: {error:#}; persist: {persist_error:#}"),
                            true,
                        );
                    }
                    None
                }
            }
        };
    // R03/B05：desired 读取先于启动决策；读取失败同样压制自动启动。
    if ownership_claimed && !kernel_recovery_hold {
        let mut desired_unreadable = false;
        let desired = match state
            .runtime_kernel()
            .map(|kernel| kernel.store().load_desired())
        {
            Some(Ok((desired, _))) => Some(desired),
            Some(Err(error)) => {
                tracing::error!(
                    "desired state unreadable ({error:#}); automatic business recovery suppressed"
                );
                desired_unreadable = true;
                state.set_phase(ServerPhase::Idle);
                None
            }
            None => None,
        };
        if desired_unreadable && first_request.is_some() {
            first_request = None;
            tracing::error!(
                "pre-generated startup action discarded: desired state unreadable (R05)"
            );
        }
        if desired == Some(shared_types::DesiredState::Stopped)
            && matches!(first_request, Some(InitialAction::Existing { .. }))
        {
            first_request = None;
            tracing::info!(
                "desired state is Stopped; automatic business recovery suppressed (staying Idle)"
            );
            state.set_phase(ServerPhase::Idle);
        }
    }

    // 启动恢复完成（含 Failed 相位）：开放写端点受理与 /ready 判定，并发布
    // 本会话的管理就绪（原生快照 phase=ready——管理就绪，非业务就绪）。
    if ownership_claimed {
        if let Some(kernel) = state.runtime_kernel() {
            kernel
                .dispatch_pending_after_quiescence()
                .await
                .context("dispatch original queued input after confirmed startup recovery")?;
        }
        state.mark_initialized();
        ready_guard.mark_ready()?;
    }

    state.set_log_layout(if host.is_some() {
        LogLayout::Supervisord
    } else {
        LogLayout::Builtin
    });

    let result = if let Some(error) = startup_error {
        state.cancel_token().cancelled().await;
        tracing::error!(%error, "startup ownership was not claimed; business session ends");
        Err(error).context("startup ownership was not claimed")
    } else {
        let supervised = host.is_some();
        // RV01：到达此处即本会话即将成为通道消费者（server_loop 启动）——
        // 会话结束时槽内操作按"本会话交接集"处理；此前的失败出口不记录。
        driver_reached.store(true, std::sync::atomic::Ordering::Release);
        let driver_state = state.clone();
        let driver_args = args.clone();
        // RV05：驱动任务延续业务会话捕获的命令范围（spawn_scoped 克隆
        // 当前 task-local 后重装——裸 tokio::spawn 会丢失捕获）。
        let mut driver = process_utils::command_context::spawn_scoped(async move {
            driver_state
                .supervision_driver_started
                .store(true, std::sync::atomic::Ordering::Release);
            // Poll challenges in the actual driver task, outside the business
            // operation queue. Yielding downloads/migrations stay healthy; a
            // blocked driver task or admission path cannot acknowledge them.
            let mut probes = driver_state.supervision_probe_rx.lock().await;
            let driver = server_loop(&driver_args, &driver_state, host, first_request, foreground);
            tokio::pin!(driver);
            loop {
                tokio::select! {
                    result = &mut driver => break result,
                    Some(reply) = probes.recv() => { let _sent = reply.send(()); }
                }
            }
        });
        let cancel = state.cancel_token();
        // R4 前台语义：serve 形态编排失败驻留（Failed 可查询、可再部署），
        // run 形态必须以非零退出终结——有界间隔观察相位，失败即取消并以
        // 该失败原因退出。
        let failure_watch = async {
            if !foreground {
                std::future::pending::<String>().await;
            }
            if explicit_run_replacement {
                let Some(operation_id) = explicit_run_operation_id.as_deref() else {
                    // Work accepted in the launch window belongs to its caller,
                    // not this foreground request. Never terminate it because
                    // this CLI did not obtain its own operation slot.
                    return std::future::pending::<String>().await;
                };
                let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    match observe_explicit_run_failure(&state, operation_id).await {
                        Ok(ExplicitRunObservation::Failure(message)) => {
                            return message;
                        }
                        Ok(ExplicitRunObservation::Detached) => {
                            // Completion, cancellation or a successor retires
                            // this observer permanently. A late old failure
                            // cannot stop the platform's replacement service.
                            return std::future::pending::<String>().await;
                        }
                        Ok(ExplicitRunObservation::Pending) => {}
                        Err(error) => {
                            tracing::warn!(%error, %operation_id, "foreground operation observation failed; preserving current business")
                        }
                    }
                }
            }
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if matches!(state.phase(), ServerPhase::Failed(_)) {
                    state.trigger_cancel();
                    return match state.phase() {
                        ServerPhase::Failed(message) => message,
                        _ => "foreground orchestration failed".to_owned(),
                    };
                }
            }
        };
        tokio::pin!(failure_watch);
        let (driver_result, shutdown_deadline) = tokio::select! {
            result = &mut driver => {
                state.close_admission();
                (result.context("server driver panicked").and_then(|result| result), tokio::time::Instant::now().checked_add(state.shutdown_budget(supervised)).context("shutdown budget exceeds clock range")?)
            },
            message = &mut failure_watch => {
                state.close_admission();
                let deadline = tokio::time::Instant::now().checked_add(state.shutdown_budget(supervised)).context("shutdown budget exceeds clock range")?;
                // A Failed phase requests shutdown; it does not prove that the
                // driver ended. Retire the task before handing the generation
                // to cleanup so it cannot keep writing into the next session.
                let result = match drain_server_driver(&mut driver, deadline).await {
                    Ok(()) => Err(anyhow::anyhow!("{message}")),
                    Err(error) => Err(error).with_context(|| format!("foreground orchestration failed: {message}")),
                };
                (result, deadline)
            },
            () = cancel.cancelled() => {
                let deadline = tokio::time::Instant::now().checked_add(state.shutdown_budget(supervised)).context("shutdown budget exceeds clock range")?;
                let result = drain_server_driver(&mut driver, deadline).await;
                (result, deadline)
            },
        };
        // P1：owner 退出前收束常驻 pingap（owner 域进程随 owner 生死；
        // 槽空为 no-op）。放在代际清理前——generation 清理要求 guardians
        // 静默，常驻进程的 lease 不释放会卡住收束确认。
        match driver_result {
            Ok(()) => match tokio::time::timeout_at(
                shutdown_deadline,
                finish_clean_shutdown(&args, &state, ownership_claimed, false),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => Err(error).context("remaining shutdown writers did not stop"),
            },
            Err(error) => Err(error),
        }
    };
    state.close_admission();
    signal_task.abort();
    result
}

/// RV04：durable 停止交接探测——原生 Stop/Shutdown 已受理（挂起操作身份
/// 与 intent）且尚未发布终态。终态发布（poll_cleanup 或空闲立即完成）会
/// 清空 operation_id，使已完成的停止不构成在途交接；该探测在 admission
/// 线性化点内执行，与 WorkerControl::shutdown 的"先取锁再取消"配合，
/// 取消令牌换代不再丢弃已受理的停止。
fn stop_handover_in_progress(session: &std::sync::Arc<runtime_supervisor::OwnerSession>) -> bool {
    let snapshot = session.snapshot();
    snapshot.operation_id.is_some()
        && matches!(
            snapshot.intent,
            runtime_supervisor::Intent::Stopped | runtime_supervisor::Intent::Shutdown
        )
}

/// Wait for cooperative shutdown within the existing budget. If it expires,
/// abort and join the driver before releasing its generation to recovery.
/// Aborting alone only schedules cancellation; dropping the handle detaches it.
pub(super) async fn drain_server_driver(
    driver: &mut tokio::task::JoinHandle<Result<()>>,
    deadline: tokio::time::Instant,
) -> Result<()> {
    match tokio::time::timeout_at(deadline, &mut *driver).await {
        Ok(result) => result
            .context("server driver panicked")
            .and_then(|result| result),
        Err(timeout_error) => {
            driver.abort();
            match (&mut *driver).await {
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    return Err(error).context("join server driver after shutdown timeout");
                }
                Ok(result) => result.context("server driver failed after shutdown timeout")?,
            }
            Err(timeout_error).context("server shutdown confirmation timed out")
        }
    }
}

/// 旧版进程模式 owner 派生的 worker 子进程路径（升级窗口兼容）：保持既有
/// 二进制结构——本进程绑定 3010 并承载全部业务编排，父 owner 负责监督。
async fn serve_supervised_worker(
    args: &RuntimeArgs,
    state_root: std::path::PathBuf,
    supervised_worker: runtime_supervisor::Worker,
) -> Result<()> {
    let stop_intent = supervised_worker.intent() == runtime_supervisor::Intent::Stopped;
    // Hold the real public listener before legacy journal archival/migration.
    let api_listener = crate::api::bind_listener(&args.admin_addr).await?;
    let bound_api_addr = api_listener
        .local_addr()
        .context("read bound app-cli management address")?
        .to_string();
    let journal = Journal::open_recovering(&args.workspace, state_root.clone(), true)?;
    let ready = RuntimeStatusService::default();
    let initial_state = ServerState::new(ready.clone());
    initial_state.initialize_owner_token()?;
    // A failed serve startup is a recovery failure, never legacy run mode.
    initial_state.mark_kernel_required();
    if std::env::var(shared_types::APP_DEPLOY_GENERATION_ID).is_err()
        && let Some(receipt) = journal.receipt.as_ref()
    {
        initial_state.set_generation(receipt.generation.clone());
    }
    let state = Arc::new(initial_state);
    *state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))? = Some(journal);

    let api_state = state.clone();
    let api_workspace = args.workspace.clone();
    let api_log_dir = args.log_dir.clone();
    let api_pingap_bin = args.pingap_bin.clone();
    let api_app = crate::api::bound_router(api_workspace, api_log_dir, api_pingap_bin, api_state);
    let api_failure = Arc::new(std::sync::Mutex::new(None::<String>));
    let api_monitor_failure = api_failure.clone();
    let api_monitor_state = state.clone();
    let api_handle = tokio::spawn(async move {
        let result = axum::serve(api_listener, api_app)
            .await
            .context("serve app-cli management API");
        if let Err(error) = result {
            tracing::error!("app-cli management API failed: {error:#}");
            if let Ok(mut slot) = api_monitor_failure.lock() {
                *slot = Some(format!("{error:#}"));
            }
            api_monitor_state.trigger_cancel();
        }
    });
    let _worker_control = Some(supervised_worker.serve(state.clone()).await?);

    let migrate = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
        .as_mut()
        .context("deployment journal missing")?
        .migrate_after_bind();
    if let Err(error) = migrate {
        api_handle.abort();
        return Err(error).context("migrate deployment journal after management bind");
    }
    crate::deploy::cleanup_startup(&args.workspace).await?;

    let signal_state = state.clone();
    let signal_task = tokio::spawn(async move {
        tokio::select! {
            () = crate::supervisor::sigterm_watch() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        let closer = signal_state.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || closer.close_admission()).await {
            signal_state.begin_failure(format!("shutdown admission task failed: {error}"), true);
            signal_state.close_admission();
        }
    });
    let (host, mut startup_error) = match SupervisordHost::detect().await {
        Ok(host) => (host, None),
        Err(error) => (None, Some(error)),
    };
    if startup_error.is_none() {
        startup_error = establish_startup_quiescence(&state, host.is_some(), async {
            if let Some(host) = host.as_ref() {
                // 同容器换 owner 的启动收束：残留业务组先经 standby 摘流再停
                //（同容器常驻 pingap 仍在服务旧路由——裸停会留裸 502 窗口）。
                let standby_root = crate::proxy::compiler::runtime_root(&args.log_dir);
                host.stop_all(Some(&standby_root)).await?;
            }
            crate::static_hosting::reconcile(&[], &args.workspace, false).await
        })
        .await
        .err();
    }
    let ownership_claimed = startup_error.is_none();
    let mut kernel_recovery_hold = false;
    if ownership_claimed {
        state.mark_kernel_required();
        match assemble_runtime_kernel(&state, args).await {
            Ok(kernel) => {
                let recovered = kernel
                    .recover()
                    .await
                    .context("recover runtime operation state")?;
                if !recovered.is_empty() {
                    tracing::warn!("runtime operations held for recovery: {:?}", recovered);
                }
                let mut saved = state
                    .journal
                    .lock()
                    .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
                    .as_ref()
                    .and_then(|journal| journal.receipt.clone());
                if let Some(receipt) = saved.as_ref().filter(|receipt| {
                    receipt.boundary == Boundary::Switching
                        && (receipt.generation == state.generation_value()
                            || !env_deploy_requested(&state))
                }) {
                    let reconciled = (|| -> Result<Receipt> {
                        anyhow::ensure!(
                            receipt.generation == state.generation_value(),
                            "switch generation mismatch"
                        );
                        let expected = receipt
                            .operation
                            .artifact_release_id
                            .as_deref()
                            .context("switch intent has no target artifact identity")?;
                        let workspace = resolved_execution_workspace(
                            &args.workspace,
                            receipt.request.execution_target,
                            &state,
                        )?;
                        if !workspace.try_exists()? {
                            let active = receipt
                                .active
                                .as_ref()
                                .context("previous active version missing")?;
                            anyhow::ensure!(
                                receipt.operation.recovery.is_none()
                                    && active
                                        .request
                                        .as_ref()
                                        .is_some_and(|request| request.execution_target
                                            == receipt.request.execution_target),
                                "previous execution binding is not confirmed for restoration"
                            );
                            crate::deploy::restore_previous_generation(
                                &workspace,
                                &active.artifact_release_id,
                            )?;
                        }
                        let release = crate::manifest::read_release_lock(&workspace)?;
                        let mut journal = state
                            .journal
                            .lock()
                            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
                        let journal = journal.as_mut().context("switch journal missing")?;
                        if release.release_id == expected {
                            journal.confirm_switched_artifact(receipt, expected)
                        } else {
                            journal.confirm_preserved_active(receipt, &release.release_id)
                        }
                    })();
                    match reconciled {
                        Ok(receipt) => saved = Some(receipt),
                        Err(error) => {
                            state.begin_runtime_recovery_hold();
                            tracing::error!(%error, "Switch outcome remains unconfirmed");
                        }
                    }
                }
                if let Some(mut receipt) = saved
                    && matches!(
                        receipt.boundary,
                        Boundary::StartupFailed
                            | Boundary::Active
                            | Boundary::Preparing
                            | Boundary::Activated
                    )
                {
                    let evidence = (|| -> Result<()> {
                        if receipt.boundary == Boundary::Preparing && receipt.active.is_none() {
                            return Ok(());
                        }
                        let active = receipt.active.as_ref().context("active artifact missing")?;
                        let target = active
                            .request
                            .as_ref()
                            .and_then(|request| request.execution_target);
                        let workspace =
                            resolved_execution_workspace(&args.workspace, target, &state)?;
                        let release = crate::manifest::read_release_lock(&workspace)?;
                        anyhow::ensure!(
                            release.release_id == active.artifact_release_id,
                            "startup recovery artifact changed"
                        );
                        Ok(())
                    })();
                    match evidence {
                        Ok(()) => {
                            if receipt.boundary == Boundary::Activated {
                                let eligible = match receipt.request.runtime_operation_id.as_ref() {
                                    Some(id) => kernel.get(id).await?.is_some_and(|operation|
                                        operation.state == shared_types::RuntimeOperationState::RecoveryRequired),
                                    None => true,
                                };
                                if eligible && receipt.generation == state.generation_value() {
                                    let normalized = state
                                        .journal
                                        .lock()
                                        .map_err(|_| {
                                            anyhow::anyhow!("deployment journal lock poisoned")
                                        })?
                                        .as_mut()
                                        .context("activation journal missing")?
                                        .confirm_interrupted_activation(&receipt);
                                    match normalized {
                                        Ok(updated) => receipt = updated,
                                        Err(error) => {
                                            state.begin_runtime_recovery_hold();
                                            tracing::error!(%error, "Activation reconciliation could not be persisted");
                                        }
                                    }
                                }
                            }
                            if let Err(error) = kernel.reconcile_quiesced_operation(&receipt).await
                            {
                                tracing::error!(%error, "Quiesced operation reconciliation remains blocked");
                            }
                        }
                        Err(error) => {
                            tracing::error!(%error, "Quiesced operation evidence is incomplete")
                        }
                    }
                }
                if let Err(error) = kernel.reconcile_quiesced_stop().await {
                    tracing::error!(%error, "Persisted stop reconciliation remains blocked");
                }
                match kernel.settle_unresolved_recoveries().await {
                    Ok(settled) if !settled.is_empty() => {
                        tracing::warn!(
                            count = settled.len(),
                            "startup settled interrupted operations; automatic business startup proceeds"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "Interrupted operations could not be settled automatically");
                    }
                }
                if kernel.recovery_protection_active() {
                    kernel_recovery_hold = true;
                    tracing::error!(
                        "runtime state requires storage-level recovery; automatic business startup suppressed"
                    );
                }
                let credential_result = state
                    .control_token()
                    .context("owner control token missing")
                    .and_then(|token| kernel.store().store_token(&token))
                    .context("publish owner control credential");
                if let Err(error) = credential_result {
                    state.close_admission();
                    api_handle.abort();
                    signal_task.abort();
                    return Err(error);
                }
                state.set_runtime_kernel(kernel.clone());
                {
                    let kernel = kernel.clone();
                    let event_state = state.clone();
                    crate::orchestration_events::install_bridge(Box::new(move |json| {
                        if let Some(operation_id) = event_state.current_runtime_operation()
                            && let Some(record) = bridge_event_fields(&json)
                        {
                            kernel.append_orchestration_event(
                                &operation_id,
                                &record.stage,
                                record.service,
                                &record.event_name,
                                record.payload,
                            );
                        }
                    }));
                }
                if let Err(endpoint_error) =
                    kernel
                        .store()
                        .store_endpoint(&crate::runtime_kernel::EndpointRecord {
                            protocol_version: kernel.identity().protocol_version,
                            application_id: kernel.identity().application_id.clone(),
                            workspace_id: kernel.identity().workspace_id.clone(),
                            runtime_instance_id: kernel.identity().runtime_instance_id.clone(),
                            address: bound_api_addr,
                        })
                {
                    tracing::warn!("endpoint discovery record publish failed: {endpoint_error:#}");
                }
            }
            Err(error) => {
                kernel_recovery_hold = true;
                tracing::error!(
                    "runtime kernel unavailable ({error:#}); automatic business startup suppressed"
                );
            }
        }
    }
    if stop_intent || args.control_only {
        state
            .runtime_recovery_hold
            .fetch_and(1, std::sync::atomic::Ordering::AcqRel);
    }
    if stop_intent {
        if let Some(kernel) = state.runtime_kernel() {
            kernel
                .ensure_stopped_if_idle()
                .await
                .context("persist legacy stopped intent without overwriting an admitted request")?;
        } else {
            let _admission = state
                .admission
                .lock()
                .map_err(|_| anyhow::anyhow!("runtime admission lock poisoned"))?;
            let store = crate::runtime_kernel::RuntimeStore::open_with_root(
                state_root.clone(),
                &args.workspace,
            )?;
            let (desired, revision) = store.load_desired()?;
            if desired != shared_types::DesiredState::Stopped {
                store.store_desired(
                    shared_types::DesiredState::Stopped,
                    revision
                        .checked_add(1)
                        .context("desired revision overflow")?,
                )?;
            }
        }
    }
    let mut first_request = if stop_intent {
        None
    } else if let Some(error) = startup_error.as_ref() {
        state.begin_failure(format!("startup shutdown unconfirmed: {error:#}"), true);
        None
    } else if kernel_recovery_hold {
        state.set_phase(ServerPhase::Idle);
        None
    } else {
        match initialize_startup(args, &state).await {
            Ok(action) => action,
            Err(error) => {
                tracing::error!(%error, "Deployment startup reconciliation failed");
                state.ready.set_ready(false);
                if state.runtime_recovery_hold_active() {
                    state.begin_failure(
                        format!("startup recovery required: {error:#}"),
                        !state.source_replacement_hold_only(),
                    );
                } else if let Err(persist_error) =
                    state.fail_operation(format!("deployment startup: {error:#}"), Boundary::Failed)
                {
                    state.begin_failure(
                        format!("deployment startup: {error:#}; persist: {persist_error:#}"),
                        true,
                    );
                }
                None
            }
        }
    };
    if ownership_claimed && !kernel_recovery_hold {
        let mut desired_unreadable = false;
        let desired = match state
            .runtime_kernel()
            .map(|kernel| kernel.store().load_desired())
        {
            Some(Ok((desired, _))) => Some(desired),
            Some(Err(error)) => {
                tracing::error!(
                    "desired state unreadable ({error:#}); automatic business recovery suppressed"
                );
                desired_unreadable = true;
                state.set_phase(ServerPhase::Idle);
                None
            }
            None => None,
        };
        if desired_unreadable && first_request.is_some() {
            first_request = None;
            tracing::error!(
                "pre-generated startup action discarded: desired state unreadable (R05)"
            );
        }
        if desired == Some(shared_types::DesiredState::Stopped)
            && matches!(first_request, Some(InitialAction::Existing { .. }))
        {
            first_request = None;
            tracing::info!(
                "desired state is Stopped; automatic business recovery suppressed (staying Idle)"
            );
            state.set_phase(ServerPhase::Idle);
        }
    }

    if ownership_claimed {
        state.mark_initialized();
    }

    state.set_log_layout(if host.is_some() {
        LogLayout::Supervisord
    } else {
        LogLayout::Builtin
    });

    let result = if let Some(error) = startup_error {
        state.cancel_token().cancelled().await;
        tracing::error!(%error, "startup ownership was not claimed; business session ends");
        Err(error).context("startup ownership was not claimed")
    } else {
        let supervised = host.is_some();
        let driver_args = args.clone();
        let driver_state = state.clone();
        let mut driver = tokio::spawn(async move {
            driver_state
                .supervision_driver_started
                .store(true, std::sync::atomic::Ordering::Release);
            let mut probes = driver_state.supervision_probe_rx.lock().await;
            let driver = server_loop(&driver_args, &driver_state, host, first_request, false);
            tokio::pin!(driver);
            loop {
                tokio::select! {
                    result = &mut driver => break result,
                    Some(reply) = probes.recv() => { let _sent = reply.send(()); }
                }
            }
        });
        let mut joined = false;
        let cancel = state.cancel_token();
        let (driver_result, shutdown_deadline) = tokio::select! {
            result = &mut driver => {
                joined = true;
                state.close_admission();
                (result.context("server driver panicked").and_then(|result| result), tokio::time::Instant::now().checked_add(state.shutdown_budget(supervised)).context("shutdown budget exceeds clock range")?)
            },
            () = cancel.cancelled() => {
                let deadline = tokio::time::Instant::now().checked_add(state.shutdown_budget(supervised)).context("shutdown budget exceeds clock range")?;
                let result = match tokio::time::timeout_at(deadline, &mut driver).await {
                    Ok(result) => { joined = true; result.context("server driver panicked").and_then(|result| result) },
                    Err(error) => Err(error).context("server shutdown confirmation timed out"),
                };
                (result, deadline)
            },
        };
        if !joined {
            driver.abort();
        }
        match driver_result {
            Ok(()) => match tokio::time::timeout_at(
                shutdown_deadline,
                finish_clean_shutdown(args, &state, ownership_claimed, true),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => Err(error).context("remaining shutdown writers did not stop"),
            },
            Err(error) => Err(error),
        }
    };
    state.close_admission();
    api_handle.abort();
    signal_task.abort();
    if let Ok(slot) = api_failure.lock()
        && let Some(api_error) = slot.as_ref()
    {
        return Err(
            anyhow::anyhow!("app-cli management API terminated: {api_error}").context("serve"),
        );
    }
    result
}

/// 干净关停确认（RV02 重整）：
/// - 部署/控制通道是 **owner 级**资源，业务会话退出不关闭、不排空——
///   统一 owner 的 ServerState 跨会话复用同一 receiver，关闭会让后续
///   会话的 server_loop 立即 recv None 返回、再部署永久失败。残留的旧
///   信号由消费侧按内核终态核验（settle 路径跳过已收束操作），不靠盲
///   drain（那会丢弃会话结束后新受理的请求）。
/// - endpoint 发现记录只在 owner 最终退出时清除（legacy worker 进程随
///   会话退出；统一 owner 由 owner_serve 在 run 返回后清除）——业务会话
///   结束但 owner 存活期间发现记录必须保持，供跨会话 dispatch 使用。
pub(super) async fn finish_clean_shutdown(
    args: &RuntimeArgs,
    state: &ServerState,
    ownership_claimed: bool,
    owner_exiting: bool,
) -> Result<()> {
    anyhow::ensure!(ownership_claimed, "coordinator ownership was not claimed");
    anyhow::ensure!(
        !state.accepting.load(std::sync::atomic::Ordering::Acquire),
        "deployment admission must be closed before shutdown confirmation"
    );
    anyhow::ensure!(
        !state
            .shutdown_unconfirmed
            .load(std::sync::atomic::Ordering::Acquire),
        "an earlier shutdown failure remains unconfirmed"
    );
    state.preparations.drain().await?;
    crate::static_hosting::reconcile(&[], &args.workspace, false).await?;
    if owner_exiting {
        // 干净退出清除 endpoint 发现记录（崩溃残留的旧记录由客户端核验拒绝）
        if let Some(kernel) = state.runtime_kernel() {
            kernel.store().clear_endpoint()?;
        }
    }
    let mut guard = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
    guard
        .as_mut()
        .context("deployment journal missing")?
        .commit_quiescent()
}

pub(super) async fn join_supervisor(
    task: &mut tokio::task::JoinHandle<Result<()>>,
    joined: &mut bool,
) -> Result<()> {
    if *joined {
        return Ok(());
    }
    let result = task.await;
    *joined = true;
    result.context("supervisor task panicked")?
}

/// Control identifiers are opaque keys, not filesystem path components. Keep
/// existing valid project names stable; native names outside the wire grammar
/// use a deterministic digest. Source, .run and filesystem aliases share a key.
pub(super) fn runtime_workspace_id(workspace: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};

    let project_root = runtime_state_layout::canonical_project_root(workspace);
    if let Some(name) = project_root.file_name().and_then(|name| name.to_str())
        && shared_types::validate_identifier(name, "workspace_id").is_ok()
    {
        return name.to_owned();
    }
    // Hash the platform's lossless path encoding, not a lossy display string.
    // A full SHA-256 hex digest fits the protocol's 64-byte identifier limit.
    hex::encode(Sha256::digest(project_root.as_os_str().as_encoded_bytes()))
}

/// 装配运行操作内核（serve 专用；dispatch 把内核动作翻译进既有执行通道）。
pub(super) async fn assemble_runtime_kernel(
    state: &Arc<ServerState>,
    args: &crate::config::RuntimeArgs,
) -> Result<Arc<crate::runtime_kernel::RuntimeKernel>> {
    use crate::runtime_kernel::{DispatchAction, RuntimeKernel, RuntimeStore};
    let application_id = std::env::var("PROJECT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".to_string());
    // B04：显式状态根（env 权威；缺省按应用隔离）——source/.run/别名同域
    let root = RuntimeStore::resolve_root(&args.workspace, &application_id)?;
    // Select a standalone deployment identity under the same authority/lease
    // before publishing the immutable kernel identity. Platform env wins.
    if std::env::var(shared_types::APP_DEPLOY_GENERATION_ID).is_err()
        && let Err(error) =
            restore_standalone_generation_before_identity(state, &args.workspace, &root)
    {
        tracing::error!(%error, "Standalone deployment identity could not be restored; management remains available with recovery protection");
        if !error.is::<credential_recovery::SourceHistoryProblem>() {
            state.begin_runtime_recovery_hold();
        }
    }
    let workspace_id = crate::control::managed_owner::workspace_id(
        &args.workspace,
        &root,
        runtime_workspace_id(&args.workspace),
    )?;
    let store = RuntimeStore::open_with_root(root, &args.workspace)?;
    let source_root = runtime_state_layout::canonical_project_root(&args.workspace)
        .to_string_lossy()
        .into_owned();
    let identity = store.load_or_init_identity(
        application_id,
        "userapp-dev".to_string(),
        workspace_id,
        source_root,
        state.generation_value(),
    )?;
    let dispatch_state = state.clone();
    let dispatch_workspace = runtime_state_layout::resolve_project_origin(&args.workspace)
        .context("resolve owner execution project")?;
    state
        .execution_project
        .set(dispatch_workspace.clone())
        .map_err(|_| anyhow::anyhow!("owner execution project was already initialized"))?;
    let owner_workspace = args.workspace.clone();
    state
        .owner_execution_workspace
        .set(owner_workspace.clone())
        .map_err(|_| anyhow::anyhow!("owner execution workspace was already initialized"))?;
    let dispatch = Box::new(move |action: DispatchAction| {
        let executing_id = match &action {
            DispatchAction::OrchestrateSource { operation_id, .. }
            | DispatchAction::DeployLocalArtifact { operation_id, .. }
            | DispatchAction::DeployArtifact { operation_id, .. } => Some(operation_id),
            _ => None,
        };
        if let Some(id) = executing_id
            && dispatch_state
                .runtime_kernel()
                .is_some_and(|kernel| kernel.is_cancelled(id))
        {
            let state = dispatch_state.clone();
            let id = id.clone();
            tokio::spawn(async move {
                state.settle_cancelled_before_execution(&id).await;
            });
            return;
        }
        let credential_recovery = match &action {
            DispatchAction::OrchestrateSource { pg, .. } => dispatch_state
                .credential_recovery(pg.as_ref(), CredentialRecoveryPurpose::StartCurrentSource),
            DispatchAction::DeployArtifact { pg, .. }
            | DispatchAction::DeployLocalArtifact { pg, .. } => dispatch_state.credential_recovery(
                pg.as_ref(),
                CredentialRecoveryPurpose::RestoreConfirmedArtifact,
            ),
            DispatchAction::StopBusiness { .. } => Ok(CredentialRecovery::NotRequired),
        };
        let (evaluated_recovery, mut recovery_failure) = match credential_recovery {
            Ok(allowed) => (allowed, None),
            Err(error) => {
                tracing::warn!(%error, "Credential recovery validation failed at dispatch");
                (
                    CredentialRecovery::NotRequired,
                    Some(format!("validate runtime recovery: {error:#}")),
                )
            }
        };
        if evaluated_recovery.supplies_credentials()
            && let Some(operation_id) = executing_id
        {
            // Kernel admission checked instance/revision and its separate
            // recovery fence. Never clear a concurrent unknown-state hold.
            if let Err(error) =
                dispatch_state.consume_credentials_hold(operation_id, evaluated_recovery)
            {
                tracing::warn!(%error, "Credential recovery handoff failed");
                recovery_failure = Some(format!("runtime recovery handoff: {error:#}"));
            }
        }
        if let Some(id) = executing_id
            && (recovery_failure.is_some() || dispatch_state.runtime_recovery_hold_active())
        {
            // The loop remains alive for Stop, but must not turn missing
            // startup credentials or an uncertain journal into permission to
            // run a different request with environment defaults.
            let state = dispatch_state.clone();
            let id = id.clone();
            let reason = recovery_failure.unwrap_or_else(|| {
                "runtime recovery must be resolved before starting business".into()
            });
            tokio::spawn(async move {
                if let Err(error) = state
                    .finish_runtime_operation_by_id(
                        &id,
                        // No control signal or command was sent. Preserve the
                        // real existing hold, but do not invent an unknown result
                        // for this rejected request and permanently fence retry.
                        shared_types::RuntimeOperationState::Failed,
                        Some((shared_types::ERR_RECOVERY_REQUIRED.into(), reason)),
                    )
                    .await
                {
                    tracing::error!(%error, "Preserve blocked runtime dispatch");
                }
            });
            return;
        }
        // Validate before sending a control signal that may stop the old services.
        let source_operation = match &action {
            DispatchAction::OrchestrateSource { operation_id, .. }
            | DispatchAction::DeployLocalArtifact { operation_id, .. } => Some(operation_id),
            _ => None,
        };
        if let Some(operation_id) = source_operation
            && let Err(error) = resolved_execution_workspace(
                &owner_workspace,
                Some(ExecutionTarget::Source),
                &dispatch_state,
            )
        {
            let state = dispatch_state.clone();
            let id = operation_id.clone();
            tokio::spawn(async move {
                if let Err(error) = state
                    .finish_runtime_operation_by_id(
                        &id,
                        shared_types::RuntimeOperationState::Failed,
                        Some((
                            shared_types::ERR_BACKEND_ERROR.into(),
                            format!("resolve execution project: {error:#}"),
                        )),
                    )
                    .await
                {
                    hold_unconfirmed(&state, format!("persist project rejection: {error:#}")).await;
                }
            });
            return;
        }
        match action {
            DispatchAction::DeployLocalArtifact {
                operation_id,
                artifact_id,
                sha256,
                pg,
            } => {
                // R03：登记的本地构建制品——共享卷 builds/ 目录 zip 由 owner 侧
                // 部署准备链校验/解压/激活（不经网络下载；artifact_id 已过
                // identifier 白名单，路径拼接无穿越面）
                let settle_state = dispatch_state.clone();
                if settle_state
                    .runtime_kernel()
                    .is_some_and(|kernel| kernel.is_cancelled(&operation_id))
                {
                    tokio::spawn(async move {
                        settle_state
                            .settle_cancelled_before_execution(&operation_id)
                            .await;
                    });
                    return;
                }
                let local_path = Some(
                    dispatch_workspace
                        .join("builds")
                        .join(format!("workspace-package-{artifact_id}.zip")),
                );
                if let Some(path) = &local_path
                    && !path.exists()
                {
                    tracing::warn!(
                        "runtime dispatch: registered artifact {artifact_id} not found at {}; \
                         recovery continues only if the activated run directory still \
                         declares this release",
                        path.display()
                    );
                }
                let marker = format!("runtime-{operation_id}");
                let request = DeployRequest {
                    runtime_operation_id: Some(operation_id.clone()),
                    url: format!("artifact://{artifact_id}"),
                    release_id: marker,
                    sha256,
                    local_path,
                    execution_target: Some(ExecutionTarget::ProjectRun),

                    run_pg: pg,
                };
                if dispatch_state.deploy_tx.send(request).is_err() {
                    // RV02：发送失败按原 ID 收束真实失败——内核已受理，
                    // 不能只打日志留下永久 Accepted。
                    let state = dispatch_state.clone();
                    let id = operation_id.clone();
                    tracing::error!("runtime dispatch: deploy channel closed ({id})");
                    tokio::spawn(async move {
                        if let Err(error) = state
                            .finish_runtime_operation_by_id(
                                &id,
                                shared_types::RuntimeOperationState::Failed,
                                Some((
                                    "ERR_BACKEND_ERROR".into(),
                                    "runtime deploy channel closed".into(),
                                )),
                            )
                            .await
                        {
                            tracing::error!(%error, "persist deploy channel failure");
                        }
                    });
                } else {
                    // 统一 owner：受理后若业务会话未在跑（Stopped 后的显式
                    // 部署/启动），触发会话重建消费该受理。
                    dispatch_state.request_business_relaunch();
                }
            }
            DispatchAction::DeployArtifact {
                operation_id,
                url,
                sha256,
                pg,
            } => {
                // R03 取消检查点：排队期间被取消 → 不进部署链（dispatch 是同步
                // 闭包，终态收束经 spawn 落盘）
                let settle_state = dispatch_state.clone();
                if settle_state
                    .runtime_kernel()
                    .is_some_and(|kernel| kernel.is_cancelled(&operation_id))
                {
                    tokio::spawn(async move {
                        settle_state
                            .settle_cancelled_before_execution(&operation_id)
                            .await;
                    });
                    return;
                }
                // release_id 语义 = 调用方请求标识（request_release_id 驱动等待方
                // 确认）；以 runtime 操作 ID 承载，形成 API 侧可观察的关联。
                let marker = format!("runtime-{operation_id}");
                let request = DeployRequest {
                    runtime_operation_id: Some(operation_id.clone()),
                    url,
                    release_id: marker,
                    sha256,
                    local_path: None,
                    execution_target: None,

                    run_pg: pg,
                };
                if dispatch_state.deploy_tx.send(request).is_err() {
                    // RV02：同上——发送失败按原 ID 收束真实失败。
                    let state = dispatch_state.clone();
                    let id = operation_id.clone();
                    tracing::error!("runtime dispatch: deploy channel closed ({id})");
                    tokio::spawn(async move {
                        if let Err(error) = state
                            .finish_runtime_operation_by_id(
                                &id,
                                shared_types::RuntimeOperationState::Failed,
                                Some((
                                    "ERR_BACKEND_ERROR".into(),
                                    "runtime deploy channel closed".into(),
                                )),
                            )
                            .await
                        {
                            tracing::error!(%error, "persist deploy channel failure");
                        }
                    });
                } else {
                    // 统一 owner：受理后若业务会话未在跑（Stopped 后的显式
                    // 部署/启动），触发会话重建消费该受理。
                    dispatch_state.request_business_relaunch();
                }
            }
            DispatchAction::OrchestrateSource {
                operation_id,
                dev_profile,
                pg,
            } => {
                let state = dispatch_state.clone();
                let source = dispatch_workspace.clone();
                tokio::spawn(async move {
                    if state.settle_cancelled_before_execution(&operation_id).await {
                        return;
                    }
                    // This original admitted Source operation owns derivation;
                    // neither its CLI caller nor artifact restoration writes it.
                    // Retain the writer until physical publication has ended so
                    // a native Stop/Shutdown cannot claim an unfinished writer.
                    let checked = async {
                        let mut writer = state.begin_auxiliary_write()?;
                        let prepared = crate::build_deploy::source_lock::prepare(&source).await;
                        // This is a rebuildable cache, not an unknown database
                        // write. A completed I/O error remains retryable input.
                        writer.confirm();
                        prepared?;
                        crate::manifest::preflight_startup(&source, dev_profile).await
                    }
                    .await;
                    if let Err(error) = checked {
                        if let Err(persist) = state
                            .finish_runtime_operation_by_id(
                                &operation_id,
                                shared_types::RuntimeOperationState::Failed,
                                Some((
                                    shared_types::ERR_VALIDATION.into(),
                                    format!("startup preflight: {error:#}"),
                                )),
                            )
                            .await
                        {
                            hold_unconfirmed(
                                &state,
                                format!("persist startup rejection: {persist:#}"),
                            )
                            .await;
                        }
                        return;
                    }
                    if state.settle_cancelled_before_execution(&operation_id).await {
                        return;
                    }
                    match state.control_tx.send(ControlSignal::OrchestrateSource {
                        operation_id: operation_id.clone(),
                        dev_profile,
                        pg,
                    }) {
                        Err(_) => {
                            hold_unconfirmed(
                                &state,
                                format!("runtime control channel closed ({operation_id})"),
                            )
                            .await;
                        }
                        Ok(()) => {
                            // 统一 owner：同上——受理后确保业务会话在跑。
                            state.request_business_relaunch();
                        }
                    }
                });
            }
            DispatchAction::StopBusiness { operation_id } => {
                // R01：Stop 在 active 期间受理（意图屏障），但**不抢占执行身份**——
                // 主循环在下一个边界消费本信号并按此 ID 显式执行/收束停止操作。
                if dispatch_state
                    .control_tx
                    .send(ControlSignal::StopBusiness {
                        operation_id: operation_id.clone(),
                    })
                    .is_err()
                {
                    // RV02：发送失败按原 ID 收束真实失败；owner 驻留（无会话
                    // 消费）期间的 Stop 由 owner_serve 的停止看门狗收束。
                    let state = dispatch_state.clone();
                    let id = operation_id.clone();
                    tracing::error!("runtime dispatch: control channel closed ({id})");
                    tokio::spawn(async move {
                        if let Err(error) = state
                            .finish_runtime_operation_by_id(
                                &id,
                                shared_types::RuntimeOperationState::Failed,
                                Some((
                                    "ERR_BACKEND_ERROR".into(),
                                    "runtime control channel closed".into(),
                                )),
                            )
                            .await
                        {
                            tracing::error!(%error, "persist control channel failure");
                        }
                    });
                }
            }
        }
    });
    Ok(Arc::new(RuntimeKernel::new(store, identity, dispatch)))
}

fn restore_standalone_generation_before_identity(
    state: &ServerState,
    workspace: &std::path::Path,
    root: &std::path::Path,
) -> Result<()> {
    let mut slot = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
    // Legacy workers already opened and selected their journal before kernel
    // assembly. Retain that same authority instead of taking a second lease.
    if slot.is_some() {
        return Ok(());
    }
    let mut journal = Journal::open_with_root(workspace, root.to_path_buf())?;
    journal.migrate_after_bind()?;
    if let Some(receipt) = journal.receipt.as_ref() {
        if receipt.generation.trim().is_empty()
            || receipt.operation.deployment_generation_id != receipt.generation
        {
            journal.preserve_source_history()?;
            state.begin_source_history_hold();
            if credential_recovery::redacted_run_credentials(receipt) {
                tracing::warn!(
                    "historical PostgreSQL input was redacted; source history remains unavailable"
                );
            }
            // Retain the original bytes and real journal lease. The new Source
            // operation will publish its own identity after normal cleanup.
            *slot = Some(journal);
            return Err(
                credential_recovery::SourceHistoryProblem::InconsistentDeploymentIdentity.into(),
            );
        }
        state.set_generation(receipt.generation.clone());
    }
    // Keep the real journal lease until the first native business session opens
    // the same authority. The common OwnerGuard spans both steps.
    *slot = Some(journal);
    Ok(())
}

pub(super) async fn establish_startup_quiescence(
    state: &ServerState,
    supervised: bool,
    stop_owned: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    if !supervised {
        let guard = state
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
        if let Some(journal) = guard.as_ref() {
            journal.require_fresh_process_scope()?;
        }
    }
    stop_owned
        .await
        .context("confirm startup business process shutdown")?;
    let mut guard = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
    if let Some(journal) = guard.as_mut() {
        if let Err(error) = normalize_platform_generation_after_quiescence(state, journal) {
            tracing::error!(%error, "Legacy deployment identity remains unconfirmed; preserving its records and recovery protection");
            state.begin_runtime_recovery_hold();
        }
        journal.commit_coordinator()?;
    }
    Ok(())
}

fn normalize_platform_generation_after_quiescence(
    state: &ServerState,
    journal: &mut Journal,
) -> Result<()> {
    let Some(owner_workspace) = state.owner_execution_workspace.get() else {
        return Ok(());
    };
    // A recovery launch intentionally does not execute the env declaration, but
    // its immutable platform values can still prove the legacy identity map.
    if !crate::deploy::deploy_requested() {
        return Ok(());
    }
    let generation = match std::env::var(shared_types::APP_DEPLOY_GENERATION_ID) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return Ok(()),
    };
    let operation_id = std::env::var(shared_types::APP_DEPLOY_OPERATION_ID)
        .context("platform deployment operation identity is required for compatibility")?;
    let seed = crate::deploy::request_from_env()?;
    let target = journal
        .receipt
        .as_ref()
        .and_then(|receipt| receipt.active.as_ref())
        .and_then(|active| active.request.as_ref())
        .and_then(|request| request.execution_target);
    let workspace = resolved_execution_workspace(owner_workspace, target, state)?;
    journal.normalize_legacy_deployment_generation(
        &generation,
        &operation_id,
        &seed,
        &workspace,
    )?;
    Ok(())
}

pub(super) fn execution_workspace(
    owner: &std::path::Path,
    target: Option<ExecutionTarget>,
) -> std::path::PathBuf {
    match target {
        Some(ExecutionTarget::Source) => runtime_state_layout::canonical_project_root(owner),
        Some(ExecutionTarget::ProjectRun) => {
            runtime_state_layout::canonical_project_root(owner).join(".run")
        }
        None => owner.to_path_buf(),
    }
}

/// Resolve recorded local build provenance without relocating the owner's
/// journal, lock or token. A corrupt marker must never select another workspace.
pub(super) fn resolved_execution_workspace(
    owner: &std::path::Path,
    target: Option<ExecutionTarget>,
    state: &ServerState,
) -> Result<std::path::PathBuf> {
    if target.is_none() {
        return Ok(owner.to_path_buf());
    }
    let project = runtime_state_layout::resolve_project_origin(owner)
        .context("resolve execution project origin")?;
    if let Some(expected) = state.execution_project.get() {
        anyhow::ensure!(
            &project == expected,
            "owner project origin changed after initialization"
        );
    }
    Ok(execution_workspace(&project, target))
}

/// Restore the confirmed execution directory under the existing owner identity.
/// Unknown journal identity or target remains protected; credential updates do
/// not create a new owner or authorize replacement of this journal.
pub(super) fn restored_runtime_args(
    args: &RuntimeArgs,
    state: &ServerState,
) -> Result<RuntimeArgs> {
    restored_runtime_args_inner(args, state, true)
}

/// Upgrade a legacy directory binding only under the existing owner journal
/// lease and only when a unique directory contains the confirmed artifact.
pub(super) fn recover_legacy_execution_target(
    owner: &std::path::Path,
    state: &ServerState,
) -> Result<()> {
    let mut guard = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
    let Some(journal) = guard.as_mut() else {
        return Ok(());
    };
    let Some(mut receipt) = journal.receipt.clone() else {
        return Ok(());
    };
    if receipt.generation != state.generation_value()
        || !matches!(
            receipt.boundary,
            Boundary::Active
                | Boundary::RestoredActive
                | Boundary::StartupFailed
                | Boundary::Preparing
        )
    {
        return Ok(());
    }
    let Some(active) = receipt.active.as_mut() else {
        return Ok(());
    };
    let Some(request) = active.request.as_mut() else {
        return Ok(());
    };
    if request.execution_target.is_some() || request.local_path.is_none() {
        return Ok(());
    }
    let project = runtime_state_layout::resolve_project_origin(owner)?;
    if let Some(expected) = state.execution_project.get() {
        anyhow::ensure!(
            &project == expected,
            "owner project changed during legacy recovery"
        );
    }
    let mut selected = None;
    for target in [ExecutionTarget::Source, ExecutionTarget::ProjectRun] {
        let workspace = execution_workspace(&project, Some(target));
        if !workspace.join("release.lock.toml").try_exists()? {
            continue;
        }
        let release = crate::manifest::read_release_lock(&workspace)?;
        if release.release_id != active.artifact_release_id {
            continue;
        }
        if selected.is_some() {
            return Err(credential_recovery::SourceHistoryProblem::AmbiguousArtifact.into());
        }
        selected = Some(target);
    }
    let target = selected.ok_or(credential_recovery::SourceHistoryProblem::MissingArtifact)?;
    request.execution_target = Some(target);
    // Preparing/RestoredActive may point to a different failed attempt. Only
    // update its request when it is actually the same active request.
    if receipt.request.runtime_operation_id == request.runtime_operation_id
        && receipt.request.release_id == request.release_id
        && receipt.request.url == request.url
        && receipt.request.local_path == request.local_path
    {
        receipt.request.execution_target = Some(target);
    }
    journal
        .write(receipt)
        .context("persist recovered execution directory")
}

/// Resolving a confirmed directory is also needed by the idle control loop.
/// Durable credentials restore the confirmed business input. Historical
/// redaction is diagnostic only; current environment or application inputs may
/// still restore business. Identity and physical recovery checks remain intact.
pub(super) fn restored_runtime_args_inner(
    args: &RuntimeArgs,
    state: &ServerState,
    require_run_credentials: bool,
) -> Result<RuntimeArgs> {
    if let Err(error) = recover_legacy_execution_target(&args.workspace, state) {
        if error.is::<credential_recovery::SourceHistoryProblem>() {
            let guard = state
                .journal
                .lock()
                .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
            let journal = guard
                .as_ref()
                .context("historical deployment journal missing")?;
            journal.preserve_source_history()?;
            state.begin_source_history_hold();
            let missing_credentials = journal
                .receipt
                .as_ref()
                .is_some_and(credential_recovery::redacted_run_credentials);
            if missing_credentials {
                tracing::warn!(
                    "historical PostgreSQL input was redacted; source history remains unavailable"
                );
            }
        } else {
            state.begin_runtime_recovery_hold();
        }
        return Err(error);
    }
    let receipt = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
        .as_ref()
        .and_then(|journal| journal.receipt.clone());
    let mut restored = args.clone();
    if let Some(receipt) = receipt {
        if receipt.generation != state.generation_value() {
            if !env_deploy_requested(state) {
                // Automatic restoration cannot guess an older deployment, but
                // this mismatch is not evidence of a live writer. Native cleanup
                // and the kernel retain their independent execution protection.
                state
                    .journal
                    .lock()
                    .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
                    .as_ref()
                    .context("historical deployment journal missing")?
                    .preserve_source_history()?;
                state.begin_source_history_hold();
                if credential_recovery::redacted_run_credentials(&receipt) {
                    tracing::warn!(
                        "historical PostgreSQL input was redacted; deployment generation changed"
                    );
                }
                return Err(
                    credential_recovery::SourceHistoryProblem::DeploymentGenerationChanged.into(),
                );
            }
            // Only an explicit new env deployment may replace a prior generation;
            // initialize_startup must still validate/admit that deployment.
            return Ok(restored);
        }
        if let Some(request) = receipt
            .active
            .as_ref()
            .and_then(|active| active.request.as_ref())
        {
            // Old local-artifact receipts without a target cannot establish that
            // source/.run was selected safely. Keep recovery protection.
            if request.local_path.is_some() && request.execution_target.is_none() {
                state.begin_runtime_recovery_hold();
                anyhow::bail!(
                    "local artifact journal has no confirmed execution target; explicit recovery required"
                );
            }
            restored.workspace =
                resolved_execution_workspace(&args.workspace, request.execution_target, state)?;
            if let Some(target) = request.execution_target {
                state.set_pending_dev_profile(target == ExecutionTarget::Source);
            }
            if require_run_credentials && !env_deploy_requested(state) {
                let mut pending = state
                    .pending_run_config
                    .lock()
                    .map_err(|_| anyhow::anyhow!("pending run credential lock poisoned"))?;
                // A current operation's explicit input wins over historical
                // receipt data. Do not mutate or backfill the historical file.
                if let Some(pg) = pending.as_ref() {
                    shared_types::resolve_source_run_pg(Some(pg))?;
                } else {
                    *pending = credential_recovery::restored_run_pg(request.run_pg.as_ref())?;
                }
            }
        }
    }
    Ok(restored)
}

/// 统一 owner 会话的一次性部署声明门禁：非 fresh 会话（恢复式重启）不消费
/// APP_DEPLOY_*（等效进程模式在派生 worker 时剥除该组 env 的语义）。
/// legacy worker/直跑路径的 deploy_inputs_eligible 恒为 true，行为不变。
fn env_deploy_requested(state: &ServerState) -> bool {
    state.deploy_inputs_eligible() && crate::deploy::deploy_requested()
}

pub(super) async fn initialize_startup(
    args: &RuntimeArgs,
    state: &ServerState,
) -> Result<Option<InitialAction>> {
    if args.control_only {
        state.set_phase(ServerPhase::Idle);
        return Ok(None);
    }
    if !env_deploy_requested(state) && !automatic_deployment_recovery_allowed(state)? {
        state.set_phase(ServerPhase::Failed(
            "Deployment input was preserved after corruption; an explicit start or deployment is required".into(),
        ));
        state.ready.set_ready(false);
        return Ok(None);
    }
    let stopped_args = restored_runtime_args_inner(args, state, false)?;
    // Decide automatic recovery before generating Switching. Stopped must not
    // leave a false interrupted-switch journal when no business was started.
    if !env_deploy_requested(state)
        && state
            .runtime_kernel()
            .map(|kernel| kernel.store().load_desired())
            .transpose()?
            .is_some_and(|(desired, _)| desired == shared_types::DesiredState::Stopped)
    {
        state.set_phase(ServerPhase::Idle);
        return Ok(None);
    }
    let restored = restored_runtime_args(&stopped_args, state)?;
    let args = &restored;
    if env_deploy_requested(state) {
        let generation = std::env::var(shared_types::APP_DEPLOY_GENERATION_ID)
            .context("APP_DEPLOY_GENERATION_ID is required")?;
        anyhow::ensure!(
            !generation.trim().is_empty(),
            "APP_DEPLOY_GENERATION_ID is empty"
        );
    }
    let saved = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
        .as_ref()
        .context("deployment journal missing")?
        .receipt
        .clone()
        .filter(|receipt| receipt.generation == state.generation_value());
    if let Some(receipt) = saved.as_ref() {
        let mut status = state
            .deploy_status
            .write()
            .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))?;
        status.request_release_id = Some(receipt.operation.request_release_id.clone());
        status.error = receipt.operation.error.clone();
        status.operation = Some(receipt.operation.clone());
    }
    let resume = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
        .as_ref()
        .context("deployment journal missing")?
        .resume(&state.generation_value())
        .inspect_err(|_| state.begin_runtime_recovery_hold())?;
    if let Some(receipt) = resume {
        let release = crate::manifest::read_release_lock(&args.workspace)?;
        anyhow::ensure!(
            receipt
                .active
                .as_ref()
                .map(|active| active.artifact_release_id.as_str())
                == Some(release.release_id.as_str()),
            "active artifact does not match deployment journal"
        );
        if receipt.boundary == Boundary::StartupFailed {
            // Explicit recovery of the confirmed artifact. Spec semantics: a
            // completed stop is not a permanent disable — a deliberate restart
            // must bring the business back. The artifact identity was verified
            // before resume; a fresh process scope
            // proves the previous orchestration processes are gone, so this is
            // a new explicit attempt, not an in-process retry loop. The failed
            // operation itself stays failed in deploy status — recovery here
            // never rewrites that historical outcome. A same-scope restart
            // (app-cli crashed and the supervisor restarted it while the
            // container lived) cannot prove the old processes stopped: park in
            // Failed like before instead of erroring into a recovery hold.
            let fresh = state
                .journal
                .lock()
                .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
                .as_ref()
                .context("deployment journal missing")?
                .require_fresh_process_scope()
                .is_ok();
            if fresh {
                state.set_release(release);
                state.set_phase(ServerPhase::Orchestrating);
                return Ok(Some(InitialAction::Existing {
                    workspace: args.workspace.clone(),
                }));
            }
            state.set_release(release);
            state.set_phase(ServerPhase::Failed(
                receipt.operation.error.clone().unwrap_or_else(|| {
                    "Business startup failed; restart the container for an explicit start".into()
                }),
            ));
            return Ok(None);
        }
        if receipt.boundary == Boundary::Preparing
            && receipt.operation.phase != AppCliDeployPhase::Failed
        {
            state.fail_operation(
                "deployment preparation interrupted by restart".into(),
                Boundary::Preparing,
            )?;
        }
        state.set_release(release);
        // Reusing the confirmed artifact does not exchange directories. Keep
        // its existing boundary so a process stop cannot invent an interrupted
        // activation. MigrationJournal retains diagnostics and the execution lease.
        state.set_phase(ServerPhase::Orchestrating);
        return Ok(Some(InitialAction::Existing {
            workspace: args.workspace.clone(),
        }));
    }
    if env_deploy_requested(state) {
        let request = crate::deploy::request_from_env()?;
        let operation_id = std::env::var(shared_types::APP_DEPLOY_OPERATION_ID)
            .context("APP_DEPLOY_OPERATION_ID is required")?;
        anyhow::ensure!(
            !operation_id.trim().is_empty(),
            "APP_DEPLOY_OPERATION_ID is empty"
        );
        let admission = state
            .try_accept_deploy_with_id(request, operation_id)
            .map_err(anyhow::Error::msg)?;
        return initial_action_for_deployment_admission(state, admission).await;
    }
    if tokio::fs::try_exists(args.workspace.join("release.lock.toml")).await? {
        return Ok(Some(InitialAction::Existing {
            workspace: args.workspace.clone(),
        }));
    }
    Ok(None)
}

pub(super) async fn initial_action_for_deployment_admission(
    state: &ServerState,
    admission: DeployAdmission,
) -> Result<Option<InitialAction>> {
    match admission {
        DeployAdmission::Accepted => {
            let request = state
                .deploy_rx
                .lock()
                .await
                .try_recv()
                .context("accepted initial deployment was not queued")?;
            Ok(Some(InitialAction::Deploy(request)))
        }
        DeployAdmission::Replayed(operation) => {
            if operation.deployment_generation_id != state.generation_value() {
                state.begin_runtime_recovery_hold();
                anyhow::bail!(
                    "recorded deployment generation does not match this owner; an explicit new deployment is required"
                );
            }
            {
                let mut status = state
                    .deploy_status
                    .write()
                    .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))?;
                status.phase = operation.phase;
                status.error = operation.error.clone();
                status.request_release_id = Some(operation.request_release_id.clone());
                status.release_id = operation.artifact_release_id.clone();
                status.operation = Some(operation.clone());
            }
            if operation.persisted
                && operation.phase == AppCliDeployPhase::Failed
                && operation.deploy_stage != AppDeploymentStage::Pending
                && operation.recovery.as_ref().is_none_or(|recovery| {
                    matches!(recovery.status.as_str(), "restored" | "failed")
                })
            {
                state.set_phase(ServerPhase::Failed(operation.error.unwrap_or_else(|| {
                    "Recorded deployment failed; submit a new operation to deploy again".into()
                })));
                return Ok(None);
            }
            // Successful startup must have taken the matching confirmed journal
            // resume path above. A replay by itself never authorizes execution.
            state.begin_runtime_recovery_hold();
            anyhow::bail!(
                "recorded deployment {} has no resumable confirmed artifact; inspect its original outcome or submit an explicit new deployment",
                operation.operation_id
            )
        }
    }
}

fn automatic_deployment_recovery_allowed(state: &ServerState) -> Result<bool> {
    state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
        .as_ref()
        .context("deployment journal missing")?
        .automatic_recovery_allowed()
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExplicitRunObservation {
    Pending,
    Detached,
    Failure(String),
}

/// Observe only the operation created by this foreground run. Terminal states
/// in the kernel remain authoritative even when startup preflight failed before
/// the server-loop phase changed from Idle.
pub(super) async fn observe_explicit_run_failure(
    state: &ServerState,
    operation_id: &str,
) -> Result<ExplicitRunObservation> {
    let kernel = state
        .runtime_kernel()
        .context("runtime kernel unavailable for foreground observation")?;
    let view = kernel
        .get(operation_id)
        .await?
        .context("foreground operation record missing")?;
    anyhow::ensure!(
        view.operation_id == operation_id
            && view.runtime_instance_id == kernel.identity().runtime_instance_id,
        "foreground operation identity changed"
    );
    if matches!(
        view.state,
        shared_types::RuntimeOperationState::Succeeded
            | shared_types::RuntimeOperationState::Cancelled
    ) {
        return Ok(ExplicitRunObservation::Detached);
    }
    if matches!(
        view.state,
        shared_types::RuntimeOperationState::Failed
            | shared_types::RuntimeOperationState::RecoveryRequired
    ) {
        return Ok(
            match kernel
                .begin_foreground_failure_exit(operation_id, || state.trigger_cancel())
                .await?
            {
                Some(message) => ExplicitRunObservation::Failure(message),
                None => ExplicitRunObservation::Detached,
            },
        );
    }
    Ok(ExplicitRunObservation::Pending)
}

pub(super) async fn admit_explicit_run_replacement(
    args: &RuntimeArgs,
    state: &ServerState,
) -> Result<Option<String>> {
    let kernel = state
        .runtime_kernel()
        .context("runtime kernel unavailable for explicit run")?;
    if kernel.has_live_admission().await {
        return Ok(None);
    }
    let identity = kernel.identity();
    let (_, revision) = kernel.store().load_desired()?;
    let requested_workspace = std::fs::canonicalize(&args.workspace)
        .context("resolve explicitly requested run workspace")?;
    let artifact_workspace =
        runtime_state_layout::canonical_project_root(&requested_workspace).join(".run");
    let (kind, profile) = if requested_workspace == artifact_workspace {
        let release = crate::manifest::read_release_lock(&args.workspace)
            .context("read explicitly requested artifact identity")?;
        (
            shared_types::RuntimeOperationKind::Deploy,
            shared_types::RunProfileInput::Artifact {
                artifact: shared_types::ArtifactInput::ArtifactId {
                    artifact_id: release.release_id,
                },
            },
        )
    } else {
        (
            shared_types::RuntimeOperationKind::Start,
            shared_types::RunProfileInput::Source {
                workspace_id: identity.workspace_id.clone(),
            },
        )
    };
    let operation_id = format!("cli-recovery-{}", uuid::Uuid::new_v4().simple());
    let request = shared_types::RuntimeOperationRequest {
        operation_id: operation_id.clone(),
        expected_runtime_instance_id: identity.runtime_instance_id.clone(),
        expected_revision: revision,
        workspace_id: identity.workspace_id.clone(),
        kind,
        profile,
        run_config: None,
        request_context: None,
    };
    kernel
        .admit_with_control_hold(
            request,
            state.runtime_recovery_hold_active(),
            state.native_control_blocker(),
        )
        .await
        .map_err(|rejection| {
            anyhow::anyhow!(
                "explicit run replacement rejected: {}: {}",
                rejection.code,
                rejection.message
            )
        })?;
    Ok(Some(operation_id))
}
