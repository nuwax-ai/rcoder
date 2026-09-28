use super::*;

/// 一次编排/运行的执行档位：由调用形态决定（server 操作上下文显式传递 /
/// legacy 直跑兜底），沿 migrate→启动链原样传递。收拢三元组避免参数表
/// 膨胀（clippy too_many_arguments）。
#[derive(Clone, Debug, Default)]
pub struct RunProfile {
    /// 是否执行 [run].migrate 命令（server 形态按操作语义决定）。
    pub run_migrations: bool,
    /// dev 源码态信号（[devrun] 优先；见 [`dev_run_profile`]）。
    pub dev_profile: bool,
    /// R08 每操作 PG 凭据（owner 复用时平台传入；注入服务进程 env）。
    pub pg: Option<shared_types::StartPgCredential>,
}

/// run 前台会话的档位：migrate 恒执行、dev 由 env 信号、无操作级凭据。
pub fn legacy_run_profile() -> RunProfile {
    RunProfile {
        run_migrations: true,
        dev_profile: dev_run_profile(),
        pg: None,
    }
}

/// 前台编排入口：启动服务后持续监督到停止或退出，无外部取消源。
pub async fn run(args: &RuntimeArgs, runtime_status: RuntimeStatusService) -> Result<()> {
    // 直跑形态（无操作上下文）：env 兜底（R08 显式 profile/凭据仅经 server 形态）
    run_inner(args, runtime_status, None, None, legacy_run_profile()).await
}

/// 编排主入口（server 形态：`cancel` 触发 = 优雅停全部子服务后 Ok 返回，
/// 供热部署切换/容器 SIGTERM 级联停服；`on_running` 在编排完成进入 supervise
/// 时发送一次——server 据此把相位切到 Running）。
///
/// `pg`：R08 每操作 PG 凭据（owner 复用时平台传入）——注入服务进程 env
/// 的 POSTGRES_USER/PASSWORD（运行时变量 last-wins 覆盖 spec env 与进程
/// 透传值）；None 维持既有 env。
pub async fn run_with_cancel(
    args: RuntimeArgs,
    runtime_status: RuntimeStatusService,
    cancel: tokio_util::sync::CancellationToken,
    on_running: Option<tokio::sync::oneshot::Sender<()>>,
    profile: RunProfile,
) -> Result<()> {
    run_inner(&args, runtime_status, Some(cancel), on_running, profile).await
}

/// 等 SIGTERM 的可复用 future（server 主循环 select 消费；Unix handler 安装
/// 失败降级为永不完成，由其他分支兜底）。
pub(crate) async fn sigterm_watch() {
    wait_sigterm().await
}

