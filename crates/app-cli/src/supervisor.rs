//! 子项目 + pingap 编排核心：wait PG → migrate → start 子项目 → spawn pingap → supervise。
//!
//! 替代 workspace start.sh。由 main.rs 调用 `run(&args)`，前台阻塞直到任一子进程退出或收到信号，
//! 然后 kill 所有子进程 + return → supervisor [program:app] 感知退出 → 整组重启。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tracing::{error, info, warn};

use crate::config::CliArgs;
use crate::manifest::{self, ServiceSpec};
use crate::proxy::admin_probe;
use crate::proxy::compiler::compile_and_validate;
use crate::proxy::pingap::PINGAP_PORT;
use crate::runtime_status::RuntimeStatusService;

/// 编排主入口（legacy 直跑形态：一次性编排，无外部取消源）。
pub async fn run(args: &CliArgs, runtime_status: RuntimeStatusService) -> Result<()> {
    run_inner(args, runtime_status, None, None).await
}

/// 编排主入口（server 形态：`cancel` 触发 = 优雅停全部子服务后 Ok 返回，
/// 供热部署切换/容器 SIGTERM 级联停服；`on_running` 在编排完成进入 supervise
/// 时发送一次——server 据此把相位切到 Running）。
pub async fn run_with_cancel(
    args: CliArgs,
    runtime_status: RuntimeStatusService,
    cancel: tokio_util::sync::CancellationToken,
    on_running: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    run_inner(&args, runtime_status, Some(cancel), on_running).await
}

/// 等 SIGTERM 的可复用 future（server 主循环 select 消费；Unix handler 安装
/// 失败降级为永不完成，由其他分支兜底）。
pub(crate) async fn sigterm_watch() {
    wait_sigterm().await
}

