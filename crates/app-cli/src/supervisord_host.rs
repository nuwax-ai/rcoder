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

use crate::config::CliArgs;
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

/// 日志轮转对齐 builtin 引擎（10MB/3 份）。
const LOG_MAXBYTES: &str = "10MB";
const LOG_BACKUPS: &str = "3";

pub(crate) struct SupervisordHost {
    client: SupervisorClient,
    conf_path: PathBuf,
}

impl SupervisordHost {
    /// 引擎探测：socket 存在 + XML-RPC ping 成功（serve 启动时调用一次）。
    pub(crate) async fn detect() -> Option<Self> {
        if !crate::xmlrpc::socket_exists() {
            return None;
        }
        let client = SupervisorClient::new(crate::xmlrpc::default_socket_path());
        match client.ping().await {
            Ok(version) => {
                info!("service host: supervisord {version} detected (engine=supervisord)");
                Some(Self {
                    client,
                    conf_path: PathBuf::from(CONF_PATH),
                })
            }
            Err(e) => {
                warn!(
                    "service host: supervisord socket exists but ping failed: {e:#} (engine=builtin)"
                );
                None
            }
        }
    }

    /// 停掉全部动态组（热部署切换 / 容器停服级联）。
    pub(crate) async fn stop_all(&self) -> Result<()> {
        for name in self.dynamic_groups().await? {
            if let Err(e) = self.client.stop_remove_group(&name).await {
                warn!("stop dynamic group {name} failed (continue): {e:#}");
            }
        }
        Ok(())
    }

    /// 当前动态组名集合（app-svc-* / app-pingap）。
    async fn dynamic_groups(&self) -> Result<Vec<String>> {
        let infos = self.client.get_all_process_info().await?;
        Ok(infos
            .iter()
            .filter_map(|info| info.get("group").and_then(|g| g.as_str()))
            .filter(|g| g.starts_with(SVC_PROGRAM_PREFIX) || *g == PINGAP_PROGRAM)
            .map(str::to_string)
            .collect())
    }

