//! supervisord 托管引擎：用户服务 + pingap 注册为动态 program，由 supervisord
//! per-service 托管（隔离重启：单服务崩只重启它，PG/ttyd/终端不中断）。
//!
//! 编排流（server 的 Orchestrating 阶段调用）：
//! 下载换 code（server 主循环已做）→ validate → wait PG → migrate → 写
//! per-service spec 文件（/run tmpfs，run-service 的启动契约）→ 生成 program
//! conf（conf.d 分片）→ reloadConfig → 旧代组摘除 → 依赖序 startProcess →
//! pingap 启动 + hash 确认 → bridge readiness。
//!
//! 与 builtin 引擎（supervisor.rs 的 spawn+supervise 循环）共享：lock 解析、
//! migrate、pingap 配置编译、readiness 语义；差异只在进程托管层。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::config::RuntimeArgs;
use crate::manifest::{ReleaseLock, ServiceSpec};
use crate::proxy::admin_probe;
use crate::proxy::compiler::{CompileOutcome, compile_and_validate};
use crate::runtime_status::RuntimeStatusService;
use crate::supervisor;
use crate::svc_spec::ServiceSpecFile;
use crate::xmlrpc::{SupervisorClient, SupervisorProcessState, SupervisordProcessInfo};

mod cleanup;
mod legacy;
mod receipt;
mod resident;

use legacy::*;
use receipt::*;

/// 动态 program 名前缀（与镜像固定 program 命名空间隔离）。
pub(crate) const SVC_PROGRAM_PREFIX: &str = "app-svc-";
pub(crate) const PINGAP_PROGRAM: &str = "app-pingap";
/// 动态分片文件（conf.d 通配吸入；50 排在固定服务之后无实际顺序意义——
/// 启停顺序由 server 显式 startProcess 控制）。
const CONF_PATH: &str = "/etc/supervisor/conf.d/50-app-services.conf";
const RESIDENT_CONF_FILE: &str = "51-app-cli-resident.conf";

/// 日志轮转对齐 builtin 引擎（10MB/3 份）。
const LOG_MAXBYTES: &str = "10MB";
const LOG_BACKUPS: &str = "3";

pub(crate) struct SupervisordHost {
    client: SupervisorClient,
    conf_path: PathBuf,
}

impl SupervisordHost {
    fn resident_conf_path(&self) -> Result<PathBuf> {
        Ok(self
            .conf_path
            .parent()
            .context("supervisor fragment has no parent")?
            .join(RESIDENT_CONF_FILE))
    }
    /// Read-only construction for proxy publication checks. Engine ownership
    /// receipts are written only by the business host's detect path.
    pub(crate) async fn from_env() -> Result<Option<Self>> {
        if !crate::xmlrpc::socket_exists() {
            return Ok(None);
        }
        let client = SupervisorClient::new(crate::xmlrpc::default_socket_path());
        client
            .ping()
            .await
            .context("supervisord observation unavailable")?;
        Ok(Some(Self {
            client,
            conf_path: CONF_PATH.into(),
        }))
    }
    #[cfg(test)]
    pub(crate) fn protocol_fixture(socket: PathBuf, conf_path: PathBuf) -> Self {
        Self {
            client: SupervisorClient::new(socket),
            conf_path,
        }
    }
    /// 引擎探测：socket 存在 + XML-RPC ping 成功（serve 启动时调用一次）。
    pub(crate) async fn detect() -> Result<Option<Self>> {
        if !crate::xmlrpc::socket_exists() {
            return Ok(None);
        }
        let client = SupervisorClient::new(crate::xmlrpc::default_socket_path());
        let version = client
            .ping()
            .await
            .context("supervisord socket exists but RPC is unavailable")?;
        // Remember the engine before the first runtime mutation. Losing its
        // socket later cannot turn external programs into a builtin-only run.
        // recovery v2 R1：统一 owner 进程内会话经显式 SessionScope 登记
        //（不再依赖已取消的 worker env）；旧 worker 链保留 env 分支。
        if let Some(scope) = runtime_supervisor::current_scope() {
            let receipt = EngineReceipt {
                generation: scope.generation.clone(),
                supervisor_id: scope.supervisor_id.clone(),
                socket: crate::xmlrpc::default_socket_path(),
            };
            let mut file = tempfile::NamedTempFile::new_in(&scope.work_root)?;
            serde_json::to_writer(&mut file, &receipt)?;
            std::io::Write::flush(&mut file)?;
            file.as_file().sync_all()?;
            process_utils::atomic_file::persist(file, &scope.work_root.join(ENGINE_RECEIPT))?;
            #[cfg(unix)]
            std::fs::File::open(&scope.work_root)?.sync_all()?;
        } else if let Some(root) = std::env::var_os(runtime_supervisor::WORKER_ENV) {
            let root = Path::new(&root);
            let scope = root
                .parent()
                .and_then(Path::parent)
                .context("supervisord worker scope missing")?;
            let worker = runtime_supervisor::Worker::from_env(scope)
                .await?
                .context("supervisord worker authorization missing")?;
            let receipt = EngineReceipt {
                generation: worker.generation().into(),
                supervisor_id: worker.supervisor_id().into(),
                socket: crate::xmlrpc::default_socket_path(),
            };
            let mut file = tempfile::NamedTempFile::new_in(worker.work_root())?;
            serde_json::to_writer(&mut file, &receipt)?;
            std::io::Write::flush(&mut file)?;
            file.as_file().sync_all()?;
            process_utils::atomic_file::persist(file, &root.join(ENGINE_RECEIPT))?;
            #[cfg(unix)]
            std::fs::File::open(root)?.sync_all()?;
        }
        info!("service host: supervisord {version} detected (engine=supervisord)");
        Ok(Some(Self {
            client,
            conf_path: PathBuf::from(CONF_PATH),
        }))
    }