async fn run_inner(
    args: &CliArgs,
    runtime_status: RuntimeStatusService,
    cancel: Option<tokio_util::sync::CancellationToken>,
    on_running: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    runtime_status.set_ready(false);
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
    // dev 形态编排信号：[devrun].command 优先、[run].command 兜底（源码态 dev 链路）。
    let dev_profile = dev_run_profile();
    if dev_profile {
        info!("🧪 dev run profile: services with [devrun] start via their dev command");
    }

    // 2. wait PG（PG 由 supervisor [program:postgresql] 托管，秒级就绪；失败不阻断）
    wait_for_pg().await?;

    // 3. 各子项目 migrate → start；4. 编译验证并启动 Pingap。
    // 任一阶段失败统一兜底：先 shutdown_all 优雅停掉已启动的子进程再返回 Err。
    // tokio Child drop 默认不杀进程, 子进程又在独立进程组: 不清理会被 reparent 到
    // PID1 继续持端口, 外层 supervisor 重启 app-cli 后新实例同名服务 bind 冲突
    // → 永久 crash loop (只能重建容器恢复)。start_pingap 内部 config 确认失败
    // 路径已自行清理, 此处对已 take 空的集合再调 shutdown_all 幂等无害。
    let mut children: Vec<(String, Child)> = Vec::new();
    let mut started_user_services = 0usize;
    let startup = async {
        for spec in &specs {
            // migrate（如有）—— 失败 error 上报 + Fail Fast; stdout/stderr 已落 app-cli 日志。
            // migrate 恒用 [run].migrate（迁移是数据语义，与 dev/prod 启动形态无关）。
            if !spec.run.migrate.is_empty() {
                info!("🛠️  migrate {}", spec.service_id);
                run_transient(&spec.run.migrate, &args.workspace.join(&spec.dir))
                    .await
                    .with_context(|| format!("migrate {}", spec.service_id))?;
            }
            // start（dev 形态下 [devrun].command 优先、[run].command 兜底）
            let argv = effective_run_argv(spec, dev_profile);
            if argv.is_empty() {
                warn!("⚠️  {} 无启动 command（[run]/[devrun]），跳过", spec.service_id);
                continue;
            }
            let child = start_service(spec, argv, &args.workspace, &args.log_dir, &release.release_id)
                .with_context(|| format!("start {}", spec.service_id))?;
            children.push((spec.service_id.clone(), child));
            started_user_services += 1;
        }
        // 编译、完整验证并启动 Pingap；代理失败时 workspace 不得进入 ready。
        start_pingap(&args.workspace, &args.pingap_bin, &release, &mut children).await
    };
    if let Err(e) = startup.await {
        error!("❌ startup failed, shutting down already-started children: {e:#}");
        shutdown_all(std::mem::take(&mut children), 5).await;
        return Err(e);
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
        let state = if started_ids.contains(spec.service_id.as_str()) {
            if dev_profile && spec.devrun.is_some() {
                "running (devrun)"
            } else {
                "running"
            }
        } else {
            "skipped (无启动 command)"
        };
        info!(
            "🔌   {} port={} {state} {route}",
            spec.service_id, spec.port
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
        shutdown_all(std::mem::take(&mut children), 5).await;
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
    supervise(children, shutdown_timeout, cancel).await;
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

// ── PG 等待 ──────────────────────────────────────────────────────────────────

/// pg_isready 轮询（最多 30 次 × 2s = 60s），失败不阻断（PG 可能晚于 app-cli 启）。
pub(crate) async fn wait_for_pg() -> Result<()> {
    // 本地开发逃生开关：前端服务不依赖 PG 时跳过 60s pg_isready 轮询（生产环境不设）。
    if std::env::var_os("APP_CLI_SKIP_PG_WAIT").is_some() {
        warn!("⏭  APP_CLI_SKIP_PG_WAIT set; skipping PostgreSQL readiness check (dev only)");
        return Ok(());
    }
    let host = std::env::var("PGHOST").unwrap_or_else(|_| "localhost".into());
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    let user = std::env::var("POSTGRES_USER").unwrap_or_else(|_| "app".into());
    let pwd = std::env::var("POSTGRES_PASSWORD").unwrap_or_else(|_| "app".into());

    for i in 1..=30u8 {
        let result = Command::new("pg_isready")
            .arg("-h")
            .arg(&host)
            .arg("-p")
            .arg(&port)
            .arg("-U")
            .arg(&user)
            .env("PGPASSWORD", &pwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if matches!(result, Ok(s) if s.success()) {
            info!("✅ PG ready (after {i} attempt(s))");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    anyhow::bail!("PostgreSQL not ready after 60 seconds");
}

/// 轮询单个桥接后端的 readiness_path 直至就绪(120s 超时)。
///
/// 仅在 workspace.manifest `[health].bridge_service` 显式配置时调用(只等那一个后端)。
/// 默认(不配 bridge)不调本函数 —— app-cli 自给 /ready,不强依赖任何后端。
pub(crate) async fn wait_for_service_ready(spec: &ServiceSpec) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .context("build readiness HTTP client")?;
    let url = format!(
        "http://127.0.0.1:{}{}",
        spec.port, spec.health.readiness_path
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
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
                "service '{}' readiness timed out after 120 seconds (path {})",
                spec.service_id,
                spec.health.readiness_path
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
fn dev_run_profile() -> bool {
    std::env::var("APP_CLI_RUN_PROFILE").as_deref() == Ok("dev")
}

/// 服务的生效启动命令：dev 形态且配置了 [devrun] 时用 devrun.command（热加载，
/// 跑源码），否则 [run].command。（[devbuild] 的回落在平台侧 dev 链路执行，
/// app-cli 不消费该字段。）
fn effective_run_argv(spec: &ServiceSpec, dev_profile: bool) -> &[String] {
    if dev_profile
        && let Some(devrun) = &spec.devrun
    {
        return &devrun.command;
    }
    &spec.run.command
}

fn start_service(
    spec: &ServiceSpec,
    argv: &[String],
    ws_root: &Path,
    log_dir: &Path,
    release_id: &str,
) -> Result<Child> {
    let cwd = ws_root.join(&spec.dir);
    let service_log_dir = log_dir.join(&spec.service_id);
    std::fs::create_dir_all(&service_log_dir)
        .with_context(|| format!("create service log dir {}", service_log_dir.display()))?;
    let out_path = service_log_dir.join("runtime.out.log");
    let err_path = service_log_dir.join("runtime.err.log");

    let mut cmd = process_group_command(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&cwd)
        .envs(&spec.env)
        // Runtime-owned variables are applied last so even a hand-crafted
        // release lock cannot override service identity, paths, or ports.
        .env("HOSTNAME", "0.0.0.0")
        .env("PORT", spec.port.to_string())
        .env("APP_LOG_DIR", &service_log_dir)
        .env("APP_SERVICE_ID", &spec.service_id)
        .env("APP_RELEASE_ID", release_id)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}: {}", spec.service_id, argv.join(" ")))?;

    // pipe → 带轮转的日志文件（append 模式，不 truncate；超 10MB rotate，保留 3 份）
    if let Some(stdout) = child.stdout.take() {
        let p = out_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(
            stdout, p, None, None,
        ));
    }
    if let Some(stderr) = child.stderr.take() {
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
pub(crate) async fn run_transient(argv: &[String], cwd: &Path) -> Result<()> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn migrate: {}", argv.join(" ")))?;

    // 并发 drain stdout/stderr：防 pipe 被写满阻塞 + 捕获失败原因
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
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

    // migrate 超时兜底：命令等待 stdin / 死循环时会永久卡住启动阶段
    //（不进 supervise、readiness 恒 false、supervisord 也不重启它——进程
    // 没退出），与 readiness 120s / bg_chat 150s 同款有限等待
    const MIGRATE_TIMEOUT: Duration = Duration::from_secs(300);
    let status = match tokio::time::timeout(MIGRATE_TIMEOUT, child.wait()).await {
        Ok(status) => status.context("wait migrate")?,
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!(
                "migrate timed out after {}s: {}",
                MIGRATE_TIMEOUT.as_secs(),
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
    pingap_bin: &Path,
    release: &workspace_manifest::ReleaseLock,
    children: &mut Vec<(String, Child)>,
) -> Result<()> {
    let runtime_root = std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/run/app-cli/pingap".into());
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

    let mut cmd = process_group_command(pingap_bin);
    cmd.arg("-c")
        .arg(&outcome.config_path)
        .arg("--autoreload")
        // pingap 的 env override 规则：`get_from_env(key)` 读 `PINGAP_{key}` 全大写
        //（pingap src/main.rs parse_arguments 的闭包），故必须用 PINGAP_ADMIN_* 而非 admin_*。
        // 凭证经 env 注入（不进命令行避免 ps 泄露、不落盘不进日志），admin 仅 loopback 只读。
        .env("PINGAP_ADMIN_ADDR", &endpoint.addr)
        .env("PINGAP_ADMIN_USER", &endpoint.user)
        .env("PINGAP_ADMIN_PASSWORD", &endpoint.password);
    let child = cmd.spawn().context("spawn pingap")?;
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
        shutdown_all(std::mem::take(children), 5).await;
        return Err(error).context("confirm initial Pingap config via loopback admin probe");
    } else {
        info!("✅ pingap initial config confirmed (config_hash matched)");
    }
    Ok(())
}

// ── supervise（信号 + 任一退出 → kill all → return）─────────────────────────────

/// 优雅停机宽限期（秒）：先 SIGTERM，超时后 SIGKILL。
/// 对齐 agent_runner shutdown 惯例，避免 DB/写文件类子进程丢未刷盘数据。
/// 阻塞直到收到 SIGINT/SIGTERM 或任一子进程退出 → 优雅停止所有子进程 → return。
async fn supervise(
    mut children: Vec<(String, Child)>,
    shutdown_timeout_seconds: u64,
    cancel: Option<tokio_util::sync::CancellationToken>,
) {
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
        exited = poll_any_exit(&mut children) => {
            if let Some(name) = exited {
                error!("❌ {name} exited — shutting down (supervisor will restart)");
            }
        }
    }
    shutdown_all(children, shutdown_timeout_seconds).await;
}

/// 优雅停止所有子进程：SIGTERM → 等 `SHUTDOWN_GRACE_SECS` → SIGKILL 残留。
async fn shutdown_all(mut children: Vec<(String, Child)>, shutdown_timeout_seconds: u64) {
    // 1. SIGTERM 所有子进程
    for (_, child) in children.iter_mut() {
        send_term(child);
    }
    info!(
        "🛑 SIGTERM sent to {} process(es); grace {}s",
        children.len(),
        shutdown_timeout_seconds
    );

    // 2. grace 窗口内轮询收尸
    let mut done = vec![false; children.len()];
    let deadline = std::time::Instant::now() + Duration::from_secs(shutdown_timeout_seconds);
    while std::time::Instant::now() < deadline {
        for (i, (_, child)) in children.iter_mut().enumerate() {
            if !done[i] && matches!(child.try_wait(), Ok(Some(_))) {
                done[i] = true;
            }
        }
        if done.iter().all(|&d| d) {
            info!("✅ all children exited gracefully");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 3. 超时 SIGKILL 残留
    for (i, (name, child)) in children.iter_mut().enumerate() {
        if !done[i] {
            send_kill(child);
            warn!("💀 force-killed {name} (SIGKILL after grace)");
        }
    }
}

/// 向子进程发 SIGTERM（unix）；其他平台无 SIGTERM，退化为 SIGKILL。
#[cfg(unix)]
fn send_term(child: &mut Child) {
    if let Some(pid) = child.id()
        && !process_utils::kill_process_group(pid, process_utils::KillSignal::SIGTERM)
    {
        // 信号未送达通常意味进程已退出; 仅 debug 留痕 (PID1 防御见 process_utils)
        tracing::debug!(pid, "send_term: signal not delivered");
    }
}

#[cfg(not(unix))]
fn send_term(child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(unix)]
fn send_kill(child: &mut Child) {
    if let Some(pid) = child.id()
        && !process_utils::kill_process_group(pid, process_utils::KillSignal::SIGKILL)
    {
        tracing::debug!(pid, "send_kill: signal not delivered");
    }
}

#[cfg(not(unix))]
fn send_kill(child: &mut Child) {
    let _ = child.start_kill();
}

/// 轮询所有子进程，第一个退出的返回其名字（500ms 间隔）。
async fn poll_any_exit(children: &mut [(String, Child)]) -> Option<String> {
    loop {
        for (name, child) in children.iter_mut() {
            match child.try_wait() {
                Ok(Some(_)) => return Some(name.clone()),
                Ok(None) => {}
                Err(_) => return Some(name.clone()),
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

#[cfg(unix)]
fn process_group_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    // 用标准库 process_group(0) 让子进程成为新进程组组长（fork 后 setpgid(0,0)）。
    // kill_process_group(-pgid) 仍能整组发信号含子孙，与原 `setsid` 方案对信号语义等价，
    // 但不依赖外部 setsid 二进制 —— Linux/macOS 标准库自带，真正跨平台（原方案 macOS 无 setsid）。
    let mut command = Command::new(program);
    command.process_group(0);
    command
}

#[cfg(not(unix))]
fn process_group_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    Command::new(program)
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
            health: Default::default(),
            proxy: None,
            logs: Vec::new(),
            env: Default::default(),
        }
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
}
