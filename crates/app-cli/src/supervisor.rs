//! 子项目 + pingap 编排核心：wait PG → migrate → start 子项目 → spawn pingap → supervise。
//!
//! 替代 workspace start.sh。由 main.rs 调用 `run(&args)`，前台阻塞直到任一子进程退出或收到信号，
//! 然后 kill 所有子进程 + return → supervisor [program:app] 感知退出 → 整组重启。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::RuntimeArgs;
use crate::manifest::{self, ServiceSpec};
use crate::orchestration_events::{FailedService, OrchestrationEvent, emit as emit_event};
use crate::platform::process_tree::{ManagedChild, StopOutcome, spawn_managed};
use crate::proxy::admin_probe;
use crate::proxy::compiler::compile_and_validate;
use crate::proxy::pingap::PINGAP_PORT;
use crate::runtime_status::RuntimeStatusService;

/// supervisor 全部真实子进程（业务服务 / migrate / pingap）统一走受管进程树：
/// Unix 进程组 / Windows Job Object（R01——spawn 前归属无逃逸窗口，停止收束整树）。
type ManagedChildren = Vec<(String, ManagedChild)>;

/// 失败终局 Done 中编排器自身条目的 service 名（区别于用户服务；平台侧
/// failed 清单按条目映射进任务失败汇总，编排阶段错误不再依赖超时兜底）。
pub(crate) const ORCHESTRATOR_FAILURE_SERVICE: &str = "orchestrator";

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

/// legacy 直跑形态的档位：migrate 恒执行、dev 由 env 信号、无操作级凭据。
pub fn legacy_run_profile() -> RunProfile {
    RunProfile {
        run_migrations: true,
        dev_profile: dev_run_profile(),
        pg: None,
    }
}

/// 编排主入口（legacy 直跑形态：一次性编排，无外部取消源）。
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