    /// 编排：migrate → 写 specs/conf → reload → 旧组摘除 → 依赖序启动 →
    /// pingap 确认 → bridge readiness。成功返回后服务由 supervisord 托管
    ///（崩溃自动重启，与 server 进程生命周期解耦）。
    pub(crate) async fn orchestrate(
        &self,
        args: &RuntimeArgs,
        release: &ReleaseLock,
        runtime_status: &RuntimeStatusService,
        profile: supervisor::RunProfile,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let supervisor::RunProfile {
            run_migrations,
            dev_profile,
            pg,
            prepared_proxy,
        } = profile;
        let pg = supervisor::resolve_run_pg(pg)?;
        supervisor::validate_runtime_compatibility(release)?;
        let specs: Vec<ServiceSpec> = release
            .services
            .iter()
            .filter(|service| service.enabled)
            .cloned()
            .collect();
        // Validate every service before migrations or process launch can mutate runtime.
        for spec in &specs {
            supervisor::service_environment(spec, pg.as_ref())?;
        }

        if specs.is_empty() {
            bail!("release has no enabled services");
        }
        if supervisor::workspace_needs_pg(&specs) {
            supervisor::wait_for_pg(&specs, pg.as_ref(), Some(cancel)).await?;
        }

        // 记录旧代组（换代码前——reload 后按新集合差量摘除）与常驻入口
        // 是否**实际在服务**（C2/C1：以进程 state=RUNNING 为准——组已配置
        // 但 STOPPED/FATAL/未启动不构成"在服务"，不得跳过 startProcess，
        // 也不触发 standby 摘流；首编/入口已死 → 无流量可摘）。
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let previous_groups = self.dynamic_groups().await?;
        let entry_serving = self.resident_entry_running().await?;

        // 2. pingap 配置编译（生成/校验/原子提交——release 目录为候选）。
        //    active 的发布推迟到新服务就绪之后（V2-03 Restart 顺序：先
        //    standby 摘流 → 停旧 → 起新 → 发布正常路由并确认；提前发布
        //    会让新路由指向未启动的服务，切换窗口 502）。
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let pingap_outcome = match prepared_proxy {
            Some(prepared) => {
                anyhow::ensure!(
                    prepared.workspace == args.workspace
                        && prepared.dev_profile == dev_profile
                        && serde_json::to_value(&prepared.release)?
                            == serde_json::to_value(release)?,
                    "prepared proxy does not belong to this execution"
                );
                prepared.outcome
            }
            None => compile_pingap(args, release, dev_profile)
                .await
                .map_err(|error| {
                    supervisor::PreflightRejected(format!("proxy candidate preflight: {error:#}"))
                })?,
        };
        let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir);
        let endpoint = admin_probe::ensure_admin_endpoint()?;
        // Restart 摘流：常驻入口正在服务旧路由时，先校验热载兼容（server
        // 拓扑变化 Fail Fast 保旧服务），再切 standby 并确认热载生效
        // （mock 503 直出），然后停旧起新；首编（入口未起）跳过。
        if entry_serving {
            let active = crate::proxy::compiler::active_config_path(&runtime_root);
            let candidate = tokio::fs::read_to_string(&pingap_outcome.config_path)
                .await
                .with_context(|| {
                    format!("read candidate {}", pingap_outcome.config_path.display())
                })?;
            crate::proxy::compiler::validate_hot_reload_compatible(&active, &candidate).map_err(
                |error| {
                    supervisor::PreflightRejected(format!(
                        "proxy compatibility preflight: {error:#}"
                    ))
                },
            )?;
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
            self.drain_entry(&runtime_root).await?;
        }
        if !previous_groups.is_empty() {
            self.stop_business().await?;
        }
        runtime_status.set_ready(false);

