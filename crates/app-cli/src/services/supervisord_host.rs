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
use crate::xmlrpc::SupervisorClient;

/// 动态 program 名前缀（与镜像固定 program 命名空间隔离）。
pub(crate) const SVC_PROGRAM_PREFIX: &str = "app-svc-";
pub(crate) const PINGAP_PROGRAM: &str = "app-pingap";
/// 动态分片文件（conf.d 通配吸入；50 排在固定服务之后无实际顺序意义——
/// 启停顺序由 server 显式 startProcess 控制）。
const CONF_PATH: &str = "/etc/supervisor/conf.d/50-app-services.conf";
/// 常驻分片文件（P1：app-pingap 独立于业务代次——业务 stop/换代不撤此
/// 分片，supervisord 重启也能按同名 spec 复原常驻组）。
const RESIDENT_CONF_PATH: &str = "/etc/supervisor/conf.d/51-app-cli-resident.conf";

/// 日志轮转对齐 builtin 引擎（10MB/3 份）。
const LOG_MAXBYTES: &str = "10MB";
const LOG_BACKUPS: &str = "3";

pub(crate) struct SupervisordHost {
    client: SupervisorClient,
    conf_path: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineReceipt {
    generation: String,
    supervisor_id: String,
    socket: PathBuf,
}

const ENGINE_RECEIPT: &str = "supervisord-engine.json";

/// Record the structured outcome and return it as the error cause; the
/// parent cleanup reads the sidecar for its own classification.
fn record_outcome(root: &Path, outcome: runtime_supervisor::CleanupOutcome) -> anyhow::Error {
    let detail = format!("{outcome:?}");
    if let Err(error) = outcome.record(root) {
        return anyhow::anyhow!("persist cleanup outcome failed: {error:#}");
    }
    anyhow::anyhow!("{detail}")
}

impl SupervisordHost {
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