    /// 编排：migrate → 写 specs/conf → reload → 旧组摘除 → 依赖序启动 →
    /// pingap 确认 → bridge readiness。成功返回后服务由 supervisord 托管
    ///（崩溃自动重启，与 server 进程生命周期解耦）。
    pub(crate) async fn orchestrate(
        &self,
        args: &CliArgs,
        release: &ReleaseLock,
        runtime_status: &RuntimeStatusService,
    ) -> Result<()> {
        runtime_status.set_ready(false);
        supervisor::validate_runtime_compatibility(release)?;
        let specs: Vec<ServiceSpec> = release
            .services
            .iter()
            .filter(|service| service.enabled)
            .cloned()
            .collect();
        if specs.is_empty() {
            bail!("release has no enabled services");
        }
        supervisor::wait_for_pg().await?;

        // 记录旧代组（换代码前——reload 后按新集合差量摘除）
        let previous_groups = self.dynamic_groups().await?;

        // 1. migrate（Fail Fast，与 builtin 同语义）
        for spec in &specs {
            if !spec.run.migrate.is_empty() {
                info!("🛠️  migrate {}", spec.service_id);
                let cwd = args.workspace.join(&spec.dir);
                supervisor::run_transient(&spec.run.migrate, &cwd)
                    .await
                    .with_context(|| format!("migrate {}", spec.service_id))?;
            }
        }

        // 2. pingap 配置编译（生成/校验/原子提交；未就绪前不启动）
        let pingap_outcome = compile_pingap(args, release).await?;
        let endpoint = admin_probe::ensure_admin_endpoint();

        // 3. 写 per-service specs（run-service 启动契约；pingap 同机制承载凭证）
        let mut started: Vec<String> = Vec::new();
        for spec in &specs {
            let svc_spec = ServiceSpecFile {
                release_id: release.release_id.clone(),
                service_id: spec.service_id.clone(),
                cwd: args
                    .workspace
                    .join(&spec.dir)
                    .to_string_lossy()
                    .into_owned(),
                argv: spec.run.command.clone(),
                env: spec.env.clone().into_iter().collect(),
                port: Some(spec.port),
            };
            svc_spec
                .write()
                .with_context(|| format!("write spec {}", spec.service_id))?;
        }
        let pingap_spec = pingap_service_spec(args, release, &pingap_outcome, endpoint);
        pingap_spec.write().context("write pingap spec")?;
        ServiceSpecFile::prune_other_generations(&release.release_id);

        // 4. 生成并重载 program conf（supervisord 不建日志目录——服务日志目录预建）
        std::fs::create_dir_all(args.log_dir.join("services"))
            .with_context(|| format!("create {}", args.log_dir.join("services").display()))?;
        let conf = render_programs_conf(release, &specs, &args.log_dir, &args.workspace);
        write_conf(&self.conf_path, &conf)?;
        if let Err(e) = self.client.reload_config().await {
            bail!("supervisord reloadConfig: {e:#}");
        }

        // 5. 旧代差量摘除（不在新集合的组）
        let new_names: Vec<String> = specs
            .iter()
            .map(|s| format!("{SVC_PROGRAM_PREFIX}{}", s.service_id))
            .chain([PINGAP_PROGRAM.to_string()])
            .collect();
        for old in &previous_groups {
            if !new_names.contains(old)
                && let Err(e) = self.client.stop_remove_group(old).await
            {
                warn!("prune stale group {old} failed (continue): {e:#}");
            }
        }

        // 6. 依赖序启动（lock services 顺序即拓扑序；startProcessWait 等 startsecs）
        for spec in &specs {
            if spec.run.command.is_empty() {
                warn!("⚠️  {} 无 [run].command，跳过", spec.service_id);
                continue;
            }
            let name = format!("{SVC_PROGRAM_PREFIX}{}", spec.service_id);
            self.client.add_process_group(&name).await?;
            self.client
                .start_process_wait(&name)
                .await
                .with_context(|| format!("start {name}"))?;
            started.push(name);
        }

        // 7. pingap 启动 + 配置 hash 确认（与 builtin 的 start_pingap 确认语义一致）
        self.client.add_process_group(PINGAP_PROGRAM).await?;
        self.client
            .start_process_wait(PINGAP_PROGRAM)
            .await
            .context("start app-pingap")?;
        started.push(PINGAP_PROGRAM.to_string());
        admin_probe::wait_for_config_hash(
            endpoint,
            &pingap_outcome.expected_hash,
            admin_probe::CONFIRM_BUDGET,
        )
        .await
        .context("pingap config hash confirm")?;

        // 8. bridge readiness（语义与 builtin 一致：无 bridge=编排完成即 ready；
        //    有 bridge 只等指定后端，失败 NotReady 摘流不失败）
        let ready = match &release.bridge_service {
            None => true,
            Some(bridge_id) => match specs.iter().find(|s| &s.service_id == bridge_id) {
                None => {
                    warn!("bridge_service '{bridge_id}' not in services; defaulting to ready");
                    true
                }
                Some(spec) => match supervisor::wait_for_service_ready(spec).await {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("bridge '{bridge_id}' not ready: {e}; staying NotReady");
                        false
                    }
                },
            },
        };
        runtime_status.set_ready(ready);
        let _ = started; // 全部成功；失败路径由外层 stop_all 清理
        Ok(())
    }
}

/// 编译 pingap 配置（复用 builtin 的编译/校验/原子提交）。
async fn compile_pingap(args: &CliArgs, release: &ReleaseLock) -> Result<CompileOutcome> {
    let runtime_root = std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/run/app-cli/pingap".into());
    compile_and_validate(&args.workspace, &runtime_root, &args.pingap_bin, release).await
}