        // 1. 应用迁移失败为诊断；物理清理和父意图取消仍保护启动边界。
        for spec in &specs {
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
            if run_migrations && !spec.run.migrate.is_empty() {
                info!("🛠️  migrate {}", spec.service_id);
                let report = supervisor::run_migration_with_receipt_cancel(
                    spec,
                    release,
                    &args.workspace,
                    &args.log_dir,
                    pg.as_ref(),
                    Some(cancel),
                )
                .await
                .with_context(|| format!("migrate {}", spec.service_id))?;
                tracing::debug!(service = %report.service_id, release = %report.release_id, outcome = ?report.outcome, stdout_bytes = report.stdout.len(), stderr_bytes = report.stderr.len(), "Migration stage finished");
            }
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        }

        // 2.5 workspace 首页静态服务（幂等；判定与 pingap 编译的兜底路由注入
        // 同源 index_port_if_eligible——路由注入了就必须有服务承接，否则根路径
        // 502。常驻 app-cli 进程：热部署重 orchestrate 不二次 bind，实时读文件
        // 自动切新 code 内容）。
        if crate::workspace_index::index_port_if_eligible(&args.workspace, &specs).is_some() {
            crate::workspace_index::ensure_spawned(&args.workspace)?;
            info!(
                "📄 workspace index (index.html) serving on :{}",
                crate::workspace_index::INDEX_PORT
            );
        }

        // 2.6 static 服务托管；dev + devrun 时让出端口，由 supervisord 启动
        // 开发服务。模式切换先排空旧静态 listener，再启动进程。
        crate::static_hosting::reconcile(&specs, &args.workspace, dev_profile).await?;
        for spec in &specs {
            if crate::static_hosting::hosts_statically(spec, dev_profile) {
                info!(
                    "📄 static host '{}' serving on :{}",
                    spec.service_id, spec.port
                );
            }
        }

        // 3. 写 per-service specs（run-service 启动契约；pingap 同机制承载凭证）
        let mut started: Vec<String> = Vec::new();
        let mut checked_instances = Vec::new();
        for spec in &specs {
            if !runs_as_process(spec, dev_profile) {
                continue;
            }
            let svc_spec = ServiceSpecFile {
                release_id: release.release_id.clone(),
                service_id: spec.service_id.clone(),
                cwd: args
                    .workspace
                    .join(&spec.dir)
                    .to_string_lossy()
                    .into_owned(),
                // B08：与 builtin 引擎同一生效命令选择（共享运行计划）——
                // dev profile 且配 [devrun] 时 devrun 优先（devrun 优先、
                // run 兜底），不再恒用 run.command。
                // R08：profile 来自**本次操作**（请求 Source 形态）显式传递，
                // env 仅作操作未指定时的兜底
                argv: crate::supervisor::effective_run_argv(spec, dev_profile).to_vec(),
                env: supervisor::service_environment(spec, pg.as_ref())?,
                port: Some(spec.port),
            };
            svc_spec
                .write()
                .with_context(|| format!("write spec {}", spec.service_id))?;
        }
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let pingap_spec = pingap_service_spec(args, endpoint);
        self.record_entry_domain(
            &runtime_root,
            &crate::proxy::compiler::active_config_path(&runtime_root),
            None,
        )
        .await?;
        pingap_spec.write().context("write resident pingap spec")?;
        // 常驻分片幂等覆写（每次编排重写同一内容——凭证/active 路径变化时
        // 随 reloadConfig 生效；分片不撤，组不删）。
        let resident_conf = render_resident_conf(&args.log_dir);
        write_conf(&self.resident_conf_path()?, &resident_conf).await?;