async fn run_inner(
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
    let startup = async {
        crate::static_hosting::reconcile(&specs, &args.workspace, dev_profile).await?;
        // ── 启动循环（容错）：单服务失败记 EVT 后 continue ──
        for spec in &specs {
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
                Ok(child) => {
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
        // ── 并行 readiness 探测：spawn 成功的服务在各自 [health].
        //    startup_timeout_seconds 窗口内探测（500ms 间隔）：
        //    - 进程态（[run]）：HTTP readiness_path 轮询（K8s readinessProbe 语义）
        //    - devrun 启动的热加载服务：**TCP 连通**即可——vite/nodemon 等 dev
        //      server 的 HTTP 路径语义不可知（多数不实现 readiness_path，按 HTTP
        //      探会误判超时）；"端口在听"是热加载命令的保守正确判定
        //    探测超时的服务**保留运行**（部分运行态：可能仅慢启动或探针路径配错，
        //    杀掉武断；supervise 循环继续管理其退出重启）。结果逐服务 emit（dev
        //    链路 SSE 可见）。
        let mut probe_tasks = Vec::new();
        for (service_id, _) in &children {
            let Some(spec) = specs.iter().find(|s| &s.service_id == service_id) else {
                continue;
            };
            let spec = spec.clone();
            probe_tasks.push(tokio::spawn(async move {
                let outcome = if dev_profile && spec.devrun.is_some() {
                    wait_for_port_open(&spec, spec.health.startup_timeout_seconds).await
                } else {
                    wait_for_service_ready_within(&spec, spec.health.startup_timeout_seconds).await
                };
                (spec.service_id.clone(), outcome.err())
            }));
        }
        for task in probe_tasks {
            let Ok((service_id, failure)) = task.await else {
                continue;
            };
            match failure {
                None => {
                    info!("✅ {service_id} ready (readiness probe passed)");
                    emit_event(&OrchestrationEvent::ServiceStartOk {
                        service: service_id,
                    });
                }
                Some(e) => {
                    let error = format!("readiness probe: {e:#}");
                    warn!("⏳ {service_id} {error} — 服务保留运行（启动判定失败）");
                    emit_event(&OrchestrationEvent::ServiceStartFail {
                        service: service_id.clone(),
                        error: error.clone(),
                    });
                    startup_failures.push(FailedService {
                        service: service_id,
                        error,
                    });
                }
            }
        }
        // 编译、完整验证并启动 Pingap；代理失败时 workspace 不得进入 ready。
        start_pingap(
            &args.workspace,
            &args.log_dir,
            &args.pingap_bin,
            &release,
            &mut children,
        )
        .await?;
        // 启动编排终局（pingap 确认后输出——9080 listen 即全部启动判定完成，
        // 下游终态判定无竞态）：failed 空 = 全部成功。
        emit_event(&OrchestrationEvent::OrchestrationDone {
            failed: startup_failures.clone(),
        });
        done_emitted = true;
        if on_running.is_some() && !startup_failures.is_empty() {
            anyhow::bail!(
                "deployment startup failed for {} service(s)",
                startup_failures.len()
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
    let ready = match &release.bridge_service {
        None => true,
        Some(bridge_id) => match specs.iter().find(|s| &s.service_id == bridge_id) {
            None => {
                warn!(
                    "⚠️  [health].bridge_service '{bridge_id}' not in release services; \
                     defaulting to ready"
                );
                true
            }
            Some(spec) => match wait_for_service_ready(spec).await {
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
    };
    runtime_status.set_ready(ready);

    // 守卫语义: 所有用户服务都因空 [run].command 被跳过时应 fail。不能用
    // children.is_empty() 判断 —— start_pingap 已无条件 push pingap, 恒非空。
    // 失败路径同样先清理 (此时 children 里至少有 pingap), 与 startup 失败兜底一致。
    if started_user_services == 0 {
        error!("❌ no service started, shutting down already-started children");
        shutdown_all(std::mem::take(&mut children), 5).await?;
        anyhow::bail!("no service started");
    }

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

fn database_url_with_credentials(
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

// ── PG 等待 ──────────────────────────────────────────────────────────────────

/// PostgreSQL preflight is an execution-environment policy supplied by RCoder.
/// Standalone app-cli does not infer a local database from migrations or templates.
pub(crate) fn workspace_needs_pg(_specs: &[ServiceSpec]) -> bool {
    pg_required_by_policy(std::env::var_os("APP_CLI_REQUIRE_PG").as_deref())
}

/// [`workspace_needs_pg`] 的纯谓词（供测试直测，不动进程 env）：
/// 平台声明（builder 形态注入 APP_CLI_REQUIRE_PG=1）才探测；其余值/缺失
/// 均不探测——含声明了 migrate 的服务（e39591126 起 PG 预检为环境策略，
/// 不再由服务清单推断）。
pub(crate) fn pg_required_by_policy(declared: Option<&std::ffi::OsStr>) -> bool {
    declared == Some(std::ffi::OsStr::new("1"))
}

/// Probe the same database and credentials that migration commands consume.
/// A listening PostgreSQL server can still be creating POSTGRES_DB asynchronously;
/// pg_isready cannot establish database existence or successful authentication.
pub(crate) async fn wait_for_pg(
    specs: &[ServiceSpec],
    pg: Option<&shared_types::StartPgCredential>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    if std::env::var_os("APP_CLI_SKIP_PG_WAIT").is_some() {
        warn!("APP_CLI_SKIP_PG_WAIT set; skipping PostgreSQL readiness check (dev only)");
        return Ok(());
    }
    let mut targets = Vec::new();
    let forced = std::env::var_os("APP_CLI_REQUIRE_PG").is_some_and(|value| value == "1");
    for spec in specs
        .iter()
        .filter(|spec| spec.enabled && (forced || !spec.run.migrate.is_empty()))
    {
        targets.push(pg_probe_environment(service_environment(spec, pg)?)?);
    }
    // Explicit APP_CLI_REQUIRE_PG without migrations uses the same runtime defaults.
    if targets.is_empty() {
        let mut environment = std::collections::BTreeMap::new();
        if let Some(pg) = pg {
            environment.insert("POSTGRES_USER".into(), pg.username.clone());
            environment.insert("POSTGRES_PASSWORD".into(), pg.password.clone());
            if let Ok(url) = std::env::var("DATABASE_URL") {
                environment.insert(
                    "DATABASE_URL".into(),
                    database_url_with_credentials(&url, pg)?,
                );
            }
        }
        targets.push(pg_probe_environment(environment)?);
    }
    wait_for_pg_targets(
        "psql",
        &targets,
        Duration::from_secs(60),
        Duration::from_secs(2),
        cancel,
    )
    .await
}

fn pg_probe_environment(
    overrides: std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let value = |name: &str, default: &str| -> Result<String> {
        if let Some(value) = overrides.get(name) {
            return Ok(if value.is_empty() {
                default.into()
            } else {
                value.clone()
            });
        }
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => Ok(value),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(default.into()),
            Err(_) => anyhow::bail!("PostgreSQL runtime environment must be valid Unicode"),
        }
    };
    let url = value("DATABASE_URL", "")?;
    let mut target = std::collections::BTreeMap::new();
    // Match child command inheritance plus per-service overrides for libpq TLS
    // and session settings; URI fields below have higher precedence.
    for name in [
        "PGSSLMODE",
        "PGSSLCERT",
        "PGSSLKEY",
        "PGSSLROOTCERT",
        "PGSSLCRL",
        "PGSSLCRLDIR",
        "PGSSLSNI",
        "PGSSLMINPROTOCOLVERSION",
        "PGSSLMAXPROTOCOLVERSION",
        "PGCHANNELBINDING",
        "PGGSSENCMODE",
        "PGKRBSRVNAME",
        "PGGSSLIB",
        "PGTARGETSESSIONATTRS",
        "PGOPTIONS",
        "PGAPPNAME",
        "PGCLIENTENCODING",
    ] {
        let configured = value(name, "")?;
        if !configured.is_empty() {
            target.insert(name.into(), configured);
        }
    }
    if !url.is_empty() {
        let parsed = reqwest::Url::parse(&url)
            .map_err(|_| anyhow::anyhow!("Runtime DATABASE_URL is invalid"))?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "postgres" | "postgresql"),
            "Runtime DATABASE_URL must use PostgreSQL"
        );
        // PGDATABASE is a literal database name, not libpq's expandable `dbname`
        // argument. Decode the URI into libpq environment settings so credentials
        // never enter argv. Reject unsupported options instead of probing another
        // target silently.
        if let Some(host) = parsed.host_str() {
            target.insert(
                "PGHOST".into(),
                host.trim_start_matches('[').trim_end_matches(']').into(),
            );
        }
        if let Some(port) = parsed.port() {
            target.insert("PGPORT".into(), port.to_string());
        }
        if !parsed.username().is_empty() {
            target.insert("PGUSER".into(), decode_pg_uri_component(parsed.username())?);
        }
        if let Some(password) = parsed.password() {
            target.insert("PGPASSWORD".into(), decode_pg_uri_component(password)?);
        }
        if let Some(database) = parsed
            .path()
            .strip_prefix('/')
            .filter(|value| !value.is_empty())
        {
            target.insert("PGDATABASE".into(), decode_pg_uri_component(database)?);
        }
        if let Some(query) = parsed.query() {
            for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                let (name, value) = pair
                    .split_once('=')
                    .context("Invalid PostgreSQL URI option")?;
                let name = decode_pg_uri_component(name)?;
                let value = decode_pg_uri_component(value)?;
                let variable = match name.as_str() {
                    "host" => "PGHOST",
                    "hostaddr" => "PGHOSTADDR",
                    "port" => "PGPORT",
                    "user" => "PGUSER",
                    "password" => "PGPASSWORD",
                    "dbname" => "PGDATABASE",
                    "sslmode" => "PGSSLMODE",
                    "sslcert" => "PGSSLCERT",
                    "sslkey" => "PGSSLKEY",
                    "sslrootcert" => "PGSSLROOTCERT",
                    "sslcrl" => "PGSSLCRL",
                    "sslcrldir" => "PGSSLCRLDIR",
                    "sslsni" => "PGSSLSNI",
                    "ssl_min_protocol_version" => "PGSSLMINPROTOCOLVERSION",
                    "ssl_max_protocol_version" => "PGSSLMAXPROTOCOLVERSION",
                    "channel_binding" => "PGCHANNELBINDING",
                    "gssencmode" => "PGGSSENCMODE",
                    "krbsrvname" => "PGKRBSRVNAME",
                    "gsslib" => "PGGSSLIB",
                    "target_session_attrs" => "PGTARGETSESSIONATTRS",
                    "options" => "PGOPTIONS",
                    "application_name" => "PGAPPNAME",
                    "client_encoding" => "PGCLIENTENCODING",
                    "connect_timeout" => "PGCONNECT_TIMEOUT",
                    // Older PostgreSQL URI clients accept ssl=true as require.
                    "ssl" if value == "true" => {
                        target.insert("PGSSLMODE".into(), "require".into());
                        continue;
                    }
                    _ => anyhow::bail!("Unsupported PostgreSQL URI option for startup probe"),
                };
                target.insert(variable.into(), value);
            }
        }
    } else {
        target.insert("PGHOST".into(), value("PGHOST", "localhost")?);
        target.insert("PGPORT".into(), value("PGPORT", "5432")?);
        target.insert("PGUSER".into(), value("POSTGRES_USER", "dev")?);
        target.insert("PGPASSWORD".into(), value("POSTGRES_PASSWORD", "dev")?);
        target.insert("PGDATABASE".into(), value("POSTGRES_DB", "dev")?);
    }
    Ok(target)
}

fn decode_pg_uri_component(value: &str) -> Result<String> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes
                .next()
                .and_then(|value| char::from(value).to_digit(16));
            let low = bytes
                .next()
                .and_then(|value| char::from(value).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                anyhow::bail!("Invalid PostgreSQL URI percent encoding");
            };
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    anyhow::ensure!(!decoded.contains(&0), "PostgreSQL URI contains a null byte");
    String::from_utf8(decoded).map_err(|_| anyhow::anyhow!("PostgreSQL URI must be valid Unicode"))
}

async fn wait_for_pg_targets(
    program: &str,
    targets: &[std::collections::BTreeMap<String, String>],
    budget: Duration,
    interval: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    let never_cancelled = tokio_util::sync::CancellationToken::new();
    let cancel = cancel.unwrap_or(&never_cancelled);
    for target in targets {
        loop {
            anyhow::ensure!(
                !cancel.is_cancelled(),
                "PostgreSQL readiness wait cancelled"
            );
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "PostgreSQL target database is not accessible within the startup budget"
            );
            let mut command = Command::new(program);
            command
                .args([
                    "-X",
                    "--no-password",
                    "-qAt",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-c",
                    "SELECT 1",
                ])
                .env_remove("PGHOSTADDR")
                .env_remove("PGSERVICE")
                .envs(target)
                .env("PGCONNECT_TIMEOUT", "2")
                .env(
                    "PGOPTIONS",
                    format!(
                        "{} -c statement_timeout=2000",
                        target.get("PGOPTIONS").map(String::as_str).unwrap_or("")
                    ),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // Both engines may be dropped by their owning control future.
                // psql is invoked directly (no shell or background descendants).
                .kill_on_drop(true);
            let mut child = command
                .spawn()
                .context("spawn PostgreSQL target database probe")?;
            let attempt_deadline =
                deadline.min(tokio::time::Instant::now() + Duration::from_secs(3));
            let outcome = tokio::select! {
                _ = cancel.cancelled() => None,
                result = tokio::time::timeout_at(attempt_deadline, child.wait()) => Some(result),
            };
            match outcome {
                Some(Ok(Ok(status))) if status.success() => {
                    anyhow::ensure!(
                        !cancel.is_cancelled(),
                        "PostgreSQL readiness wait cancelled"
                    );
                    break;
                }
                Some(Ok(Ok(_))) => {}
                Some(Ok(Err(error))) => {
                    return Err(error).context("wait PostgreSQL target database probe");
                }
                _ => {
                    // kill() waits/reaps as well. Never start a migration after
                    // cancellation or an unconfirmed child cleanup.
                    tokio::time::timeout(Duration::from_secs(2), child.kill())
                        .await
                        .context("PostgreSQL probe cleanup timed out")?
                        .context("stop PostgreSQL target database probe")?;
                    anyhow::ensure!(
                        !cancel.is_cancelled(),
                        "PostgreSQL readiness wait cancelled"
                    );
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => anyhow::bail!("PostgreSQL readiness wait cancelled"),
                _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + interval)) => {},
            }
        }
    }
    anyhow::ensure!(
        !cancel.is_cancelled(),
        "PostgreSQL readiness wait cancelled"
    );
    info!("PostgreSQL target database login and SELECT 1 confirmed");
    Ok(())
}

/// 轮询单个桥接后端的 readiness_path 直至就绪(120s 超时)。
///
/// 仅在 workspace.manifest `[health].bridge_service` 显式配置时调用(只等那一个后端)。
/// 默认(不配 bridge)不调本函数 —— app-cli 自给 /ready,不强依赖任何后端。
pub(crate) async fn wait_for_service_ready(spec: &ServiceSpec) -> Result<()> {
    wait_for_service_ready_within(spec, 120).await
}

/// 在窗口（秒）内轮询端口 TCP 连通（devrun 热加载命令的就绪判定——HTTP 路径
/// 语义对 dev server 不可知，端口在听即就绪）。
pub(crate) async fn wait_for_port_open(spec: &ServiceSpec, timeout_secs: u64) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", spec.port)).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "port {} not open within {} seconds (last connect: {e})",
                        spec.port,
                        timeout_secs
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 在给定窗口（秒）内轮询 readiness_path 至 2xx；超时返回 Err（含路径信息）。
/// bridge_service 等待（固定 120s）与启动逐服务探测（`[health].
/// startup_timeout_seconds`）共用同一探测核心。
pub(crate) async fn wait_for_service_ready_within(
    spec: &ServiceSpec,
    timeout_secs: u64,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .context("build readiness HTTP client")?;
    let url = format!(
        "http://127.0.0.1:{}{}",
        spec.port, spec.health.readiness_path
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let ready = client
            .get(&url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "readiness '{}' not ready within {} seconds (last probe: {})",
                url,
                timeout_secs,
                "no 2xx",
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── 子项目启动 ─────────────────────────────────────────────────────────────────

/// 启动一个子项目（[run].command + PORT/HOSTNAME env + stdout/stderr → 轮转日志）。
/// dev 形态编排信号（源码态 dev 链路）：平台 dev server spawn 本进程时注入
/// `APP_CLI_RUN_PROFILE=dev`。仅影响启动命令选择（[devrun] 优先、[run] 兜底），
/// 端口注入/pingap/健康检查/拓扑与生产编排完全一致；未注入（生产 serve、
/// 本地直跑）恒走 [run]——与既有行为逐字节一致。
pub fn dev_run_profile() -> bool {
    std::env::var("APP_CLI_RUN_PROFILE").as_deref() == Ok("dev")
}

/// 服务的生效启动命令：dev 形态且配置了 [devrun] 时用 devrun.command（热加载，
/// 跑源码），否则 [run].command。（[devbuild] 的回落在平台侧 dev 链路执行，
/// app-cli 不消费该字段。）
pub(crate) fn effective_run_argv(spec: &ServiceSpec, dev_profile: bool) -> &[String] {
    if dev_profile && let Some(devrun) = &spec.devrun {
        return &devrun.command;
    }
    &spec.run.command
}

async fn start_service(
    spec: &ServiceSpec,
    argv: &[String],
    ws_root: &Path,
    log_dir: &Path,
    release_id: &str,
    pg: Option<&shared_types::StartPgCredential>,
) -> Result<ManagedChild> {
    let cwd = ws_root.join(&spec.dir);
    let service_log_dir = log_dir.join(&spec.service_id);
    tokio::fs::create_dir_all(&service_log_dir)
        .await
        .with_context(|| format!("create service log dir {}", service_log_dir.display()))?;
    let out_path = service_log_dir.join("runtime.out.log");
    let err_path = service_log_dir.join("runtime.err.log");

    // R01：真实业务进程统一受管进程树 spawn（Unix 进程组 / Windows Job
    // Object）——npm/pnpm shell 启动的 node 子孙全部归属，停止收束整树。
    let mut cmd = Command::new(crate::win_cmd::resolve_spawn_program(&argv[0]));
    cmd.args(&argv[1..])
        .current_dir(&cwd)
        .envs(service_environment(spec, pg)?)
        // Runtime-owned variables are applied last so even a hand-crafted
        // release lock cannot override service identity, paths, or ports.
        .env("HOSTNAME", "0.0.0.0")
        .env("PORT", spec.port.to_string())
        .env("APP_LOG_DIR", &service_log_dir)
        .env("APP_SERVICE_ID", &spec.service_id)
        .env("APP_RELEASE_ID", release_id);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = spawn_managed(cmd)
        .with_context(|| format!("spawn {}: {}", spec.service_id, argv.join(" ")))?;

    // pipe → 带轮转的日志文件（append 模式，不 truncate；超 10MB rotate，保留 3 份）
    if let Some(stdout) = child.take_stdout() {
        let p = out_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(
            stdout, p, None, None,
        ));
    }
    if let Some(stderr) = child.take_stderr() {
        let p = err_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(
            stderr, p, None, None,
        ));
    }

    info!(
        "🚀 start {} ({}) on :{} (pid={})",
        spec.service_id,
        spec.name,
        spec.port,
        child.id().unwrap_or(0)
    );
    Ok(child)
}

/// 运行一个临时命令（migrate），等它结束后返回。
///
/// 捕获 stdout/stderr（不 `Stdio::null()` 丢弃）：成功走 `info!`，失败带 stderr 返回错误，
/// 便于排障（Fail Fast：暴露而非吞掉）。
pub(crate) async fn run_migration_with_receipt(
    spec: &ServiceSpec,
    release: &manifest::ReleaseLock,
    workspace: &Path,
    pg: Option<&shared_types::StartPgCredential>,
) -> Result<()> {
    run_migration_with_receipt_cancel(spec, release, workspace, pg, None).await
}

pub(crate) async fn run_migration_with_receipt_cancel(
    spec: &ServiceSpec,
    release: &manifest::ReleaseLock,
    workspace: &Path,
    pg: Option<&shared_types::StartPgCredential>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    anyhow::ensure!(
        !cancel.is_some_and(|token| token.is_cancelled()),
        "Migration cancelled before dispatch"
    );
    let environment = service_environment(spec, pg)?;
    let directory = workspace.join(&spec.dir);
    validate_transient_input(&spec.run.migrate, &directory)?;
    let identity = crate::migration_journal::identity(release, &spec.service_id)?;
    let Some(receipt) = crate::migration_journal::MigrationJournal::begin(workspace, identity)?
    else {
        info!(service = %spec.service_id, "Migration already confirmed for this artifact");
        return Ok(());
    };
    if let Some(cancel) = cancel {
        run_transient_cancellable(
            &spec.run.migrate,
            &directory,
            &environment,
            Duration::from_secs(300),
            Some(cancel),
        )
        .await?;
    } else {
        run_transient_with_env(&spec.run.migrate, &directory, &environment).await?;
    }
    receipt
        .complete()
        .context("persist confirmed database migration")
}

pub(crate) async fn run_transient_with_env(
    argv: &[String],
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    run_transient_with_environment_and_timeout(argv, cwd, env, Duration::from_secs(300)).await
}

#[cfg(test)]
async fn run_transient_with_timeout(argv: &[String], cwd: &Path, timeout: Duration) -> Result<()> {
    run_transient_with_environment_and_timeout(argv, cwd, &Default::default(), timeout).await
}

async fn run_transient_with_environment_and_timeout(
    argv: &[String],
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
) -> Result<()> {
    run_transient_cancellable(argv, cwd, env, timeout, None).await
}

/// Reject local input failures before creating an uncertain migration receipt.
/// This does not prove spawn will succeed; failures after dispatch remain fenced.
fn validate_transient_input(argv: &[String], cwd: &Path) -> Result<()> {
    let program = argv.first().context("migration command is empty")?;
    anyhow::ensure!(!program.trim().is_empty(), "migration executable is empty");
    anyhow::ensure!(
        argv.iter().all(|argument| !argument.contains('\0')),
        "migration command contains a NUL byte"
    );
    let metadata = std::fs::metadata(cwd)
        .with_context(|| format!("inspect migration working directory {}", cwd.display()))?;
    anyhow::ensure!(
        metadata.is_dir(),
        "migration working directory is not a directory"
    );
    Ok(())
}

async fn run_transient_cancellable(
    argv: &[String],
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    validate_transient_input(argv, cwd)?;
    let program = argv.first().context("migration command is empty")?;
    // R01：migrate 同样走受管进程树——shell 包装的迁移命令 spawn 的子孙
    // （工作进程/DB writer）整树归属，超时与收尾均收束确认。
    let mut cmd = Command::new(crate::win_cmd::resolve_spawn_program(program));
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        spawn_managed(cmd).with_context(|| format!("spawn migrate: {}", argv.join(" ")))?;

    // 并发 drain stdout/stderr：防 pipe 被写满阻塞 + 捕获失败原因
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let out_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut buf).await;
        }
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut buf).await;
        }
        buf
    });

    // 只等根进程退出（root 语义）：root 正常退出后仍可能有后台孙进程在写
    // （迁移 shell `cmd &` 后台化），超时与收尾判定不能被树级 wait 阻塞。
    let outcome = tokio::select! {
        result = tokio::time::timeout(timeout, child.wait_root()) => Some(result),
        () = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        } => None,
    };
    // A shell may exit successfully while descendants still execute migrations.
    // Confirm the original tree is stopped (and gone) before allowing recovery
    // or success — stop(ZERO) 对已退出的 root 直通，对残留孙进程 TERM→KILL
    // 并整树收束确认。
    let stop_outcome = child.stop(Duration::ZERO).await;
    if stop_outcome == StopOutcome::Unconfirmed {
        out_task.abort();
        err_task.abort();
        return Err(ShutdownUnconfirmed(
            "Migration process tree shutdown was not confirmed".into(),
        )
        .into());
    }
    let status = match outcome {
        Some(Ok(status)) => status.context("wait migrate")?,
        None => {
            out_task.abort();
            err_task.abort();
            anyhow::bail!("Migration cancelled; database outcome requires reconciliation");
        }
        Some(Err(_)) => {
            out_task.abort();
            err_task.abort();
            anyhow::bail!(
                "migrate timed out after {}ms: {}",
                timeout.as_millis(),
                argv.join(" ")
            );
        }
    };
    let out = out_task.await.unwrap_or_default();
    let err = err_task.await.unwrap_or_default();

    if !out.trim().is_empty() {
        info!("migrate stdout:\n{out}");
    }
    if !status.success() {
        if !err.trim().is_empty() {
            warn!("migrate stderr:\n{err}");
        }
        anyhow::bail!("migrate exited {status}");
    }
    Ok(())
}