/// pingap 的 spec（argv = pingap -c {config} --autoreload；admin 凭证只进
/// tmpfs spec 的 env，不落持久卷/命令行/日志）。
fn pingap_service_spec(
    args: &CliArgs,
    release: &ReleaseLock,
    outcome: &CompileOutcome,
    endpoint: &admin_probe::AdminEndpoint,
) -> ServiceSpecFile {
    ServiceSpecFile {
        release_id: release.release_id.clone(),
        service_id: "pingap".into(),
        cwd: "/".into(),
        argv: vec![
            args.pingap_bin.to_string_lossy().into_owned(),
            "-c".into(),
            outcome.config_path.to_string_lossy().into_owned(),
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
fn write_conf(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// 生成动态 program 分片（纯函数，测试覆盖）。
pub(crate) fn render_programs_conf(
    release: &ReleaseLock,
    specs: &[ServiceSpec],
    log_dir: &Path,
    workspace: &Path,
) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/usr/local/bin/app-cli".into());
    let rid = &release.release_id;
    let mut out = String::new();
    for spec in specs {
        let id = safe_program_token(&spec.service_id);
        let name = format!("{SVC_PROGRAM_PREFIX}{id}");
        let service_log = log_dir.join("services").join(format!("{id}.log"));
        out.push_str(&format!(
            "[program:{name}]\n\
             command={exe} run-service {rid} {id}\n\
             directory={}\n\
             autostart=false\n\
             autorestart=true\n\
             startsecs=5\n\
             startretries=10\n\
             stopsignal=TERM\n\
             stopasgroup=true\n\
             killasgroup=true\n\
             stopwaitsecs={}\n\
             stdout_logfile={}\n\
             stdout_logfile_maxbytes={LOG_MAXBYTES}\n\
             stdout_logfile_backups={LOG_BACKUPS}\n\
             redirect_stderr=true\n\n",
            workspace.join(&spec.dir).display(),
            spec.run.shutdown_timeout_seconds,
            service_log.display(),
        ));
    }
    let pingap_log = log_dir.join("services").join("pingap.log");
    out.push_str(&format!(
        "[program:{PINGAP_PROGRAM}]\n\
         command={exe} run-service {rid} pingap\n\
         directory={}\n\
         autostart=false\n\
         autorestart=true\n\
         startsecs=3\n\
         startretries=10\n\
         stopsignal=TERM\n\
         stopasgroup=true\n\
         killasgroup=true\n\
         stopwaitsecs=15\n\
         stdout_logfile={}\n\
         stdout_logfile_maxbytes={LOG_MAXBYTES}\n\
         stdout_logfile_backups={LOG_BACKUPS}\n\
         redirect_stderr=true\n",
        workspace.display(),
        pingap_log.display(),
    ));
    out
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

#[cfg(test)]
mod tests {
    use super::*;

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
version = "0.13.9"
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

    #[test]
    fn renders_service_and_pingap_programs() {
        let specs = vec![spec("web", "web", 45)];
        let release = ReleaseLock {
            schema_version: 1,
            release_id: "rel-t".into(),
            workspace_name: "ws".into(),
            pingap: workspace_manifest::LockedPingap {
                mode: workspace_manifest::PingapMode::Managed,
                config: None,
                version: "0.13.9".into(),
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
        );
        assert!(conf.contains("[program:app-svc-web]"));
        assert!(conf.contains("run-service rel-t web"));
        assert!(conf.contains("stopwaitsecs=45"));
        assert!(conf.contains("directory=/app/code/web"));
        assert!(conf.contains("stdout_logfile=/app/logs/services/web.log"));
        assert!(conf.contains("redirect_stderr=true"));
        assert!(conf.contains("[program:app-pingap]"));
        assert!(conf.contains("stdout_logfile=/app/logs/services/pingap.log"));
        // autostart=false：启动顺序由 server 显式控制（依赖序）
        assert_eq!(conf.matches("autostart=false").count(), 2);
    }

    #[test]
    fn safe_token_rejects_injection() {
        assert_eq!(safe_program_token("web-1_2.3"), "web-1_2.3");
        assert_eq!(safe_program_token("a b"), "");
        assert_eq!(safe_program_token("a\nb"), "");
    }
}