        // 4. 生成并重载 program conf（supervisord 不建日志目录——服务日志目录预建）
        let services_log_dir = args.log_dir.join("services");
        tokio::fs::create_dir_all(&services_log_dir)
            .await
            .with_context(|| format!("create {}", services_log_dir.display()))?;
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let conf =
            render_programs_conf(release, &specs, &args.log_dir, &args.workspace, dev_profile);
        write_conf(&self.conf_path, &conf).await?;
        let changes = mutation_result(self.client.reload_config().await)
            .context("supervisord reloadConfig")?;
        // reread reports configuration changes; update requires replacing the
        // old registered group. Preserve old specs until the replacement has
        // actually started and its active publication has been confirmed.
        for name in changes.changed.iter().chain(changes.removed.iter()) {
            if name.starts_with(SVC_PROGRAM_PREFIX) || name == PINGAP_PROGRAM {
                mutation_result(self.client.stop_remove_group(name).await)
                    .with_context(|| format!("replace changed program {name}"))?;
            }
        }

        // 5. 旧代差量摘除（不在新集合的业务组；常驻 app-pingap 不在
        //    dynamic_groups，天然不被摘除）
        let new_names: Vec<String> = specs
            .iter()
            .filter(|spec| runs_as_process(spec, dev_profile))
            .map(|s| format!("{SVC_PROGRAM_PREFIX}{}", s.service_id))
            .collect();
        for old in &previous_groups {
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
            if !new_names.contains(old)
                && let Err(e) = mutation_result(self.client.stop_remove_group(old).await)
            {
                return Err(e).with_context(|| format!("stop stale group {old}"));
            }
        }

        // 6. 依赖序启动（lock services 顺序即拓扑序；startProcessWait 等 startsecs）
        for spec in &specs {
            if !runs_as_process(spec, dev_profile) {
                // static 服务无进程（步骤 2.6 已内置托管）——不是配置问题，
                // 与进程态服务的"未配命令"区分文案
                if crate::static_hosting::hosts_statically(spec, dev_profile) {
                    info!(
                        "📄 {} static host（无进程，步骤 2.6 已托管）",
                        spec.service_id
                    );
                } else {
                    warn!(
                        "⚠️  {} has no command for the selected run profile; skipping",
                        spec.service_id
                    );
                }
                continue;
            }
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
            let name = format!("{SVC_PROGRAM_PREFIX}{}", spec.service_id);
            crate::orchestration_events::emit(
                &crate::orchestration_events::OrchestrationEvent::ServiceStarting {
                    service: spec.service_id.clone(),
                },
            );
            let result = if let Some(probe) = spec.health.startup_probe {
                self.start_checked(spec, &name, probe, cancel)
                    .await
                    .map(Some)
            } else {
                // Omitted strategy keeps supervisord's historical startsecs/retry behavior.
                async {
                    mutation_result(self.client.add_process_group(&name).await)?;
                    anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
                    mutation_result(self.client.start_process_wait(&name).await)?;
                    Ok(None)
                }
                .await
            };
            match result {
                Ok(instance) => {
                    if let Some(instance) = instance {
                        checked_instances.push((spec.service_id.clone(), name.clone(), instance));
                    }
                    crate::orchestration_events::emit(
                        &crate::orchestration_events::OrchestrationEvent::ServiceStartOk {
                            service: spec.service_id.clone(),
                        },
                    );
                }
                Err(error) => {
                    crate::orchestration_events::emit(
                        &crate::orchestration_events::OrchestrationEvent::ServiceStartFail {
                            service: spec.service_id.clone(),
                            error: format!("{error:#}"),
                        },
                    );
                    return Err(error).with_context(|| format!("start {name}"));
                }
            }
            started.push(name);
        }