// ── pingap 启动 ─────────────────────────────────────────────────────────────────

/// 编译用户权威配置到只读运行目录，`pingap -t` 成功后再启动，
/// 并经 loopback admin 只读通道确认初始配置实际生效。
async fn start_pingap(
    ws_root: &Path,
    log_root: &Path,
    pingap_bin: &Path,
    release: &workspace_manifest::ReleaseLock,
    children: &mut ManagedChildren,
) -> Result<()> {
    // N02：运行目录默认不假设容器 /run（原生 Windows/macOS 不可写）——
    // env 显式优先（平台注入容器布局），缺省挂 log_dir 子目录（用户可写、
    // 稳定、非系统临时目录；配置每次启动重生成，随日志卷持久无害）。
    let runtime_root = std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| log_root.join("pingap"));
    let outcome = compile_and_validate(ws_root, &runtime_root, pingap_bin, release).await?;
    info!(
        "📝 effective pingap config → {}",
        outcome.config_path.display()
    );

    // admin 仅启用为 loopback 只读确认通道；TOML 仍是唯一配置权威；永不通过 admin 写配置。
    // 凭证每次启动随机生成，经 env 注入（不进命令行，避免 ps 泄露；不落盘不进日志）。
    let admin_addr = format!("127.0.0.1:{}", admin_probe::admin_port());
    let endpoint = admin_probe::register_admin_endpoint(
        admin_addr,
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );

    // R01：pingap 受管进程树 spawn（与业务服务同一停止/收束链）。
    let mut cmd = Command::new(pingap_bin);
    cmd.arg("-c")
        .arg(&outcome.config_path)
        .arg("--autoreload")
        // pingap 的 env override 规则：`get_from_env(key)` 读 `PINGAP_{key}` 全大写
        //（pingap src/main.rs parse_arguments 的闭包），故必须用 PINGAP_ADMIN_* 而非 admin_*。
        // 凭证经 env 注入（不进命令行避免 ps 泄露、不落盘不进日志），admin 仅 loopback 只读。
        .env("PINGAP_ADMIN_ADDR", &endpoint.addr)
        .env("PINGAP_ADMIN_USER", &endpoint.user)
        .env("PINGAP_ADMIN_PASSWORD", &endpoint.password);
    let child = spawn_managed(cmd).context("spawn pingap")?;
    info!(
        "🚀 start pingap on :{} (pid={})",
        PINGAP_PORT,
        child.id().unwrap_or(0)
    );
    children.push(("pingap".into(), child));

    // 初始确认：pingap 必须真正加载当前配置（config_hash 匹配），否则视为启动失败，
    // 返回 Err 触发 supervisor 整组重启语义；失败前优雅停止已启动的子进程避免残留。
    //
    // 本地开发逃生开关 APP_CLI_SKIP_PINGAP_CONFIRM：跳过 admin probe 确认（pingap 仍以
    // --autoreload 启动；配置正确性已由 `pingap -t` 语法校验 + 实际 curl 验证兜底）。生产不设。
    if std::env::var_os("APP_CLI_SKIP_PINGAP_CONFIRM").is_some() {
        warn!(
            "⏭  APP_CLI_SKIP_PINGAP_CONFIRM set; skipping initial pingap config confirmation (dev only)"
        );
    } else if let Err(error) = admin_probe::wait_for_config_hash(
        endpoint,
        &outcome.expected_hash,
        admin_probe::CONFIRM_BUDGET,
    )
    .await
    {
        error!("❌ pingap initial config confirmation failed: {error:#}");
        shutdown_all(std::mem::take(children), 5).await?;
        return Err(error).context("confirm initial Pingap config via loopback admin probe");
    } else {
        info!("✅ pingap initial config confirmed (config_hash matched)");
    }
    Ok(())
}

