use super::*;

/// serve 核心逻辑：所有权 → API bind → journal 恢复 → 状态机主循环。
pub(super) async fn serve_without_attach(args: &RuntimeArgs) -> Result<()> {
    anyhow::ensure!(
        !args.control_only || !crate::deploy::deploy_requested(),
        "control-only startup cannot execute an environment deployment"
    );
    // OwnerGuard：跨进程排他锁（cross-platform.md §3）——在 API bind 前获取，
    // 确保同一项目最多一个 owner。锁文件位于部署替换范围外的稳定状态根。
    let application_id = std::env::var("PROJECT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".to_string());
    let state_root =
        crate::runtime_kernel::RuntimeStore::resolve_root(&args.workspace, &application_id)?;
    let supervised_worker = runtime_supervisor::Worker::from_env(&state_root).await?;
    let stop_intent = supervised_worker
        .as_ref()
        .is_some_and(|worker| worker.intent() == runtime_supervisor::Intent::Stopped);
    let _owner_guard = if supervised_worker.is_some() {
        None
    } else {
        match crate::platform::owner_guard::OwnerGuard::try_acquire(&state_root)? {
            Some(guard) => Some(guard),
            None => {
                // A bootstrap race must never turn into a Start request to the winner.
                anyhow::ensure!(
                    !args.control_only,
                    "runtime owner is already starting or running"
                );
                anyhow::ensure!(
                    !crate::deploy::deploy_requested(),
                    "workspace already has an owner; submit artifact deployment through its deployment API"
                );
                return match crate::owner_dispatch::dispatch_to_owner(
                    &args.admin_addr,
                    &args.workspace,
                    &state_root,
                    &application_id,
                )
                .await?
                {
                    crate::owner_dispatch::OwnerDispatch::Terminal(view) => {
                        crate::owner_dispatch::describe_terminal(&view)
                    }
                    crate::owner_dispatch::OwnerDispatch::NoOwner => {
                        anyhow::bail!(
                            "owner disappeared during dispatch; no operation was submitted"
                        )
                    }
                };
            }
        }
    };

    // Hold the real public listener before legacy journal archival/migration.
    let api_listener = crate::api::bind_listener(&args.admin_addr).await?;
    let journal = if supervised_worker.is_some() {
        Journal::open_recovering(&args.workspace, state_root.clone(), true)?
    } else {
        Journal::open_with_root(&args.workspace, state_root.clone())?
    };
    let ready = RuntimeStatusService::default();
    let mut initial_state = ServerState::new(ready.clone());
    initial_state.initialize_owner_token()?;
    // A failed serve startup is a recovery failure, never legacy run mode.
    initial_state.mark_kernel_required();
    if std::env::var(shared_types::APP_DEPLOY_GENERATION_ID).is_err()
        && let Some(receipt) = journal.receipt.as_ref()
    {
        initial_state.generation = receipt.generation.clone();
    }
    let state = Arc::new(initial_state);
    *state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))? = Some(journal);

    // 管理 API 预绑定（P1-01）：bind 成功才继续部署清理/启动恢复/业务停启——
    // 端口冲突在一切运行态副作用之前 fail-fast（未 commit_coordinator、未
    // stop_all；journal 随 Drop 释放锁，不写 Quiescent）。恢复完成前写端点由
    // initializing 门控拒绝、/ready 以 initializing 摘流（探针早有人应答的价值
    // 保留——/health 恒 200 覆盖 kubelet liveness）。serve future 运行期故障 →
    // cancel 主循环受控收束，不留"无管理面的运行态"。
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
            api_monitor_state.cancel.cancel();
        }
    });
    let _worker_control = match supervised_worker {
        Some(worker) => Some(worker.serve(state.clone()).await?),
        None => None,
    };

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
                host.stop_all().await?;
            }
            crate::static_hosting::reconcile(&[], &args.workspace, false).await
        })
        .await
        .err();
    }
    let ownership_claimed = startup_error.is_none();
    // B05：内核装配**先于**启动决策——恢复裁决（损坏记录/未终态操作/desired
    // 读取）必须先于 initialize_startup，否则 Existing 自动启动在保护生效前
    // 已进入 first_request。
    let mut kernel_recovery_hold = false;
    if ownership_claimed {
        // R05：尝试装配即标记——失败（kernel=None）时写入口 fail-closed
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
                if let Some(receipt) = saved
                    .as_ref()
                    .filter(|receipt| receipt.boundary == Boundary::Switching)
                {
                    let reconciled = (|| -> Result<Receipt> {
                        anyhow::ensure!(
                            receipt.generation == state.generation,
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
                        crate::migration_journal::require_confirmed_migrations(&workspace)?;
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
                        crate::migration_journal::require_confirmed_migrations(&workspace)?;
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
                                if eligible && receipt.generation == state.generation {
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
                // R06 事件桥：编排 EVT 同步进入活跃操作的运行事件 journal
                //（复用 owner 的平台侧经运行 API 读事件流，读不到本进程
                // stdout）。stdout 通道不变（本地 spawn 路径消费）。
                {
                    let kernel = kernel.clone();
                    crate::orchestration_events::install_bridge(Box::new(move |json| {
                        if let Some(record) = bridge_event_fields(&json) {
                            kernel.append_orchestration_event(
                                &record.stage,
                                record.service,
                                &record.event_name,
                                record.payload,
                            );
                        }
                    }));
                }
                // endpoint 发现记录（cross-platform.md §3）：API 已绑定 +
                // 身份就绪后原子发布——多项目按状态根天然隔离。
                // 发布失败仅告警（发现能力缺失，不影响已建立的 owner）。
                if let Err(endpoint_error) =
                    kernel
                        .store()
                        .store_endpoint(&crate::runtime_kernel::EndpointRecord {
                            protocol_version: kernel.identity().protocol_version,
                            application_id: kernel.identity().application_id.clone(),
                            workspace_id: kernel.identity().workspace_id.clone(),
                            runtime_instance_id: kernel.identity().runtime_instance_id.clone(),
                            address: bound_api_addr.clone(),
                        })
                {
                    tracing::warn!("endpoint discovery record publish failed: {endpoint_error:#}");
                }
            }
            Err(error) => {
                // B05：状态根不可用 = 运行态可信状态不可读——不再仅关闭新 API
                // 放行旧链。fail-closed：压住自动启动；旧部署受理也会因
                // desired/记录不可读而不可信（deploy admission 检查
                // runtime_kernel 为 None 时见下述显式阻断）。
                kernel_recovery_hold = true;
                tracing::error!(
                    "runtime kernel unavailable ({error:#}); automatic business startup suppressed"
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
        let store = crate::runtime_kernel::RuntimeStore::open_with_root(
            state_root.clone(),
            &args.workspace,
        )?;
        let (_, revision) = store.load_desired()?;
        store.store_desired(
            shared_types::DesiredState::Stopped,
            revision
                .checked_add(1)
                .context("desired revision overflow")?,
        )?;
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
                    // Unknown identity/boundary is not a failed operation of
                    // this owner; preserve its original durable evidence.
                    state.begin_failure(
                        format!("startup recovery required: {error:#}"),
                        !state.credentials_only_hold(),
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
    // R03/B05：desired 读取先于启动决策；**读取失败同样压制自动启动**
    //（损坏 desired 等价于不可信状态——不允许"读错当 Running 继续起"）。
    // R05：读错时必须清除已生成的 first_request——否则自动恢复（journal
    // resume/卷上 release.lock 的 Existing 路径）仍会在不可信状态上启动。
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
            && matches!(first_request, Some(InitialAction::Existing))
        {
            // 用户 stop（spec §5）压制**自动恢复**（journal resume/卷上
            // release.lock 的 Existing 路径），保持 Idle；显式 env 部署
            //（Deploy 路径）是新的部署意图，不受 Stopped 压制。
            first_request = None;
            tracing::info!(
                "desired state is Stopped; automatic business recovery suppressed (staying Idle)"
            );
            state.set_phase(ServerPhase::Idle);
        }
    }

    // 启动恢复完成（含 Failed 相位——可再次部署修复的合法可查状态）：开放
    // 写端点受理与 /ready 判定。quiescence 失败路径不开放（进程保护现场至退出）。
    if ownership_claimed {
        state.mark_initialized();
    }

    // 服务托管引擎探测：supervisord socket 可用（容器形态）→ 动态 program 托管
    //（per-service 隔离重启）；否则 builtin（裸跑/dev，与 legacy 同引擎）。
    state.set_log_layout(if host.is_some() {
        LogLayout::Supervisord
    } else {
        LogLayout::Builtin
    });

    let result = if let Some(error) = startup_error {
        state.cancel.cancelled().await;
        Err(error).context("startup ownership was not claimed")
    } else {
        let supervised = host.is_some();
        let driver_args = args.clone();
        let driver_state = state.clone();
        let mut driver = tokio::spawn(async move {
            driver_state
                .supervision_driver_started
                .store(true, std::sync::atomic::Ordering::Release);
            // Poll challenges in the actual driver task, outside the business
            // operation queue. Yielding downloads/migrations stay healthy; a
            // blocked driver task or admission path cannot acknowledge them.
            let mut probes = driver_state.supervision_probe_rx.lock().await;
            let driver = server_loop(&driver_args, &driver_state, host, first_request);
            tokio::pin!(driver);
            loop {
                tokio::select! {
                    result = &mut driver => break result,
                    Some(reply) = probes.recv() => { let _sent = reply.send(()); }
                }
            }
        });
        let mut joined = false;
        let (driver_result, shutdown_deadline) = tokio::select! {
            result = &mut driver => {
                joined = true;
                state.close_admission();
                (result.context("server driver panicked").and_then(|result| result), tokio::time::Instant::now().checked_add(state.shutdown_budget(supervised)).context("shutdown budget exceeds clock range")?)
            },
            () = state.cancel.cancelled() => {
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
                finish_clean_shutdown(args, &state, ownership_claimed),
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
    // API 运行期故障（监控记录）：即使主循环已正常收尾也按失败退出——
    // 无管理面的实例不可宣称成功（P1-01）。正常关停路径 api task 被
    // abort，不会写入故障记录。
    if let Ok(slot) = api_failure.lock()
        && let Some(api_error) = slot.as_ref()
    {
        return Err(
            anyhow::anyhow!("app-cli management API terminated: {api_error}").context("serve"),
        );
    }
    result
}

pub(super) async fn finish_clean_shutdown(
    args: &RuntimeArgs,
    state: &ServerState,
    ownership_claimed: bool,
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
    // 干净关停清除 endpoint 发现记录（崩溃残留的旧记录由客户端核验拒绝）
    if let Some(kernel) = state.runtime_kernel() {
        kernel.store().clear_endpoint()?;
    }
    {
        let mut receiver = state.deploy_rx.lock().await;
        receiver.close();
        while receiver.try_recv().is_ok() {}
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
    let store = RuntimeStore::open_with_root(root, &args.workspace)?;
    let workspace_id = runtime_workspace_id(&args.workspace);
    let source_root = runtime_state_layout::canonical_project_root(&args.workspace)
        .to_string_lossy()
        .into_owned();
    let identity = store.load_or_init_identity(
        application_id,
        "userapp-dev".to_string(),
        workspace_id,
        source_root,
        state.generation.clone(),
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
            DispatchAction::OrchestrateSource { pg, .. } => {
                dispatch_state.can_supply_run_credentials(pg.as_ref(), true)
            }
            DispatchAction::DeployArtifact { pg, .. }
            | DispatchAction::DeployLocalArtifact { pg, .. } => {
                dispatch_state.can_supply_run_credentials(pg.as_ref(), false)
            }
            DispatchAction::StopBusiness { .. } => Ok(false),
        };
        let supplied_credentials = match credential_recovery {
            Ok(allowed) => allowed,
            Err(error) => {
                tracing::warn!(%error, "Credential recovery validation failed at dispatch");
                false
            }
        };
        if supplied_credentials && let Some(operation_id) = executing_id {
            // Kernel admission checked instance/revision and its separate
            // recovery fence. Never clear a concurrent unknown-state hold.
            if let Err(error) = dispatch_state.consume_credentials_hold(operation_id) {
                dispatch_state.begin_runtime_recovery_hold();
                tracing::warn!(%error, "Credential recovery handoff failed");
            }
        }
        if let Some(id) = executing_id
            && dispatch_state.runtime_recovery_hold_active()
        {
            // The loop remains alive for Stop, but must not turn missing
            // startup credentials or an uncertain journal into permission to
            // run a different request with environment defaults.
            let state = dispatch_state.clone();
            let id = id.clone();
            tokio::spawn(async move {
                if let Err(error) = state
                    .finish_runtime_operation_by_id(
                        &id,
                        shared_types::RuntimeOperationState::RecoveryRequired,
                        Some((
                            shared_types::ERR_RECOVERY_REQUIRED.into(),
                            "runtime recovery must be resolved before starting business".into(),
                        )),
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
                    tracing::error!(
                        "runtime dispatch: registered artifact {artifact_id} not found at {}",
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
                    tracing::error!("runtime dispatch: deploy channel closed ({operation_id})");
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
                    tracing::error!("runtime dispatch: deploy channel closed ({operation_id})");
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
                    let checked = crate::manifest::preflight_startup(&source, dev_profile).await;
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
                    if state
                        .control_tx
                        .send(ControlSignal::OrchestrateSource {
                            operation_id: operation_id.clone(),
                            dev_profile,
                            pg,
                        })
                        .is_err()
                    {
                        hold_unconfirmed(
                            &state,
                            format!("runtime control channel closed ({operation_id})"),
                        )
                        .await;
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
                    tracing::error!("runtime dispatch: control channel closed ({operation_id})");
                }
            }
        }
    });
    Ok(Arc::new(RuntimeKernel::new(store, identity, dispatch)))
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
        journal.commit_coordinator()?;
    }
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
    if receipt.generation != state.generation
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
        anyhow::ensure!(
            selected.is_none(),
            "multiple directories contain the legacy active artifact; explicit directory reconciliation required"
        );
        selected = Some(target);
    }
    let target = selected.context("legacy active artifact directory is missing")?;
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
/// Missing redacted credentials must suppress automatic business startup, not
/// prevent the owner from consuming Stop. This does not clear recovery holds
/// or authorize deployment; admission retains its existing checks.
pub(super) fn restored_runtime_args_inner(
    args: &RuntimeArgs,
    state: &ServerState,
    require_run_credentials: bool,
) -> Result<RuntimeArgs> {
    if let Err(error) = recover_legacy_execution_target(&args.workspace, state) {
        state.begin_runtime_recovery_hold();
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
        if receipt.generation != state.generation {
            if !crate::deploy::deploy_requested() {
                state.begin_runtime_recovery_hold();
                anyhow::bail!(
                    "deployment journal generation does not match this owner; explicit deployment required"
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
            if require_run_credentials
                && request
                    .run_pg
                    .as_ref()
                    .is_some_and(|pg| pg.password.is_empty())
                && !crate::deploy::deploy_requested()
            {
                state.begin_credentials_hold();
                anyhow::bail!(
                    "confirmed runtime requires explicit PostgreSQL credentials before restarting business"
                );
            }
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
        }
    }
    Ok(restored)
}

pub(super) async fn initialize_startup(
    args: &RuntimeArgs,
    state: &ServerState,
) -> Result<Option<InitialAction>> {
    if args.control_only {
        state.set_phase(ServerPhase::Idle);
        return Ok(None);
    }
    let stopped_args = restored_runtime_args_inner(args, state, false)?;
    // Decide automatic recovery before generating Switching. Stopped must not
    // leave a false interrupted-switch journal when no business was started.
    if !crate::deploy::deploy_requested()
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
    if let Err(error) = crate::migration_journal::require_confirmed_migrations(&args.workspace) {
        state.begin_runtime_recovery_hold();
        return Err(error).context("database migration requires reconciliation");
    }
    if crate::deploy::deploy_requested() {
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
        .filter(|receipt| receipt.generation == state.generation);
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
        .resume(&state.generation)
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
            // must bring the business back. The artifact identity and confirmed
            // migrations were verified before resume; a fresh process scope
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
                return Ok(Some(InitialAction::Existing));
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
        // activation. MigrationJournal independently fences unknown SQL work.
        state.set_phase(ServerPhase::Orchestrating);
        return Ok(Some(InitialAction::Existing));
    }
    if crate::deploy::deploy_requested() {
        let request = crate::deploy::request_from_env()?;
        let operation_id = std::env::var(shared_types::APP_DEPLOY_OPERATION_ID)
            .context("APP_DEPLOY_OPERATION_ID is required")?;
        anyhow::ensure!(
            !operation_id.trim().is_empty(),
            "APP_DEPLOY_OPERATION_ID is empty"
        );
        state
            .try_accept_deploy_with_id(request, operation_id)
            .map_err(anyhow::Error::msg)?;
        let request = state
            .deploy_rx
            .lock()
            .await
            .try_recv()
            .context("initial deployment was not queued")?;
        return Ok(Some(InitialAction::Deploy(request)));
    }
    if tokio::fs::try_exists(args.workspace.join("release.lock.toml")).await? {
        return Ok(Some(InitialAction::Existing));
    }
    Ok(None)
}