        // 7. 新服务已就绪：发布正常路由到 active（常驻 -c 目标；已运行的
        //    常驻 pingap 由 --autoreload 拾取）→ 确保常驻组在 → hash 确认。
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let publication_guard = crate::proxy::compiler::publication_guard().await;
        crate::proxy::compiler::publish_active(&runtime_root, &pingap_outcome.config_path)
            .await
            .context("publish active pingap config")?;
        // C2：以进程 state=RUNNING 为准——STOPPED/FATAL/已配置未启动的组
        // 必须走 startProcess（addProcessGroup 对已存在组幂等成功）。
        if self.resident_entry_running().await? {
            info!("🛰️  resident {PINGAP_PROGRAM} running; relying on autoreload for active config");
        } else {
            // 组可能已存在（STOPPED/FATAL/supervisord 重启后 autostart=false）：
            // addProcessGroup 幂等，随后显式 startProcess。
            mutation_result(self.client.add_process_group(PINGAP_PROGRAM).await)?;
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
            mutation_result(self.client.start_process_wait(PINGAP_PROGRAM).await)
                .context("start app-pingap")?;
        }
        started.push(PINGAP_PROGRAM.to_string());
        let entry_instance = self.client.running_process(PINGAP_PROGRAM).await?;
        tokio::select! {
            result = admin_probe::wait_for_publication(
            endpoint,
            &pingap_outcome,
            admin_probe::CONFIRM_BUDGET,
        ) => {
                let confirmed = result.context("pingap publication confirm")?;
                anyhow::ensure!(i64::from(confirmed.process_id) == entry_instance.pid
                    && self.client.running_process(PINGAP_PROGRAM).await? == entry_instance,
                    "resident process changed during active confirmation");
                // 已确认生效：业务就绪观察以此核对 admin 实际 hash。
                crate::proxy::compiler::record_confirmed_publication(&pingap_outcome, &confirmed)?;
                self.record_entry_domain(&runtime_root, &crate::proxy::compiler::active_config_path(&runtime_root), Some(&entry_instance)).await?;
            }
            () = cancel.cancelled() => bail!("Orchestration cancelled"),
        }
        drop(publication_guard);