// ── supervise（信号 + 任一退出 → kill all → return）─────────────────────────────

/// 优雅停机：所有受管子进程并发停止（信号 → 共享宽限 deadline → 整树强杀），
/// 任何一个进程树收束未确认即整体失败（R01：停止完成要求整个受管树收束，
/// 不能只等直接 Child）。
async fn supervise(
    mut children: ManagedChildren,
    shutdown_timeout_seconds: u64,
    cancel: Option<tokio_util::sync::CancellationToken>,
    known_failed: Vec<String>,
) -> Result<()> {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("📡 received SIGINT, shutting down");
        }
        _ = wait_sigterm() => {
            info!("📡 received SIGTERM, shutting down");
        }
        // server 形态的外部取消（热部署切换 / 容器停服级联）：与信号同路径优雅停
        () = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending().await,
            }
        } => {
            info!("📡 orchestration cancelled (hot deploy / shutdown), stopping services");
        }
        exited = poll_any_exit(&mut children, &known_failed) => {
            if let Some(name) = exited {
                error!("❌ {name} exited — shutting down (supervisor will restart)");
            }
        }
    }
    shutdown_all(children, shutdown_timeout_seconds).await
}

/// A deployment cannot publish a terminal status until shutdown is confirmed.
#[derive(Debug)]
pub(crate) struct ShutdownUnconfirmed(pub String);
impl std::fmt::Display for ShutdownUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shutdown not confirmed: {}", self.0)
    }
}
impl std::error::Error for ShutdownUnconfirmed {}