pub(super) async fn run_inner(
    args: &RuntimeArgs,
    runtime_status: RuntimeStatusService,
    cancel: Option<tokio_util::sync::CancellationToken>,
    on_running: Option<tokio::sync::oneshot::Sender<()>>,
    profile: RunProfile,
) -> Result<()> {
    runtime_status.set_ready(false);
    let RunProfile {
        run_migrations,
        dev_profile,
        pg,
    } = profile;
    let pg = resolve_run_pg(pg)?;
    // 1. 自动发现子项目 + 组装服务清单
    let release = manifest::read_release_lock(&args.workspace).context("load release lock")?;
    validate_runtime_compatibility(&release)?;
    // 防御过滤：release.lock 正常不含 disabled 服务，此为防御手工篡改/未来锁语义变化；
    // 一处过滤覆盖后续 migrate/start/readiness/shutdown 全循环（对齐 log/service.rs 先例）。
    let specs: Vec<ServiceSpec> = release
        .services
        .iter()
        .filter(|service| service.enabled)
        .cloned()
        .collect();
    // Validate every service before migrations or process launch can mutate runtime.
    for spec in &specs {
        service_environment(spec, pg.as_ref())?;
    }

    // dev 形态编排信号：[devrun].command 优先、[run].command 兜底（源码态 dev 链路）。
    // R08：dev_profile 由调用方显式传递（server 形态 = 本次操作的 Source
    // 形态；直跑形态 = env 兜底）——不再在编排内部读 env
    if dev_profile {
        info!("🧪 dev run profile: services with [devrun] start via their dev command");
    }
    // workspace 首页静态服务判定（一次判定贯穿启动与拓扑汇总；与 pingap 兜底
    // 路由注入同源 workspace_index::index_port_if_eligible）。
    let workspace_index_port =
        crate::workspace_index::index_port_if_eligible(&args.workspace, &specs);

    // 2. Wait for the declared PostgreSQL dependency before starting services.
    // RCoder 容器通过 APP_CLI_REQUIRE_PG=1 启用；宿主机独立运行默认不探测。
    if workspace_needs_pg(&specs) {
        wait_for_pg(&specs, pg.as_ref(), cancel.as_ref()).await?;
    } else {
        info!("⏭  PostgreSQL preflight is not enabled by the runtime environment");
    }

    // 3. 各子项目 migrate → start；4. 编译验证并启动 Pingap。
    // 任一阶段失败统一兜底：先 shutdown_all 优雅停掉已启动的子进程再返回 Err。
    // tokio Child drop 默认不杀进程, 子进程又在独立进程组: 不清理会被 reparent 到
    // PID1 继续持端口, 外层 supervisor 重启 app-cli 后新实例同名服务 bind 冲突
    // → 永久 crash loop (只能重建容器恢复)。start_pingap 内部 config 确认失败
    // 路径已自行清理, 此处对已 take 空的集合再调 shutdown_all 幂等无害。
    let mut children: ManagedChildren = Vec::new();
    let mut started_user_services = 0usize;
    // 启动失败清单（容错语义：单服务 migrate/spawn/探测失败不阻塞其余服务；
    // pingap 失败仍整体 Err 全组清理——入口必需）
    let mut startup_failures: Vec<FailedService> = Vec::new();
    // Done 终局是否已发射（正常路径 :241；失败兜底路径据此保证"至多一次"）。
    let mut done_emitted = false;
    let mut checked_readiness = None;
    let startup = async {
        crate::static_hosting::reconcile(&specs, &args.workspace, dev_profile).await?;
        // ── 启动循环（容错）：单服务失败记 EVT 后 continue ──
        for spec in &specs {
            anyhow::ensure!(
                !cancel.as_ref().is_some_and(|token| token.is_cancelled()),
                "Orchestration cancelled"
            );
            // static 服务：内置静态托管承载（无进程，bind 即成恒成功；dev 源码态
            // 且配了 [devrun] 时端口让给 dev server——fallthrough 到正常 spawn）
            if crate::static_hosting::hosts_statically(spec, dev_profile) {
                emit_event(&OrchestrationEvent::ServiceStarting {
                    service: spec.service_id.clone(),
                });
                info!(
                    "Static host '{}' serving on :{}",
                    spec.service_id, spec.port
                );
                emit_event(&OrchestrationEvent::ServiceStartOk {
                    service: spec.service_id.clone(),
                });
                started_user_services += 1;
                continue;
            }
            // migrate（如有）—— per-service：失败=该服务跳过（不再全局 fail-fast；
            // 迁移错误即启动失败原因，EVT 带原始错误链）。
            if run_migrations && !spec.run.migrate.is_empty() {
                info!("🛠️  migrate {}", spec.service_id);
                if let Err(e) =
                    run_migration_with_receipt(spec, &release, &args.workspace, pg.as_ref()).await
                {
                    let error = format!("migrate {}: {e:#}", spec.service_id);
                    warn!("⚠️  {error} — 跳过该服务，继续启动其余服务");
                    emit_event(&OrchestrationEvent::ServiceStartFail {
                        service: spec.service_id.clone(),
                        error: error.clone(),
                    });
                    startup_failures.push(FailedService {
                        service: spec.service_id.clone(),
                        error,
                    });
                    continue;
                }
            }
            // start（dev 形态下 [devrun].command 优先、[run].command 兜底）
            let argv = effective_run_argv(spec, dev_profile);
            if argv.is_empty() {
                warn!(
                    "⚠️  {} 无启动 command（[run]/[devrun]），跳过",
                    spec.service_id
                );
                continue;
            }
            crate::orchestration_events::emit(
                &crate::orchestration_events::OrchestrationEvent::ServiceStarting {
                    service: spec.service_id.clone(),
                },
            );
            let launched_at = tokio::time::Instant::now();
            match start_service(
                spec,
                argv,
                &args.workspace,
                &args.log_dir,
                &release.release_id,
                pg.as_ref(),
            )
            .await
            {
                Ok(mut child) => {
                    // Observe explicit contracts as soon as this execution starts.
                    // A later service's slow migration must not consume this one's
                    // budget before its first observation. Legacy probes remain parallel.
                    if spec.health.startup_probe.is_some() {
                        let check = crate::startup_probe::builtin(
                            spec,
                            &mut child,
                            dev_profile,
                            launched_at,
                            cancel.as_ref(),
                        )
                        .await;
                        record_startup_check(&spec.service_id, check, &mut startup_failures);
                    }
                    children.push((spec.service_id.clone(), child));
                    started_user_services += 1;
                }
                Err(e) => {
                    let error = format!("spawn {}: {e:#}", spec.service_id);
                    warn!("⚠️  {error} — 跳过该服务，继续启动其余服务");
                    emit_event(&OrchestrationEvent::ServiceStartFail {
                        service: spec.service_id.clone(),
                        error: error.clone(),
                    });
                    startup_failures.push(FailedService {
                        service: spec.service_id.clone(),
                        error,
                    });
                    continue;
                }
            }
        }
        // workspace 首页静态服务（幂等：热部署重编排不二次 bind；常驻 app-cli
        // 进程生命周期，实时读文件无需随 code 换入重启）。
        if workspace_index_port.is_some() {
            crate::workspace_index::ensure_spawned(&args.workspace)?;
            info!(
                "📄 workspace index (index.html) serving on :{}",
                crate::workspace_index::INDEX_PORT
            );
        }
        // Borrow owned children for parallel checks: no detached probe tasks and
        // no JoinError branch that could silently omit a service failure.
        let checks = children
            .iter_mut()
            .filter(|(id, _)| {
                specs
                    .iter()
                    .any(|spec| &spec.service_id == id && spec.health.startup_probe.is_none())
            })
            .map(|(service_id, child)| {
                let specs = &specs;
                let cancel = cancel.as_ref();
                async move {
                    let result = async {
                        let spec = specs
                            .iter()
                            .find(|s| &s.service_id == service_id)
                            .context("started service is absent from release lock")?;
                        crate::startup_probe::builtin(
                            spec,
                            child,
                            dev_profile,
                            tokio::time::Instant::now(),
                            cancel,
                        )
                        .await
                    }
                    .await;
                    (service_id.clone(), result)
                }
            });
        for (service_id, result) in futures::future::join_all(checks).await {
            record_startup_check(&service_id, result, &mut startup_failures);
        }
        anyhow::ensure!(
            !cancel.as_ref().is_some_and(|token| token.is_cancelled()),
            "Orchestration cancelled"
        );
        // 编译、完整验证并启动 Pingap；代理失败时 workspace 不得进入 ready。
        start_pingap(
            &args.workspace,
            &args.log_dir,
            &args.pingap_bin,
            &release,
            &mut children,
            dev_profile,
        )
        .await?;
        // Explicit startup contracts cover the complete startup commit, including
        // bridge observation. Do not emit a successful terminal before this wait.
        if specs.iter().any(|spec| spec.health.startup_probe.is_some()) {
            checked_readiness = Some(wait_for_bridge(&release, &specs, cancel.as_ref()).await?);
        }
        // A worker may have exited while another service or Pingap was starting.
        // Recheck the very same owned execution before emitting the operation terminal.
        for (id, child) in &mut children {
            if specs
                .iter()
                .any(|s| &s.service_id == id && s.health.startup_probe.is_some())
                && !startup_failures.iter().any(|f| &f.service == id)
                && let Err(error) = crate::startup_probe::root_alive(child)
            {
                let error = format!("startup commit check: {error:#}");
                emit_event(&OrchestrationEvent::ServiceStartFail {
                    service: id.clone(),
                    error: error.clone(),
                });
                startup_failures.push(FailedService {
                    service: id.clone(),
                    error,
                });
            }
        }
        anyhow::ensure!(
            !cancel.as_ref().is_some_and(|token| token.is_cancelled()),
            "Orchestration cancelled"
        );
        anyhow::ensure!(started_user_services > 0, "no service started");
        // 启动编排终局（pingap 确认后输出——9080 listen 即全部启动判定完成，
        // 下游终态判定无竞态）：failed 空 = 全部成功。
        emit_event(&OrchestrationEvent::OrchestrationDone {
            failed: startup_failures.clone(),
        });
        done_emitted = true;
        if on_running.is_some() && !startup_failures.is_empty() {
            anyhow::bail!(
                "deployment startup failed: {}",
                startup_failures
                    .iter()
                    .map(|failure| format!("{}: {}", failure.service, failure.error))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        if !startup_failures.is_empty() {
            warn!(
                "⚠️  启动编排完成（部分失败 {} 项，其余服务正常运行）",
                startup_failures.len()
            );
        }
        Ok(())
    };
    if let Err(mut error) = startup.await {
        error!("❌ startup failed, shutting down already-started children: {error:#}");
        // P1-02：清理错误并入 cause 链，不覆盖原始启动错误（组合后仍可溯因）。
        if let Err(cleanup_error) = shutdown_all(std::mem::take(&mut children), 5).await {
            error = error.context(format!(
                "cleanup after startup failure unconfirmed: {cleanup_error:#}"
            ));
        }
        // P1-02 失败终局事件：旧版 startup 块内 `?` 路径（reconcile /
        // ensure_spawned / start_pingap）静默跳过 Done——平台侧只能等满窗口
        // 超时。此处统一补发一次（未发过时），保留已有服务失败清单；编排器
        // 自身阶段错误以独立 service 条目上报（含清理未确认信息）。
        if !done_emitted {
            let mut failed = startup_failures.clone();
            failed.push(FailedService {
                service: ORCHESTRATOR_FAILURE_SERVICE.to_string(),
                error: format!("{error:#}"),
            });
            emit_event(&OrchestrationEvent::OrchestrationDone { failed });
        }
        return Err(error);
    }

    // 运行拓扑汇总：service_id → port → 路由一张表。日志目录、pingap upstream/路由
    // 均按 service_id 命名，排查从启动日志直接反查，无需另读 effective config。
    let started_ids: std::collections::BTreeSet<&str> =
        children.iter().map(|(name, _)| name.as_str()).collect();
    info!("🔌 运行拓扑 entrypoint=http://0.0.0.0:{PINGAP_PORT}:");
    for spec in &specs {
        let route = spec
            .proxy
            .as_ref()
            .map(|proxy| format!("route={} (strip_prefix={})", proxy.path, proxy.strip_prefix))
            .unwrap_or_else(|| "internal (无 [proxy])".into());
        let failed_ids: std::collections::BTreeSet<&str> = startup_failures
            .iter()
            .map(|failure| failure.service.as_str())
            .collect();
        let state = if failed_ids.contains(spec.service_id.as_str()) {
            "failed (启动失败，详见事件流/日志)"
        } else if started_ids.contains(spec.service_id.as_str()) {
            if dev_profile && spec.devrun.is_some() {
                "running (devrun)"
            } else {
                "running"
            }
        } else if crate::static_hosting::hosts_statically(spec, dev_profile) {
            // static 服务无进程（不在 children）——内置托管承载
            "running (static host)"
        } else {
            "skipped (无启动 command)"
        };
        info!(
            "🔌   {} port={} {state} {route}",
            spec.service_id, spec.port
        );
    }
    if workspace_index_port.is_some() {
        info!(
            "🔌   workspace-index port={} running route=/ (兜底 index.html)",
            crate::workspace_index::INDEX_PORT
        );
    }

    // 5. readiness —— 默认不强依赖后端 app(用户核心诉求:后端有 bug 起不来时容器仍 ready、可排查)。
    //   - 无 [health].bridge_service:app-cli 自给自足,初始化完成即 ready。
    //   - 有 bridge_service:只等那一个后端的 readiness_path;超时 → 保持 NotReady(摘流)
    //     但不 bail/崩溃(liveness /health 仍 200,容器活着,用户可 exec 进去排查)。
    // 防御过滤:bridge 查找仅在已过滤的 specs(enabled 服务)中进行,disabled 服务
    // 即便被手工写入 bridge_service 也不会被等待(走 warn 默认 ready 分支)。
    let ready = match checked_readiness {
        Some(ready) => ready,
        None => match wait_for_bridge(&release, &specs, cancel.as_ref()).await {
            Ok(ready) => ready,
            Err(error) => {
                shutdown_all(std::mem::take(&mut children), 5)
                    .await
                    .context("cleanup after bridge observation interrupted")?;
                return Err(error);
            }
        },
    };
    runtime_status.set_ready(ready);

    // 5. supervise（阻塞直到任一退出或信号或外部取消）
    info!(
        "✅ all services started, supervising {} process(es)",
        children.len()
    );
    if let Some(notify) = on_running {
        let _ = notify.send(());
    }
    let shutdown_timeout = specs
        .iter()
        .map(|service| service.run.shutdown_timeout_seconds)
        .max()
        .unwrap_or(30);
    let known_failed = startup_failures
        .iter()
        .map(|failure| failure.service.clone())
        .collect::<Vec<_>>();
    supervise(children, shutdown_timeout, cancel, known_failed).await?;
    runtime_status.set_ready(false);
    Ok(())
}

pub(super) fn record_startup_check(
    service_id: &str,
    result: Result<()>,
    failures: &mut Vec<FailedService>,
) {
    match result {
        Ok(()) => {
            info!("✅ {service_id} startup check passed");
            emit_event(&OrchestrationEvent::ServiceStartOk {
                service: service_id.into(),
            });
        }
        Err(error) => {
            let error = format!("startup check: {error:#}");
            warn!("{service_id}: {error}");
            emit_event(&OrchestrationEvent::ServiceStartFail {
                service: service_id.into(),
                error: error.clone(),
            });
            failures.push(FailedService {
                service: service_id.into(),
                error,
            });
        }
    }
}

pub(super) async fn wait_for_bridge(
    release: &workspace_manifest::ReleaseLock,
    specs: &[ServiceSpec],
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<bool> {
    Ok(match &release.bridge_service {
        None => true,
        Some(bridge_id) => match specs.iter().find(|s| &s.service_id == bridge_id) {
            None => {
                warn!(
                    "⚠️  [health].bridge_service '{bridge_id}' not in release services; \
                     defaulting to ready"
                );
                true
            }
            Some(spec) => match tokio::select! {
                result = wait_for_service_ready(spec) => result,
                () = async { match cancel { Some(token) => token.cancelled().await, None => std::future::pending().await } } => anyhow::bail!("Orchestration cancelled"),
            } {
                Ok(()) => {
                    info!("✅ bridge service '{bridge_id}' ready");
                    true
                }
                Err(e) => {
                    warn!(
                        "⏳ bridge service '{bridge_id}' not ready: {e}; \
                         staying NotReady (traffic withheld, liveness unaffected)"
                    );
                    false
                }
            },
        },
    })
}

pub(crate) fn validate_runtime_compatibility(
    release: &workspace_manifest::ReleaseLock,
) -> Result<()> {
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .context("parse current app-cli version")?;
    let minimum = semver::Version::parse(&release.minimum_app_cli_version)
        .context("parse minimum app-cli version from release lock")?;
    if current < minimum {
        anyhow::bail!("release requires app-cli >= {minimum}, current version is {current}");
    }
    // 运行时身份(pingap 版本/commit、镜像 digest)仅记录日志,不做硬校验:
    // pingap 向前兼容(新版接受旧配置),且 app-runtime 镜像与 pingap 会随升级变动,
    // 硬性相等比对会阻塞容器启动 → 无法平滑升级。此处打印 release.lock 与运行时实际值
    // 供日志追溯;mismatch / 缺失仅 warn,不阻断启动。
    for (name, locked) in [
        ("RCODER_PINGAP_VERSION", release.pingap.version.as_str()),
        ("RCODER_PINGAP_COMMIT", release.pingap.commit.as_str()),
        (
            "RCODER_RUNTIME_IMAGE_DIGEST",
            release.runtime_image_digest.as_str(),
        ),
    ] {
        match std::env::var(name) {
            Ok(runtime) => {
                if runtime == locked {
                    info!("{name}: release={locked} runtime={runtime} (matched)");
                } else {
                    warn!(
                        "{name} mismatch (non-fatal, will not block startup): release={locked}, runtime={runtime}"
                    );
                }
            }
            Err(_) => warn!("{name} not set in runtime (non-fatal): release={locked}"),
        }
    }
    Ok(())
}

/// Capture credentials once per orchestration, before launching any command.
/// Managed container env overrides artifact defaults; request credentials win.
pub(crate) fn resolve_run_pg(
    supplied: Option<shared_types::StartPgCredential>,
) -> Result<Option<shared_types::StartPgCredential>> {
    if supplied.is_some() {
        return Ok(supplied);
    }
    let user = std::env::var("POSTGRES_USER");
    let password = std::env::var("POSTGRES_PASSWORD");
    match (user, password) {
        (Ok(username), Ok(password)) => {
            Ok(Some(shared_types::StartPgCredential { username, password }))
        }
        (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => Ok(None),
        _ => anyhow::bail!(
            "Runtime PostgreSQL credentials must provide both POSTGRES_USER and POSTGRES_PASSWORD as Unicode"
        ),
    }
}

pub(crate) fn service_environment(
    spec: &ServiceSpec,
    pg: Option<&shared_types::StartPgCredential>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut env: std::collections::BTreeMap<_, _> = spec.env.clone().into_iter().collect();
    if let Some(pg) = pg {
        env.insert("POSTGRES_USER".into(), pg.username.clone());
        env.insert("POSTGRES_PASSWORD".into(), pg.password.clone());
        let database_url = match env.get("DATABASE_URL") {
            Some(value) => Some(value.clone()),
            None => match std::env::var("DATABASE_URL") {
                Ok(value) => Some(value),
                Err(std::env::VarError::NotPresent) => None,
                Err(_) => anyhow::bail!("DATABASE_URL must be valid Unicode"),
            },
        };
        if let Some(database_url) = database_url.filter(|value| !value.is_empty()) {
            env.insert(
                "DATABASE_URL".into(),
                database_url_with_credentials(&database_url, pg)?,
            );
        }
    }
    Ok(env)
}

pub(super) fn database_url_with_credentials(
    value: &str,
    pg: &shared_types::StartPgCredential,
) -> Result<String> {
    // Parse errors deliberately exclude the original URL, which may contain secrets.
    let mut url = reqwest::Url::parse(value)
        .map_err(|_| anyhow::anyhow!("Runtime DATABASE_URL is invalid"))?;
    if !matches!(url.scheme(), "postgres" | "postgresql") || url.host_str().is_none() {
        anyhow::bail!("Runtime DATABASE_URL must be a PostgreSQL connection URL");
    }
    // URL userinfo setters preserve percent signs; encode literal percent first
    // so a password such as "%40" does not become "@" at the database driver.
    url.set_username(&pg.username.replace('%', "%25"))
        .map_err(|_| anyhow::anyhow!("Cannot set DATABASE_URL username"))?;
    url.set_password(Some(&pg.password.replace('%', "%25")))
        .map_err(|_| anyhow::anyhow!("Cannot set DATABASE_URL password"))?;
    // libpq/node-postgres URI query fields can override authority credentials.
    // Remove every encoded or repeated user/password override, preserving all
    // unrelated query bytes (not form-decoding literal '+' into a space).
    if let Some(query) = url.query() {
        let mut retained = Vec::new();
        for pair in query.split('&') {
            let key = pair.split_once('=').map_or(pair, |(key, _)| key);
            if !matches!(decode_pg_uri_component(key)?.as_str(), "user" | "password") {
                retained.push(pair);
            }
        }
        let query = retained.join("&");
        url.set_query((!query.is_empty()).then_some(query.as_str()));
    }
    Ok(url.into())
}