        // 8. bridge readiness（语义与 builtin 一致：无 bridge=编排完成即 ready；
        //    有 bridge 只等指定后端，失败 NotReady 摘流不失败）
        let ready = match &release.bridge_service {
            None => true,
            Some(bridge_id) => match specs.iter().find(|s| &s.service_id == bridge_id) {
                None => {
                    warn!("bridge_service '{bridge_id}' not in services; defaulting to ready");
                    true
                }
                Some(spec) => match tokio::select! {
                    result = supervisor::wait_for_service_ready(spec) => result,
                    () = cancel.cancelled() => bail!("Orchestration cancelled"),
                } {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("bridge '{bridge_id}' not ready: {e}; staying NotReady");
                        false
                    }
                },
            },
        };
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        for (service, name, expected) in &checked_instances {
            let check = async {
                let current = crate::startup_probe::within_budget(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                    Some(cancel),
                    self.client.running_process(name),
                )
                .await?;
                anyhow::ensure!(
                    &current == expected,
                    "program {name} restarted before startup completed"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = check {
                crate::orchestration_events::emit(
                    &crate::orchestration_events::OrchestrationEvent::ServiceStartFail {
                        service: service.clone(),
                        error: format!("startup commit check: {error:#}"),
                    },
                );
                return Err(error).with_context(|| format!("startup commit check for {name}"));
            }
        }
        runtime_status.set_ready(ready);
        ServiceSpecFile::prune_other_generations(&release.release_id);
        let _ = started; // 全部成功；失败路径由外层 stop_all 清理
        Ok(())
    }

    async fn start_checked(
        &self,
        spec: &ServiceSpec,
        name: &str,
        probe: workspace_manifest::StartupProbe,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::xmlrpc::RunningProcess> {
        let deadline = crate::startup_probe::deadline(spec, tokio::time::Instant::now())?;
        let added =
            settle_start_mutation(deadline, cancel, self.client.add_process_group(name)).await?;
        if !added {
            return Err(supervisor::ShutdownUnconfirmed(format!(
                "program {name} was already registered; launch identity is unconfirmed"
            ))
            .into());
        }
        // A stop between the two acknowledged mutations must not launch a new root.
        anyhow::ensure!(!cancel.is_cancelled(), "Startup check cancelled");
        settle_start_mutation(deadline, cancel, self.client.start_process_wait(name)).await?;
        crate::startup_probe::within_budget(deadline, Some(cancel), async {
            // startProcess(wait=true) already observed startsecs=5. Do not wait another 5s.
            let instance = self.client.running_process(name).await?;
            if probe != workspace_manifest::StartupProbe::Process {
                let network = crate::startup_probe::network(spec, probe);
                tokio::pin!(network);
                loop {
                    anyhow::ensure!(
                        self.client.running_process(name).await? == instance,
                        "program {name} restarted during startup check"
                    );
                    tokio::select! {
                        result = &mut network => { result?; break; }
                        () = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
                    }
                }
            }
            anyhow::ensure!(
                self.client.running_process(name).await? == instance,
                "program {name} changed before startup confirmation"
            );
            Ok(instance)
        })
        .await
    }
}

/// Keep an in-flight mutation's response when Stop arrives. A confirmed response
/// allows the caller's normal stop_all cleanup; dropping the HTTP future would
/// unnecessarily turn every cancellation during startsecs into recovery-required.
/// A lost reply still remains uncertain, and cancellation cannot extend the
/// original startup budget. Six seconds covers startsecs=5 plus response latency.
async fn settle_start_mutation<T>(
    deadline: tokio::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    anyhow::ensure!(!cancel.is_cancelled(), "Startup check cancelled");
    tokio::pin!(future);
    let result = tokio::select! {
        biased;
        result = tokio::time::timeout_at(deadline, &mut future) => result,
        () = cancel.cancelled() => {
            let drain_until = deadline.min(tokio::time::Instant::now()
                + std::time::Duration::from_secs(workspace_manifest::PROCESS_STARTUP_OBSERVATION_SECONDS + 1));
            tokio::time::timeout_at(drain_until, &mut future).await
        }
    };
    mutation_result(
        result
            .context("startup mutation response deadline exceeded")
            .and_then(|result| result),
    )
}

/// 编译 pingap 配置（复用 builtin 的编译/校验/原子提交）。
async fn compile_pingap(
    args: &RuntimeArgs,
    release: &ReleaseLock,
    dev_profile: bool,
) -> Result<CompileOutcome> {
    let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir);
    compile_and_validate(
        &args.workspace,
        &runtime_root,
        &args.pingap_bin,
        release,
        dev_profile,
    )
    .await
}

/// pingap 的 spec（argv = pingap -c {config} --autoreload；admin 凭证只进
/// tmpfs spec 的 env，不落持久卷/命令行/日志）。
fn pingap_service_spec(
    args: &RuntimeArgs,
    endpoint: &admin_probe::AdminEndpoint,
) -> ServiceSpecFile {
    // P1 常驻：spec 固定 `resident` 代（不被 prune），`-c` 指向 owner 生命
    // 周期固定的 active 路径——release 发布只原子替换 active，常驻 pingap
    // 经 --autoreload 热载，不随编排代次重建。
    let active = crate::proxy::compiler::active_config_path(&crate::proxy::compiler::runtime_root(
        &args.log_dir,
    ));
    ServiceSpecFile {
        release_id: crate::svc_spec::RESIDENT_SPEC_ID.into(),
        service_id: "pingap".into(),
        cwd: "/".into(),
        argv: vec![
            args.pingap_bin.to_string_lossy().into_owned(),
            "-c".into(),
            active.to_string_lossy().into_owned(),
            "--autoreload".into(),
        ],
        env: [
            ("PINGAP_ADMIN_ADDR".to_string(), endpoint.addr.clone()),
            ("PINGAP_ADMIN_USER".to_string(), endpoint.user.clone()),
            (
                "PINGAP_ADMIN_PASSWORD".to_string(),
                endpoint.password.clone(),
            ),
        ]
        .into_iter()
        .collect(),
        port: None,
    }
}