async fn shutdown_all(children: ManagedChildren, shutdown_timeout_seconds: u64) -> Result<()> {
    // 并发停止：每个 stop 的首个 poll 即发出信号，宽限窗口共享同一时刻起算
    //（与旧实现"先全体 TERM、再并行等宽限、超时全体 KILL"等时序）。
    let grace = Duration::from_secs(shutdown_timeout_seconds);
    let results =
        futures::future::join_all(children.into_iter().map(|(name, mut child)| async move {
            let outcome = child.stop(grace).await;
            (name, outcome)
        }))
        .await;
    let unconfirmed: Vec<String> = results
        .into_iter()
        .filter_map(|(name, outcome)| match outcome {
            StopOutcome::Unconfirmed => Some(name),
            _ => None,
        })
        .collect();
    if unconfirmed.is_empty() {
        Ok(())
    } else {
        Err(ShutdownUnconfirmed(format!(
            "process tree(s) remain after termination deadline: {}",
            unconfirmed.join(", ")
        ))
        .into())
    }
}

/// Detect an unexpected child exit without discarding handles or tree
/// identities: final shutdown still has to confirm every original tree stopped.
/// root 语义（孙进程存活不掩盖根进程死亡，见 [`ManagedChild::try_wait_root`]）。
async fn poll_any_exit(
    children: &mut [(String, ManagedChild)],
    known_failed: &[String],
) -> Option<String> {
    loop {
        for (name, child) in children.iter_mut() {
            if matches!(child.try_wait_root(), Ok(Some(_)) | Err(_))
                && !known_failed.iter().any(|failed| failed == name)
            {
                return Some(name.clone());
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 等 SIGTERM（Unix 专属）。handler 安装失败时降级（不 panic），
/// 由 [`tokio::signal::ctrl_c`] / `poll_any_exit` 兜底触发关闭。
#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sig = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!("install SIGTERM handler failed: {e} — SIGTERM 不会被捕获");
            return;
        }
    };
    sig.recv().await;
}

#[cfg(not(unix))]
async fn wait_sigterm() {
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_manifest::{DevrunSection, RunSection};

    /// 最小 ServiceSpec（LockedService）：只填启动命令相关字段。
    fn spec_with(devrun: Option<Vec<&str>>) -> ServiceSpec {
        ServiceSpec {
            service_id: "frontend".into(),
            name: "Frontend".into(),
            dir: "frontend".into(),
            r#type: workspace_manifest::ProjectType::Node,
            kind: workspace_manifest::ProjectKind::Web,
            enabled: true,
            port: 4578,
            devbuild: None,
            run: RunSection {
                command: vec!["node".into(), "server.js".into()],
                migrate: Vec::new(),
                depends_on: Vec::new(),
                shutdown_timeout_seconds: 30,
            },
            devrun: devrun.map(|command| DevrunSection {
                command: command.into_iter().map(String::from).collect(),
            }),
            static_content_dir: None,
            health: Default::default(),
            proxy: None,
            logs: Vec::new(),
            env: Default::default(),
        }
    }

    #[test]
    fn runtime_url_replaces_credentials_without_losing_database_options() {
        let pg = shared_types::StartPgCredential {
            username: "runtimeuser".into(),
            password: "new@:/#?%40password".into(),
        };
        let value = database_url_with_credentials(
            "postgresql://old:stale@localhost:5433/business?sslmode=disable",
            &pg,
        )
        .unwrap();
        let url = reqwest::Url::parse(&value).unwrap();
        assert_eq!(url.username(), "runtimeuser");
        assert_eq!(url.host_str(), Some("localhost"));
        assert_eq!(url.port(), Some(5433));
        assert_eq!(url.path(), "/business");
        assert_eq!(url.query(), Some("sslmode=disable"));
        assert!(!value.contains("stale"));
        assert!(value.contains("%40"));
        assert!(value.contains("%2540password"));
        assert_eq!(url.fragment(), None);
        let error = database_url_with_credentials("not a url privatepassword", &pg).unwrap_err();
        assert!(!error.to_string().contains("privatepassword"));
    }

    #[test]
    fn managed_credentials_remove_query_overrides_in_actual_service_environment() {
        let mut spec = spec_with(None);
        spec.env.insert("DATABASE_URL".into(),
            "postgresql://old:old@localhost/db?user=old&password=old&user=again&%70assword=again&sslmode=disable&options=-c%20search_path%3Dpublic".into());
        spec.env
            .insert("PGSSLROOTCERT".into(), "/fixture/root.crt".into());
        spec.env
            .insert("PGOPTIONS".into(), "-c application_name=fixture".into());
        let pg = shared_types::StartPgCredential {
            username: "managed".into(),
            password: "managedsecret".into(),
        };
        let environment = service_environment(&spec, Some(&pg)).unwrap();
        let url = reqwest::Url::parse(&environment["DATABASE_URL"]).unwrap();
        assert!(
            !url.query_pairs()
                .any(|(key, _)| key == "user" || key == "password")
        );
        let target = pg_probe_environment(environment).unwrap();
        assert_eq!(target["PGUSER"], "managed");
        assert_eq!(target["PGPASSWORD"], "managedsecret");
        assert_eq!(target["PGSSLROOTCERT"], "/fixture/root.crt");
        assert_eq!(target["PGOPTIONS"], "-c search_path=public");
        spec.env.remove("DATABASE_URL");
        spec.env.insert("DATABASE_URL".into(), String::new());
        let target = pg_probe_environment(service_environment(&spec, Some(&pg)).unwrap()).unwrap();
        assert_eq!(target["PGOPTIONS"], "-c application_name=fixture");
    }

    #[test]
    fn operation_credentials_override_artifact_defaults_and_survive_spec_roundtrip() {
        let pg = resolve_run_pg(Some(shared_types::StartPgCredential {
            username: "runtimeuser".into(),
            password: "runtimepassword".into(),
        }))
        .unwrap()
        .unwrap();
        let mut spec = spec_with(None);
        spec.env
            .insert("POSTGRES_USER".into(), "artifactuser".into());
        spec.env
            .insert("POSTGRES_PASSWORD".into(), "artifactpassword".into());
        spec.env.insert("OTHER_SETTING".into(), "retained".into());
        spec.env.insert(
            "DATABASE_URL".into(),
            "postgresql://artifactuser:artifactpassword@localhost/dev".into(),
        );
        let env = service_environment(&spec, Some(&pg)).unwrap();
        assert_eq!(env["POSTGRES_USER"], "runtimeuser");
        assert_eq!(env["POSTGRES_PASSWORD"], "runtimepassword");
        assert_eq!(env["OTHER_SETTING"], "retained");
        assert_eq!(
            env["DATABASE_URL"],
            "postgresql://runtimeuser:runtimepassword@localhost/dev"
        );
        let file = crate::svc_spec::ServiceSpecFile {
            release_id: "release1".into(),
            service_id: "service1".into(),
            cwd: ".".into(),
            argv: vec!["node".into()],
            env,
            port: Some(4200),
        };
        let decoded: crate::svc_spec::ServiceSpecFile =
            toml::from_str(&toml::to_string(&file).unwrap()).unwrap();
        assert_eq!(decoded.env["POSTGRES_PASSWORD"], "runtimepassword");
        assert!(
            !decoded
                .runtime_env_overrides(Path::new("logs"))
                .contains_key("POSTGRES_PASSWORD")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn migration_process_receives_captured_runtime_credentials() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = spec_with(None);
        spec.env.insert("POSTGRES_USER".into(), "staleuser".into());
        spec.env
            .insert("POSTGRES_PASSWORD".into(), "stalepassword".into());
        let pg = shared_types::StartPgCredential {
            username: "operationuser".into(),
            password: "operationpassword".into(),
        };
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "printf '%s\n%s\n' \"$POSTGRES_USER\" \"$POSTGRES_PASSWORD\" > credentials".into(),
        ];
        run_transient_with_env(
            &argv,
            root.path(),
            &service_environment(&spec, Some(&pg)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("credentials")).unwrap(),
            "operationuser\noperationpassword\n"
        );
    }

    /// PG 预检门控（e39591126 起为环境策略）：平台声明 APP_CLI_REQUIRE_PG=1
    /// 才探测；缺省不探测——即使服务声明了 migrate（独立 app-cli 不从服务
    /// 清单推断本地数据库，60s 轮询不得阻塞无数据库工作区启动）。
    #[test]
    fn pg_wait_is_gated_on_declared_need() {
        // 平台未声明 → 不探测（含声明 migrate 的服务）
        assert!(!pg_required_by_policy(None));
        assert!(!pg_required_by_policy(Some(std::ffi::OsStr::new(""))));
        assert!(!pg_required_by_policy(Some(std::ffi::OsStr::new("0"))));
        // 平台声明（builder 形态注入）→ 探测
        assert!(pg_required_by_policy(Some(std::ffi::OsStr::new("1"))));
    }

    /// dev 形态 + 有 [devrun] → devrun.command（热加载命令生效）。
    #[test]
    fn dev_profile_prefers_devrun_command() {
        let spec = spec_with(Some(vec!["pnpm", "exec", "vite"]));
        let argv = effective_run_argv(&spec, true);
        assert_eq!(argv, &["pnpm", "exec", "vite"]);
    }

    /// dev 形态但未配 [devrun] → 回落 [run].command（未配置服务的兜底语义）。
    #[test]
    fn dev_profile_falls_back_to_run_without_devrun() {
        let spec = spec_with(None);
        let argv = effective_run_argv(&spec, true);
        assert_eq!(argv, &["node", "server.js"]);
    }

    /// 非 dev 形态（生产/本地直跑）恒走 [run]——即便配置了 [devrun] 也不生效。
    #[test]
    fn prod_profile_always_uses_run_command() {
        let spec = spec_with(Some(vec!["pnpm", "exec", "vite"]));
        let argv = effective_run_argv(&spec, false);
        assert_eq!(argv, &["node", "server.js"]);
    }

    /// 窗口内探测超时：无人监听的端口在 1s 窗口内轮询后 Err（含 URL 与窗口信息）。
    /// （成功分支与 bridge 等待共用同一探测核心，由集成/冒烟覆盖。）
    #[tokio::test]
    async fn readiness_probe_times_out_within_window() {
        // 找一个确定空闲的端口：bind 后立即释放（TIME_WAIT 由 connect 端触发，服务端无）
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let spec = spec_with(None);
        let spec = crate::manifest::ServiceSpec {
            port,
            health: workspace_manifest::HealthSection {
                readiness_path: "/ready".into(),
                ..Default::default()
            },
            ..spec
        };
        let err = wait_for_service_ready_within(&spec, 1)
            .await
            .expect_err("must time out");
        let message = format!("{err:#}");
        assert!(message.contains("not ready within 1 seconds"), "{message}");
        assert!(message.contains("/ready"), "{message}");
    }
    /// R08 反例：owner 复用的每操作 PG 凭据必须到达服务进程 env——
    /// run_with_cancel(pg) → start_service 注入 POSTGRES_USER/PASSWORD
    /// （last-wins 覆盖 spec.env 与进程透传值）。
    #[cfg(unix)]
    #[tokio::test]
    async fn per_operation_pg_credential_reaches_service_env() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let service_dir = workspace.join("web");
        std::fs::create_dir_all(&service_dir).unwrap();
        let log_dir = dir.path().join("logs");
        let dump = dir.path().join("env.dump");
        let spec = ServiceSpec {
            service_id: "web".into(),
            name: "Web".into(),
            dir: "web".into(),
            r#type: workspace_manifest::ProjectType::Node,
            kind: workspace_manifest::ProjectKind::Web,
            enabled: true,
            port: 4578,
            devbuild: None,
            run: RunSection {
                // spec.env 里的旧凭据必须被运行时变量覆盖（last-wins）
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    // 完成标记防撕裂读（env 逐段写文件，POSTGRES_* 按序靠后）
                    format!(
                        "env | sort > '{}'; echo __DUMP_DONE__ >> '{}'; sleep 30",
                        dump.display(),
                        dump.display()
                    ),
                ],
                migrate: Vec::new(),
                depends_on: Vec::new(),
                shutdown_timeout_seconds: 0,
            },
            devrun: None,
            static_content_dir: None,
            health: Default::default(),
            proxy: None,
            logs: Vec::new(),
            env: [
                ("POSTGRES_USER".to_string(), "stale-user".to_string()),
                ("POSTGRES_PASSWORD".to_string(), "stale-pass".to_string()),
            ]
            .into(),
        };
        let pg = shared_types::StartPgCredential {
            username: "biz_user".into(),
            password: "new-s3cret".into(),
        };
        let mut child = start_service(
            &spec,
            &spec.run.command,
            &workspace,
            &log_dir,
            "rel-1",
            Some(&pg),
        )
        .await
        .unwrap();
        // 等 env dump 落盘（spawn 异步）
        let content = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&dump).await
                    && text.contains("__DUMP_DONE__")
                {
                    break text;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            content.lines().any(|l| l == "POSTGRES_USER=biz_user"),
            "new username must win over spec.env; got:\n{content}"
        );
        assert!(
            content.lines().any(|l| l == "POSTGRES_PASSWORD=new-s3cret"),
            "new password must win over spec.env; got:\n{content}"
        );
        let _ = child.stop(Duration::from_secs(5)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forced_shutdown_reaps_real_child_and_confirms_group_absence() {
        use tokio::io::AsyncBufReadExt;
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
            .stdout(Stdio::piped());
        let mut child = spawn_managed(command).unwrap();
        let pid = child.id().unwrap();
        let mut lines = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("ready"));
        shutdown_all(vec![("ignores-term".into(), child)], 0)
            .await
            .unwrap();
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }

    /// R01 反例：服务根进程退出、孙进程仍存活持组时，监督循环的退出检测必须
    /// 立即触发（root 语义）——树级 try_wait 会把根进程死亡掩盖到孙进程退出，
    /// 服务该重启时不重启。
    #[cfg(unix)]
    #[tokio::test]
    async fn exit_detection_fires_on_root_exit_even_with_live_grandchild() {
        let mut command = Command::new("sh");
        // root 立即退出，孙进程 sleep 60 继续存活持有进程组
        command
            .args(["-c", "sleep 60 & exit 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_managed(command).unwrap();
        let pid = child.id().unwrap();
        // 等 root 退出事实落地（孙进程仍在）
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait_root())
            .await
            .expect("root wait deadline")
            .unwrap();
        assert!(status.success());
        assert!(
            process_utils::process_group_exists(pid).unwrap(),
            "grandchild must still be alive to set up the regression fixture"
        );
        // root 语义退出检测必须立刻识别（500ms 轮询周期内）
        let mut children = vec![("root-exited".into(), child)];
        let detected =
            tokio::time::timeout(Duration::from_secs(3), poll_any_exit(&mut children, &[]))
                .await
                .expect("poll_any_exit must fire on root exit while grandchild lives");
        assert_eq!(detected, Some("root-exited".into()));
        shutdown_all(children, 0).await.unwrap();
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn migration_timeout_confirms_original_process_group_stopped() {
        let root = tempfile::tempdir().unwrap();
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "echo $$ > migration.pid; exec sleep 30".into(),
        ];
        let result =
            run_transient_with_timeout(&argv, root.path(), Duration::from_millis(100)).await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
        let pid: u32 = std::fs::read_to_string(root.path().join("migration.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_migration_cannot_leave_background_group_writing() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().to_owned();
        let migration = tokio::spawn(async move {
            let argv: Vec<String> = vec![
                "sh".into(),
                "-c".into(),
                "echo $$ > migration.pid; while [ ! -f exit-now ]; do :; done; exit 0".into(),
            ];
            run_transient_with_timeout(&argv, &directory, Duration::from_secs(5)).await
        });
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(root.path().join("migration.pid")).await
                    && let Ok(pid) = text.trim().parse::<u32>()
                    // 撕裂读出的 pid 前缀几乎必为死组：join 前确认组确实存在
                    && matches!(process_utils::process_group_exists(pid), Ok(true))
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Keep a second real process in the migration group, but parent it to the
        // test so it is reaped deterministically instead of depending on PID 1.
        let mut member = None;
        for _ in 0..3 {
            let mut command = Command::new("sleep");
            command.arg("30").process_group(i32::try_from(pid).unwrap());
            match command.spawn() {
                Ok(child) => {
                    member = Some(child);
                    break;
                }
                // tokio/std spawn 的瞬时 ECHILD（运行时竞态）：短暂退避重试
                Err(error) if error.raw_os_error() == Some(10) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("spawn migration group member: {error}"),
            }
        }
        let mut member = member.expect("member spawn after retries");
        let reaper = tokio::spawn(async move { member.wait().await });
        tokio::fs::write(root.path().join("exit-now"), b"go")
            .await
            .unwrap();
        migration.await.unwrap().unwrap();
        // 成员死亡证据是下方组探测 ESRCH。成员 wait 可能被树收束确认路径的
        // waitpid(-pgid) 抢先 reap（tokio 视角 ECHILD）——那是确认收束的正
        // 常组成，不构成失败；仍可观测到退出码时必须非成功。
        match reaper.await.expect("reaper join") {
            Ok(status) => assert!(!status.success()),
            Err(error) if error.raw_os_error() == Some(10) => {}
            Err(error) => panic!("member wait: {error}"),
        }
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }
}

#[cfg(test)]
mod pg_readiness_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn pg_probe_uses_migration_database_and_managed_credentials() {
        let environment = BTreeMap::from([
            ("DATABASE_URL".into(), "".into()),
            ("PGHOST".into(), "127.0.0.2".into()),
            ("PGPORT".into(), "5439".into()),
            ("POSTGRES_USER".into(), "runtimeuser".into()),
            ("POSTGRES_PASSWORD".into(), "fixturepassword".into()),
            ("POSTGRES_DB".into(), "applicationdb".into()),
        ]);
        let target = pg_probe_environment(environment).unwrap();
        assert_eq!(target["PGDATABASE"], "applicationdb");
        assert_eq!(target["PGUSER"], "runtimeuser");
        assert_eq!(target["PGHOST"], "127.0.0.2");
        assert_eq!(target["PGPORT"], "5439");
        assert_eq!(target["PGPASSWORD"], "fixturepassword");
        let url = "postgresql://fixture:secret@localhost:5440/otherdb?sslmode=disable";
        let target = pg_probe_environment(BTreeMap::from([
            ("DATABASE_URL".into(), url.into()),
            ("POSTGRES_DB".into(), "ignored".into()),
        ]))
        .unwrap();
        assert_eq!(target["PGDATABASE"], "otherdb");
        assert_eq!(target["PGUSER"], "fixture");
        assert_eq!(target["PGPASSWORD"], "secret");
        assert_eq!(target["PGHOST"], "localhost");
        assert_eq!(target["PGPORT"], "5440");
        assert_eq!(target["PGSSLMODE"], "disable");
        let target = pg_probe_environment(BTreeMap::from([("DATABASE_URL".into(),
            "postgresql://user:p%40ss%2Bword@[::1]:5442/db%20name?host=%2Ftmp&dbname=overridden&options=-c%20search_path%3Dpublic".into())])).unwrap();
        assert_eq!(target["PGHOST"], "/tmp");
        assert_eq!(target["PGPASSWORD"], "p@ss+word");
        assert_eq!(target["PGDATABASE"], "overridden");
        assert_eq!(target["PGOPTIONS"], "-c search_path=public");
        assert!(
            pg_probe_environment(BTreeMap::from([(
                "DATABASE_URL".into(),
                "postgresql://localhost/db?unrecognized=hidden".into()
            )]))
            .is_err()
        );
    }

    #[cfg(unix)]
    fn probe_fixture(directory: &Path, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let program = directory.join("psql-fixture");
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        program.to_str().unwrap().to_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pg_probe_waits_for_database_creation_before_allowing_migration() {
        let directory = tempfile::tempdir().unwrap();
        let program = probe_fixture(
            directory.path(),
            r#"#!/bin/sh
[ "$PGDATABASE" = "delayeddb" ] || exit 99
[ "$PGUSER" = "manageduser" ] || exit 99
[ "$7" = "SELECT 1" ] || exit 99
if [ ! -f "$PROBE_STATE/attempted" ]; then
  touch "$PROBE_STATE/attempted"
  exit 2
fi
touch "$PROBE_STATE/database-login-confirmed"
"#,
        );
        let target = BTreeMap::from([
            ("PGDATABASE".into(), "delayeddb".into()),
            ("PGUSER".into(), "manageduser".into()),
            (
                "PROBE_STATE".into(),
                directory.path().to_str().unwrap().into(),
            ),
        ]);
        wait_for_pg_targets(
            &program,
            &[target],
            Duration::from_secs(2),
            Duration::from_millis(10),
            None,
        )
        .await
        .unwrap();
        assert!(directory.path().join("attempted").exists());
        assert!(directory.path().join("database-login-confirmed").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pg_probe_deadline_kills_and_reaps_hung_client() {
        let directory = tempfile::tempdir().unwrap();
        let program = probe_fixture(
            directory.path(),
            r#"#!/bin/sh
printf '%s' "$$" > "$PROBE_STATE/pid"
exec sleep 30
"#,
        );
        let target = BTreeMap::from([(
            "PROBE_STATE".into(),
            directory.path().to_str().unwrap().into(),
        )]);
        let result = wait_for_pg_targets(
            &program,
            &[target],
            Duration::from_millis(150),
            Duration::from_millis(10),
            None,
        )
        .await;
        assert!(result.is_err());
        let pid = std::fs::read_to_string(directory.path().join("pid")).unwrap();
        let status = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "probe client survived deadline");
    }
}

/// Executed only by tools/test_pg_readiness_real.py against its isolated PG16.
#[cfg(test)]
mod real_pg_readiness_fixture {
    use super::*;

    #[tokio::test]
    #[ignore = "requires tools/test_pg_readiness_real.py isolated PostgreSQL fixture"]
    async fn actual_pg_delayed_database_uri() {
        let program = std::env::var("PG_READINESS_FIXTURE_PROGRAM")
            .expect("use tools/test_pg_readiness_real.py");
        let target = pg_probe_environment(std::collections::BTreeMap::from([(
            "DATABASE_URL".into(),
            "postgresql://fixture@127.0.0.1:5549/delayeddb".into(),
        )]))
        .unwrap();
        let started = std::time::Instant::now();
        wait_for_pg_targets(
            &program,
            &[target],
            Duration::from_secs(15),
            Duration::from_millis(200),
            None,
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(4),
            "migration released before delayed database existed"
        );
    }
}