    /// Called only after the guardian authenticated the draining generation.
    /// Returns the structured physical-cleanup outcome (recovery v2 §7.1) and
    /// persists it beside the receipts; business results are never inferred.
    pub(crate) async fn cleanup_generation(
        root: &Path,
    ) -> Result<runtime_supervisor::CleanupOutcome> {
        match std::fs::read(root.join(ENGINE_RECEIPT)) {
            Ok(bytes) => {
                let receipt: EngineReceipt = match serde_json::from_slice(&bytes) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        return Err(record_outcome(
                            root,
                            runtime_supervisor::CleanupOutcome::ObservationFailed {
                                reason: format!("decode engine receipt: {error}"),
                            },
                        ));
                    }
                };
                let generation: serde_json::Value =
                    match serde_json::from_slice(&std::fs::read(root.join("generation.json"))?) {
                        Ok(value) => value,
                        Err(error) => {
                            return Err(record_outcome(
                                root,
                                runtime_supervisor::CleanupOutcome::ObservationFailed {
                                    reason: format!("decode generation receipt: {error}"),
                                },
                            ));
                        }
                    };
                if !(generation["id"] == receipt.generation
                    && generation["supervisor"] == receipt.supervisor_id)
                {
                    // 记录属于另一管理域（前容器/换代）——保留为历史，不在
                    // 本域执行引擎清理（plan §7.4：不考古旧本地进程）。
                    return Err(record_outcome(
                        root,
                        runtime_supervisor::CleanupOutcome::ForeignIdentity {
                            detail: format!(
                                "supervisord cleanup generation identity differs: engine {}",
                                receipt.generation
                            ),
                        },
                    ));
                }
                let client = SupervisorClient::new(receipt.socket);
                if let Err(error) = client.ping().await {
                    return Err(record_outcome(
                        root,
                        runtime_supervisor::CleanupOutcome::ObservationFailed {
                            reason: format!(
                                "recorded supervisord engine is unavailable: {error:#}"
                            ),
                        },
                    ));
                }
                Self {
                    client,
                    conf_path: CONF_PATH.into(),
                }
                // 换代清理不发布 standby：入口保持当前配置（接管方的编排
                // 会发布新 active）；发布反而会与接管方竞态。
                .stop_all(None)
                .await?;
                runtime_supervisor::CleanupOutcome::Empty.record(root)?;
                Ok(runtime_supervisor::CleanupOutcome::Empty)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // No external-engine mutation was accepted by this worker.
                // Existing installations may still have a reachable engine.
                if let Some(host) = Self::detect().await? {
                    host.stop_all(None).await?;
                }
                runtime_supervisor::CleanupOutcome::Empty.record(root)?;
                Ok(runtime_supervisor::CleanupOutcome::Empty)
            }
            Err(error) => Err(record_outcome(
                root,
                runtime_supervisor::CleanupOutcome::ObservationFailed {
                    reason: format!("read supervisord engine receipt: {error}"),
                },
            )),
        }
    }

    /// 停掉全部**业务**动态组（`app-svc-*`；热部署切换 / 容器停服级联）。
    ///
    /// P1 常驻语义（V2-03/V2-07）：`app-pingap` 不在停止集——入口常驻。
    /// `standby_root = Some(runtime_root)` 时先发布 standby 到 active 并
    /// 经 admin hash 确认热载生效，**然后**才停止业务（摘流顺序；发布或
    /// 确认失败 = 停机失败返回 Err，旧服务保持可证实地运行）。`None` 用于
    /// 拆除/换代清理路径（入口保持当前配置，由接管方发布新 active）。
    pub(crate) async fn stop_all(&self, standby_root: Option<&Path>) -> Result<()> {
        if let Some(runtime_root) = standby_root {
            let publication = uuid::Uuid::new_v4().simple().to_string();
            let expected = crate::proxy::compiler::publish_standby(runtime_root, &publication)
                .await
                .context("publish standby before stopping business services")?;
            let endpoint = admin_probe::ensure_admin_endpoint();
            admin_probe::wait_for_config_hash(endpoint, &expected, admin_probe::CONFIRM_BUDGET)
                .await
                .context("confirm standby hot-reload before stopping business services")?;
            info!("🛑 standby confirmed (publication {publication}); stopping business services");
        }
        // Remove the restart source before stopping the live groups. Keeping the
        // old fragment after removeProcessGroup lets a supervisord restart or a
        // later reload resurrect the retired release. Do not touch fixed groups
        // and the resident fragment (app-pingap).
        let mut failures = Vec::new();
        if let Err(error) =
            write_conf(&self.conf_path, "# app-cli dynamic services stopped\n").await
        {
            failures.push(format!("withdraw dynamic configuration: {error:#}"));
        }
        if let Err(error) = mutation_result(self.client.reload_config().await) {
            failures.push(format!("reload dynamic configuration: {error:#}"));
        }
        // A broken configuration must not prevent stopping live groups. Keep
        // the withdrawal failure, but attempt the physical stop before returning
        // it; an external cleanup receipt still requires every part to succeed.
        let groups = self.dynamic_groups().await?;
        // Begin shutdown together; one slow service must not multiply the grace
        // period by the number of application modules.
        for (name, result) in futures::future::join_all(
            groups
                .iter()
                .map(|name| async move { (name, self.client.stop_remove_group(name).await) }),
        )
        .await
        {
            if let Err(error) = result {
                failures.push(format!("{name}: {error:#}"));
            }
        }
        if !failures.is_empty() {
            bail!(
                "dynamic groups did not stop or configuration withdrawal failed: {}",
                failures.join("; ")
            );
        }
        let remaining = self.dynamic_groups().await?;
        if !remaining.is_empty() {
            bail!("dynamic groups remain after stop: {}", remaining.join(", "));
        }
        Ok(())
    }

    /// 当前动态业务组名集合（`app-svc-*`；P1 起 `app-pingap` 为常驻组，
    /// 不属业务停止/清理范围——入口生命周期独立于编排代次）。
    async fn dynamic_groups(&self) -> Result<Vec<String>> {
        Ok(self
            .managed_groups()
            .await?
            .into_iter()
            .filter(|group| group != PINGAP_PROGRAM)
            .collect())
    }

    /// app-cli 托管的全部组（业务 `app-svc-*` + 常驻 `app-pingap`）——
    /// 编排就绪检查用（判定常驻组是否已被 supervisord 托管）。
    async fn managed_groups(&self) -> Result<Vec<String>> {
        let infos = self.client.get_all_process_info().await?;
        let mut groups = std::collections::BTreeSet::new();
        for info in infos {
            let group = info
                .get("group")
                .and_then(|group| group.as_str())
                .filter(|group| !group.is_empty())
                .context("supervisord process info has no group identity")?;
            if group.starts_with(SVC_PROGRAM_PREFIX) || group == PINGAP_PROGRAM {
                groups.insert(group.to_owned());
            }
        }
        Ok(groups.into_iter().collect())
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
        runtime_status.set_ready(false);
        let supervisor::RunProfile {
            run_migrations,
            dev_profile,
            pg,
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

        // 记录旧代组（换代码前——reload 后按新集合差量摘除）
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let previous_groups = self.dynamic_groups().await?;

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

        // 2. pingap 配置编译（生成/校验/原子提交；未就绪前不启动）+
        //    发布到 active（P1：release 目录为候选，active 为常驻 -c 目标；
        //    已运行的常驻 pingap 由 --autoreload 周期轮询拾取）。
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let pingap_outcome = compile_pingap(args, release, dev_profile).await?;
        let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir);
        crate::proxy::compiler::publish_active(&runtime_root, &pingap_outcome.config_path)
            .await
            .context("publish active pingap config")?;
        let endpoint = admin_probe::ensure_admin_endpoint();

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
        pingap_spec.write().context("write resident pingap spec")?;
        ServiceSpecFile::prune_other_generations(&release.release_id);
        // 常驻分片幂等覆写（每次编排重写同一内容——凭证/active 路径变化时
        // 随 reloadConfig 生效；分片不撤，组不删）。
        let resident_conf = render_resident_conf(&args.log_dir);
        write_conf(Path::new(RESIDENT_CONF_PATH), &resident_conf).await?;

        // 4. 生成并重载 program conf（supervisord 不建日志目录——服务日志目录预建）
        let services_log_dir = args.log_dir.join("services");
        tokio::fs::create_dir_all(&services_log_dir)
            .await
            .with_context(|| format!("create {}", services_log_dir.display()))?;
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let conf =
            render_programs_conf(release, &specs, &args.log_dir, &args.workspace, dev_profile);
        write_conf(&self.conf_path, &conf).await?;
        mutation_result(self.client.reload_config().await).context("supervisord reloadConfig")?;

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

        // 7. 常驻 pingap 就绪 + 配置 hash 确认（与 builtin 的 start_pingap
        //    确认语义一致）。组已存在（跨编排代次存活或 supervisord 重启
        //    复原）则跳过 add/start——active 已发布，--autoreload 拾取。
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        let resident_running = self
            .managed_groups()
            .await?
            .contains(&PINGAP_PROGRAM.to_string());
        if resident_running {
            info!(
                "🛰️  resident {PINGAP_PROGRAM} already managed; relying on autoreload for active config"
            );
        } else {
            mutation_result(self.client.add_process_group(PINGAP_PROGRAM).await)?;
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
            mutation_result(self.client.start_process_wait(PINGAP_PROGRAM).await)
                .context("start app-pingap")?;
        }
        started.push(PINGAP_PROGRAM.to_string());
        tokio::select! {
            result = admin_probe::wait_for_config_hash(
            endpoint,
            &pingap_outcome.expected_hash,
            admin_probe::CONFIRM_BUDGET,
        ) => {
                result.context("pingap config hash confirm")?;
                // 已确认生效：业务就绪观察以此核对 admin 实际 hash。
                crate::proxy::compiler::record_expected_hash(&pingap_outcome.expected_hash);
            }
            () = cancel.cancelled() => bail!("Orchestration cancelled"),
        }

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
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_start_keeps_acknowledgement_but_lost_reply_stays_uncertain() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for reply in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("supervisor.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let cancel = tokio_util::sync::CancellationToken::new();
            let stop = cancel.clone();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                // Stop is accepted after the mutation reached the server, before
                // its response. Closing the socket simulates a lost response.
                stop.cancel();
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                if reply {
                    let body = "<methodResponse><params><param><value><boolean>1</boolean></value></param></params></methodResponse>";
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            });
            let client = SupervisorClient::new(path);
            let result = settle_start_mutation(
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                &cancel,
                client.start_process_wait("app-svc-worker"),
            )
            .await;
            if reply {
                assert!(
                    result.is_ok(),
                    "acknowledged start must allow cleanup: {result:?}"
                );
            } else {
                assert!(result.unwrap_err().is::<supervisor::ShutdownUnconfirmed>());
            }
            server.await.unwrap();
        }
    }

    /// B08：supervisord 引擎与 builtin 同一生效命令选择——dev profile 且
    /// 配 [devrun] 时 devrun.command 优先（不再恒用 run.command）。
    #[test]
    fn effective_argv_prefers_devrun_in_dev_profile() {
        let mut spec = spec("web", "web", 30);
        spec.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["pnpm".into(), "dev".into()],
        });
        // 非 dev profile：run 兜底
        assert_eq!(
            crate::supervisor::effective_run_argv(&spec, false),
            &spec.run.command
        );
        // dev profile：devrun 优先
        assert_eq!(
            crate::supervisor::effective_run_argv(&spec, true),
            &["pnpm".to_string(), "dev".to_string()]
        );
    }

    /// B08：devrun-only 服务（run.command 为空）在 dev profile 下可启动；
    /// 非 dev profile 判空跳过（生效命令选择同源）。
    #[test]
    fn devrun_only_service_starts_only_in_dev_profile() {
        let mut spec = spec("web", "web", 30);
        spec.run.command = Vec::new();
        spec.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["vite".into(), "--host".into()],
        });
        assert!(
            crate::supervisor::effective_run_argv(&spec, true)
                == ["vite".to_string(), "--host".to_string()]
        );
        assert!(crate::supervisor::effective_run_argv(&spec, false).is_empty());
    }

    fn spec(id: &str, dir: &str, shutdown: u64) -> ServiceSpec {
        let mut s = toml::from_str::<ReleaseLock>(
            r#"
schema_version = 1
release_id = "rel-t"
workspace_name = "ws"
minimum_app_cli_version = "0.0.0"
runtime_image_digest = ""

[pingap]
mode = "managed"
version = "0.14.3"
commit = "abc"

[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200

[services.run]
command = ["node", "server.js"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"

[[services.logs]]
id = "application"
glob = "web*.log*"
format = "text"

[services.env]
NODE_ENV = "production"
"#,
        )
        .unwrap();
        let svc = &mut s.services[0];
        svc.service_id = id.into();
        svc.dir = dir.into();
        svc.run.shutdown_timeout_seconds = shutdown;
        s.services.remove(0)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recorded_engine_loss_never_becomes_builtin_cleanup_success() {
        let root = tempfile::tempdir().unwrap();
        let receipt = EngineReceipt {
            generation: "original-generation".into(),
            supervisor_id: "original-owner".into(),
            socket: root.path().join("missing-supervisord.sock"),
        };
        std::fs::write(
            root.path().join(ENGINE_RECEIPT),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let generation = root.path().join("generation.json");
        std::fs::write(
            &generation,
            r#"{"id":"original-generation","supervisor":"original-owner"}"#,
        )
        .unwrap();
        let error = SupervisordHost::cleanup_generation(root.path())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("engine is unavailable"));
        std::fs::write(
            generation,
            r#"{"id":"replacement","supervisor":"original-owner"}"#,
        )
        .unwrap();
        let error = SupervisordHost::cleanup_generation(root.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("identity differs"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_withdraws_dynamic_restart_source_and_preserves_fixed_programs() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("supervisor.sock");
        let conf = root.path().join("dynamic.conf");
        std::fs::write(&conf, "[program:app-svc-web]\nautorestart=true\n").unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let group = |name: &str| {
            format!(
                "<value><struct><member><name>group</name><value><string>{name}</string></value></member></struct></value>"
            )
        };
        let array = |values: String| format!("<array><data>{values}</data></array>");
        let fixed = array(group("postgres"));
        // P1：app-pingap 为常驻组——停止序列只含业务组 app-svc-web，
        // 常驻组与固定组（postgres）同样不被触碰。
        let requests = vec![
            ("reloadConfig", "", array(String::new())),
            (
                "getAllProcessInfo",
                "",
                array(group("postgres") + &group("app-pingap") + &group("app-svc-web")),
            ),
            (
                "stopProcessGroup",
                "app-svc-web",
                "<boolean>1</boolean>".into(),
            ),
            (
                "removeProcessGroup",
                "app-svc-web",
                "<boolean>1</boolean>".into(),
            ),
            ("getAllProcessInfo", "", fixed.clone()),
            ("reloadConfig", "", array(String::new())),
            ("getAllProcessInfo", "", fixed.clone()),
            ("getAllProcessInfo", "", fixed),
        ];
        let checked_conf = conf.clone();
        let server = tokio::spawn(async move {
            let mut pending = requests;
            let mut stopped = std::collections::BTreeSet::new();
            while !pending.is_empty() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(stream.read_u8().await.unwrap());
                }
                let header = String::from_utf8(header).unwrap();
                let size: usize = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut bytes = vec![0; size];
                stream.read_exact(&mut bytes).await.unwrap();
                let request = String::from_utf8(bytes).unwrap();
                // Groups now stop concurrently. Preserve per-group stop/remove
                // ordering while allowing either group to make progress first.
                let index = if matches!(pending[0].0, "stopProcessGroup" | "removeProcessGroup") {
                    pending
                        .iter()
                        .position(|(method, target, _)| {
                            request
                                .contains(&format!("<methodName>supervisor.{method}</methodName>"))
                                && request.contains(&format!("<string>{target}</string>"))
                        })
                        .expect("expected an outstanding group request")
                } else {
                    0
                };
                let (method, target, value) = pending.remove(index);
                assert!(
                    request.contains(&format!("<methodName>supervisor.{method}</methodName>")),
                    "{request}"
                );
                if method == "stopProcessGroup" {
                    assert!(stopped.insert(target));
                } else if method == "removeProcessGroup" {
                    assert!(
                        stopped.remove(target),
                        "remove must follow stop for this group"
                    );
                }
                if !target.is_empty() {
                    assert!(request.contains(&format!("<string>{target}</string>")));
                }
                assert!(
                    !request.contains("<string>postgres</string>"),
                    "must not stop fixed PG"
                );
                assert!(
                    !matches!(
                        (method, target),
                        ("stopProcessGroup", "app-pingap") | ("removeProcessGroup", "app-pingap")
                    ),
                    "resident proxy group must never be stopped or removed"
                );
                assert!(
                    !std::fs::read_to_string(&checked_conf)
                        .unwrap()
                        .contains("[program:"),
                    "withdraw config before RPC"
                );
                let body = format!(
                    "<methodResponse><params><param><value>{value}</value></param></params></methodResponse>"
                );
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let host = SupervisordHost {
            client: SupervisorClient::new(socket),
            conf_path: conf,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            host.stop_all(None).await.unwrap();
            host.stop_all(None).await.unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[test]
    #[cfg(unix)] // supervisord 引擎 Unix-only（xmlrpc 走 Unix socket）；断言含 Unix 路径字符串
    fn renders_service_and_pingap_programs() {
        let specs = vec![spec("web", "web", 45)];
        let release = ReleaseLock {
            schema_version: 1,
            release_id: "rel-t".into(),
            workspace_name: "ws".into(),
            pingap: workspace_manifest::LockedPingap {
                mode: workspace_manifest::PingapMode::Managed,
                config: None,
                version: "0.14.3".into(),
                commit: "abc".into(),
            },
            minimum_app_cli_version: "0.0.0".into(),
            runtime_image_digest: String::new(),
            services: specs.clone(),
            bridge_service: None,
        };
        let conf = render_programs_conf(
            &release,
            &specs,
            Path::new("/app/logs"),
            Path::new("/app/code"),
            false,
        );
        assert!(conf.contains("[program:app-svc-web]"));
        assert!(conf.contains("run-service rel-t web"));
        assert!(conf.contains("stopwaitsecs=3"));
        assert!(conf.contains("directory=/app/code/web"));
        assert!(conf.contains("stdout_logfile=/app/logs/services/web.log"));
        assert!(conf.contains("redirect_stderr=true"));
        // P1：pingap program 移入常驻分片——业务分片只含 app-svc-*
        assert!(!conf.contains("[program:app-pingap]"));
        let resident = render_resident_conf(Path::new("/app/logs"));
        assert!(resident.contains("[program:app-pingap]"));
        assert!(resident.contains("run-service resident pingap"));
        assert!(resident.contains("stdout_logfile=/app/logs/services/pingap.log"));
        // autostart=false：启动顺序由 server 显式控制（依赖序）
        assert_eq!(conf.matches("autostart=false").count(), 1);

        // Frontend templates are static in prod, but must get a real Vite
        // program in dev even though [run].command is empty.
        let mut frontend = specs[0].clone();
        frontend.r#type = workspace_manifest::ProjectType::Static;
        frontend.run.command.clear();
        frontend.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["pnpm".into(), "dev".into()],
        });
        let dev_conf = render_programs_conf(
            &release,
            std::slice::from_ref(&frontend),
            Path::new("/app/logs"),
            Path::new("/app/code"),
            true,
        );
        assert!(dev_conf.contains("[program:app-svc-web]"));
        assert!(dev_conf.contains("run-service rel-t web"));

        // The same service becomes an in-process static host in production,
        // even when a stale [run] command is present in the manifest.
        frontend.run.command = vec!["must-not-run".into()];
        let prod_conf = render_programs_conf(
            &release,
            std::slice::from_ref(&frontend),
            Path::new("/app/logs"),
            Path::new("/app/code"),
            false,
        );
        assert!(!prod_conf.contains("[program:app-svc-web]"));
        assert!(!prod_conf.contains("[program:app-pingap]"));

        // No devrun means static hosting in both profiles; do not invent a
        // process from the stale command either.
        frontend.devrun = None;
        assert!(!runs_as_process(&frontend, true));
        assert!(!runs_as_process(&frontend, false));
    }

    #[test]
    fn safe_token_rejects_injection() {
        assert_eq!(safe_program_token("web-1_2.3"), "web-1_2.3");
        assert_eq!(safe_program_token("a b"), "");
        assert_eq!(safe_program_token("a\nb"), "");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_group_fault_is_not_swallowed_as_success() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for step in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 8192];
                let size = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                let body = if step == 0 {
                    assert!(request.contains("supervisor.reloadConfig"));
                    // An unrelated malformed config cannot skip the stop RPC.
                    r#"<methodResponse><fault><value><struct><member><name>faultString</name><value><string>invalid fixed supervisor config</string></value></member></struct></value></fault></methodResponse>"#
                } else if step == 2 {
                    assert!(request.contains("supervisor.stopProcessGroup"));
                    r#"<methodResponse><fault><value><struct><member><name>faultString</name><value><string>stop failed</string></value></member></struct></value></fault></methodResponse>"#
                } else {
                    assert!(request.contains("supervisor.getAllProcessInfo"));
                    r#"<methodResponse><params><param><value><array><data><value><struct><member><name>group</name><value><string>app-svc-web</string></value></member></struct></value></data></array></value></param></params></methodResponse>"#
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let host = SupervisordHost {
            client: SupervisorClient::new(path),
            conf_path: root.path().join("unused.conf"),
        };
        let error = host.stop_all(None).await.unwrap_err().to_string();
        assert!(error.contains("did not stop"));
        assert!(error.contains("invalid fixed supervisor config"));
        assert!(error.contains("stop failed"));
        server.await.unwrap();
    }
}
