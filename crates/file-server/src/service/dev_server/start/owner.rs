use super::*;

impl DevServerManager {
    /// P3-02：探测 3010 的既有 owner——三种结局：
    /// - `Ok(None)`：无人监听 → 调用方走本地 spawn（legacy 路径不变）；
    /// - `Ok(Some(started))`：匹配本 workspace 的 serve owner → 经运行 API
    ///   提交 Restart(source) 复用，登记 external DevProcess；
    /// - `Err`：legacy app-cli（无 runtime API）、foreign 应答、协议不兼容、
    ///   凭据缺失——明确诊断拒绝，不杀对方、不盲 spawn（XP04）。
    ///
    /// 复用提交的运行形态（R03）：Source = 平台已就绪的 workspace 直接编排；
    /// Artifact = 平台只登记制品（共享卷 builds/ zip），激活由 owner 在
    /// 身份/revision 核验后执行——提交拒绝不改变 active 运行目录。
    pub(crate) async fn reuse_or_refuse_owner(
        &self,
        project_id: &str,
        project_path: &Path,
        hooks: Option<crate::service::dev_server::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
        artifact_release_id: Option<&str>,
        request_context: Option<&str>,
    ) -> AppResult<Option<StartedDev>> {
        self.check_external_store().map_err(|error| {
            AppError::business(format!("external owner recovery required: {error:#}"))
        })?;
        let owner_addr = self.config.app_cli_admin_probe_addr.clone();
        let Some(crate::service::dev_server::owner_recovery::AvailableOwner {
            address: owner_addr,
            identity,
        }) = self
            .recover_owner_if_needed(project_id, project_path)
            .await?
        else {
            let persisted = self.read_external_state().map_err(|error| {
                AppError::business(format!("external recovery required: {error:#}"))
            })?;
            if persisted.owners.contains_key(project_id) {
                return Err(AppError::business(
                    "registered owner is unavailable; recovery required before spawning another owner",
                ));
            }

            // 无 runtime identity：区分"legacy app-cli 应答"与"无人监听"——
            // /v1/deploy/status 是 app-cli 专属路由（foreign 服务 404）。
            // DEV-1 §3.4：Legacy 只说明"不支持 runtime API"，不说明"没有
            // 进程"。可核验的本项目 run（发现阶段 binding 匹配且未 Stopped）
            // 交给停止路径收束（restart 的 stop 阶段同请求停止）；无法归属的
            // 外来监听保持拒绝（不猜杀）。
            if legacy_app_cli_responds(&owner_addr).await {
                let local_run_alive =
                    crate::service::dev_server::discovery::discover_targets(project_path)
                        .live_targets()
                        .next()
                        .is_some();
                if local_run_alive {
                    tracing::info!(
                        project_id,
                        "verified local run orchestrator without runtime API; routing through supervised stop",
                    );
                    return Ok(None);
                }
                return Err(AppError::business(
                    "admin port 3010 is held by a legacy app-cli process without the \
                     runtime API; stop it before starting a managed instance",
                ));
            }
            return Ok(None);
        };

        if !crate::service::dev_server::owner_client::protocol_compatible(&identity) {
            return Err(AppError::business(format!(
                "app-cli owner at 3010 speaks incompatible protocol v{} (expected v{}); \
                 upgrade it before reuse",
                identity.protocol_version,
                shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            )));
        }
        let app_id = std::env::var("PROJECT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown-app".to_string());
        crate::service::dev_server::owner_client::verify_project_identity(
            &identity,
            project_path,
            &app_id,
        )
        .map_err(|error| AppError::business(format!("owner identity rejected: {error:#}")))?;
        let expected_ws = identity.workspace_id.clone();
        crate::service::dev_server::startup_contract::require_owner_support(
            project_path,
            artifact_release_id,
            &identity.capabilities,
        )
        .await
        .map_err(|error| {
            AppError::business(format!(
                "startup contract rejected before owner restart: {error:#}"
            ))
        })?;

        // 匹配 owner：读凭据（源码/产物两种状态根落点都探测；owner 未
        // 启用写端点 → 无法路由，明确报错）
        let Some((_state_root, token)) = crate::service::dev_server::owner_client::find_owner_token(
            Path::new(&identity.source_root),
            &app_id,
        ) else {
            return Err(AppError::business(
                "workspace is already managed by an app-cli owner whose runtime API \
                 credentials are unavailable (APP_CLI_DEPLOY_TOKEN not enabled); \
                 stop it or restart it with the token enabled",
            ));
        };
        let client =
            crate::service::dev_server::owner_client::OwnerClient::new(&owner_addr, &token)
                .map_err(|error| AppError::system(format!("build owner client: {error:#}")))?;
        // P3-03：构建前捕获的期望优先（构建期间 owner 变更 → 提交被拒，
        // 不自动刷新重发）；未捕获（直接 start 无构建段）→ 提交时活取。
        let expected = lock(&self.owner_expectations)?.remove(project_id);
        // P3-03/R07：观察失败（构建期 owner 在但身份/凭据/状态读不到）→
        // 明确拒绝——不能用"没捕获到"刷新期望绕过停止屏障（R07 反例：
        // 预检断连 → 用户 Stop → 网络恢复 → 旧构建迟到提交必须被拒）。
        if let Some(crate::service::dev_server::types::OwnerExpectation::ObservationFailed {
            reason,
        }) = &expected
        {
            return Err(AppError::business(format!(
                "owner observation failed before this build ({reason}); refusing to \
                 submit without a verified admission context — stop intent must not \
                 be bypassed by a late build"
            )));
        }
        let captured = match expected {
            Some(crate::service::dev_server::types::OwnerExpectation::Captured {
                runtime_instance_id,
                revision,
            }) => Some((runtime_instance_id, revision)),
            _ => None,
        };
        let hooks_line_for_drain = hooks.as_ref().map(|hooks| hooks.on_line.clone());
        let route_restart = async {
            let (expected_instance, expected_revision) = match captured {
                Some(captured) => captured,
                None => {
                    let status = client.status().await?;
                    (identity.runtime_instance_id.clone(), status.revision)
                }
            };
            let request = shared_types::RuntimeOperationRequest {
                operation_id: format!("fs-restart-{}", uuid::Uuid::new_v4().simple()),
                expected_runtime_instance_id: expected_instance,
                expected_revision,
                workspace_id: expected_ws.clone(),
                kind: if artifact_release_id.is_some() {
                    shared_types::RuntimeOperationKind::Deploy
                } else {
                    shared_types::RuntimeOperationKind::Restart
                },
                profile: match artifact_release_id {
                    Some(id) => shared_types::RunProfileInput::Artifact {
                        artifact: shared_types::ArtifactInput::ArtifactId {
                            artifact_id: id.to_string(),
                        },
                    },
                    None => shared_types::RunProfileInput::Source {
                        workspace_id: expected_ws.clone(),
                    },
                },
                run_config: pg.map(|pg| shared_types::OperationRunConfig {
                    pg: Some(pg.clone()),
                }),
                request_context: request_context.map(str::to_owned),
            };
            let external = ExternalOwner {
                address: owner_addr.clone(),
                token: token.clone(),
                runtime_instance_id: identity.runtime_instance_id.clone(),
            };
            let intent =
                self.prepare_external_intent(project_id, project_path, &external, &request)?;
            let operation_id = intent.request.operation_id.clone();
            self.resume_external_intent(project_id, &client, &intent, &request)
                .await?;
            // R06：事件转发（游标重放轮询，替换 SSE 长连——无总超时/EOF 竞态，
            // 断线续传天然支持）
            let events_client =
                crate::service::dev_server::owner_client::OwnerClient::new(&owner_addr, &token)?;
            let hooks_line = hooks.as_ref().map(|hooks| hooks.on_line.clone());
            let stop_token = tokio_util::sync::CancellationToken::new();
            let cursor = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let stream_operation_id = operation_id.clone();
            let stream_task = tokio::spawn({
                let stop_token = stop_token.clone();
                let cursor = cursor.clone();
                async move {
                    let forward = move |json: String| {
                        if let Some(on_line) = hooks_line.as_ref() {
                            on_line(&json);
                        }
                    };
                    crate::service::dev_server::owner_client::forward_operation_events(
                        &events_client,
                        &stream_operation_id,
                        forward,
                        &stop_token,
                        &cursor,
                        Duration::from_millis(250),
                    )
                    .await;
                }
            });
            let terminal = client
                .wait_terminal(&operation_id, Duration::from_secs(180))
                .await;
            // R06：终态先停转发并 join（不 abort——游标一致无重复投递），
            // 再按游标排空剩余事件——终态事件在状态可见后才落 journal，
            // 盲目中止会丢平台的 Done 终局。
            stop_token.cancel();
            let _joined = stream_task.await;
            if terminal.is_ok()
                && let Some(on_line) = hooks_line_for_drain.clone()
            {
                let last = cursor.load(std::sync::atomic::Ordering::SeqCst);
                if let Err(error) =
                    crate::service::dev_server::owner_client::drain_operation_events(
                        &client,
                        &operation_id,
                        last,
                        |json| on_line(&json),
                    )
                    .await
                {
                    tracing::warn!("owner terminal event drain failed: {error:#}");
                }
            }
            let view = terminal?;
            crate::service::dev_server::external_store::verify_view(&view, &intent.request)?;
            if matches!(
                view.state,
                shared_types::RuntimeOperationState::Succeeded
                    | shared_types::RuntimeOperationState::Failed
                    | shared_types::RuntimeOperationState::Cancelled
            ) {
                self.finish_external_intent(project_id, &intent.request, false)?;
            }
            Ok::<_, anyhow::Error>(view)
        };
        let view = route_restart
            .await
            .map_err(|error| AppError::business(format!("reuse existing owner: {error:#}")))?;
        match view.state {
            shared_types::RuntimeOperationState::Succeeded => {}
            // R04：Cancelled 不等价成功——启动被取消既不证明编排成功也无运行
            // 证据，不登记 external（重试按新操作提交）。
            shared_types::RuntimeOperationState::Cancelled => {
                return Err(AppError::business(format!(
                    "owner restart was cancelled (operation {}); service start not confirmed",
                    view.operation_id
                )));
            }
            other => {
                return Err(AppError::business(format!(
                    "owner restart failed: {other:?} ({})",
                    view.error_message.as_deref().unwrap_or("no detail"),
                )));
            }
        }

        // Registration was published atomically before submission. Never republish it
        // after awaiting a response: a newer Stop may already have removed that registration.
        Ok(Some(StartedDev {
            pid: 0,
            port: shared_types::APP_ENTRY_PORT,
        }))
    }

    /// R03：制品态的 owner 路由决策——**激活权归属 owner**。
    /// - `Ok(Some(started))`：匹配 owner 已受理 Deploy(ArtifactId) 并确认
    ///   Succeeded（owner 侧完成校验/解压/激活/编排）；平台**不触碰 .run**。
    /// - `Ok(None)`：无 owner——调用方走本地激活 + spawn（legacy 路径）。
    /// - `Err`：owner 拒绝（身份/revision/凭据）——`.run` 保持原样（提交
    ///   拒绝不改变 active 内容），错误如实上抛。
    pub async fn route_artifact_restart(
        &self,
        project_id: &str,
        workspace: &Path,
        release_id: &str,
        hooks: Option<crate::service::dev_server::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
        request_context: Option<&str>,
    ) -> AppResult<Option<StartedDev>> {
        // owner（产物态 serve 绑定 {ws}/.run，workspace_id=".run"）
        let run_dir = workspace.join(".run");
        self.reuse_or_refuse_owner(
            project_id,
            &run_dir,
            hooks,
            pg,
            Some(release_id),
            request_context,
        )
        .await
    }
}