/// 写 conf 分片（原子：tmp + rename；supervisord reloadConfig 前落盘）。
async fn write_conf(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("conf.tmp");
    tokio::fs::write(&tmp, content)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// 生成动态 program 分片（纯函数，测试覆盖）。
pub(crate) fn render_programs_conf(
    release: &ReleaseLock,
    specs: &[ServiceSpec],
    log_dir: &Path,
    workspace: &Path,
    dev_profile: bool,
) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/usr/local/bin/app-cli".into());
    let rid = &release.release_id;
    let mut out = String::new();
    for spec in specs {
        if !runs_as_process(spec, dev_profile) {
            continue;
        }
        let id = safe_program_token(&spec.service_id);
        let name = format!("{SVC_PROGRAM_PREFIX}{id}");
        let service_log = log_dir.join("services").join(format!("{id}.log"));
        let retries =
            if spec.health.startup_probe == Some(workspace_manifest::StartupProbe::Process) {
                0
            } else {
                10
            };
        out.push_str(&format!(
            "[program:{name}]\n\
             command={exe} run-service {rid} {id}\n\
             directory={}\n\
             autostart=false\n\
             autorestart=true\n\
             startsecs=5\n\
             startretries={retries}\n\
             stopsignal=TERM\n\
             stopasgroup=true\n\
             killasgroup=true\n\
             stopwaitsecs={}\n\
             stdout_logfile={}\n\
             stdout_logfile_maxbytes={LOG_MAXBYTES}\n\
             stdout_logfile_backups={LOG_BACKUPS}\n\
             redirect_stderr=true\n\n",
            workspace.join(&spec.dir).display(),
            spec.run
                .shutdown_timeout_seconds
                .min(crate::supervision::STOP_GRACE_SECONDS),
            service_log.display(),
        ));
    }
    out
}

/// 常驻分片（仅 `app-pingap`）：spec 固定为 `resident/pingap.toml`（不被
/// 代际 prune），command 恒 `run-service resident pingap`——与 release 无关，
/// 崩溃由 supervisord 按同一 spec 复原，业务 stop/换代不撤本分片。
pub(crate) fn render_resident_conf(log_dir: &Path) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/usr/local/bin/app-cli".into());
    let pingap_log = log_dir.join("services").join("pingap.log");
    format!(
        "[program:{PINGAP_PROGRAM}]\n\
         command={exe} run-service {} pingap\n\
         directory=/\n\
         autostart=false\n\
         autorestart=true\n\
         startsecs=3\n\
         startretries=10\n\
         stopsignal=TERM\n\
         stopasgroup=true\n\
         killasgroup=true\n\
         stopwaitsecs=3\n\
         stdout_logfile={}\n\
         stdout_logfile_maxbytes={LOG_MAXBYTES}\n\
         stdout_logfile_backups={LOG_BACKUPS}\n\
         redirect_stderr=true\n",
        crate::svc_spec::RESIDENT_SPEC_ID,
        pingap_log.display(),
    )
}

/// Keep spec files, rendered programs, stale-group cleanup and process startup
/// on the same selection. A static frontend with devrun is a process only in dev.
fn runs_as_process(spec: &ServiceSpec, dev_profile: bool) -> bool {
    !crate::static_hosting::hosts_statically(spec, dev_profile)
        && !supervisor::effective_run_argv(spec, dev_profile).is_empty()
}

/// program 名/参数 token 白名单（防 conf 注入——service_id 本已过 manifest
/// identifier 校验，此处为纵深防御）。
fn safe_program_token(raw: &str) -> &str {
    if raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !raw.is_empty()
    {
        raw
    } else {
        warn!("service id '{raw}' contains unsafe chars; conf generation refused");
        ""
    }
}

// A lost reply to a mutation is not proof that supervisord stopped executing it.
fn mutation_result<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if crate::xmlrpc::is_confirmed_fault(&error) {
            error
        } else {
            supervisor::ShutdownUnconfirmed(format!(
                "Supervisord mutation response is unconfirmed: {error:#}"
            ))
            .into()
        }
    })
}

#[cfg(test)]
include!("tests.rs");
