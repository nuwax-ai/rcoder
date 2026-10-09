//! P1 常驻 pingap（builtin 引擎）：serve 进程生命周期持有，业务编排代次
//! 重建不再杀/拉入口进程。
//!
//! 归属（V2-03/RV-03）：进程注册到 **owner 会话作用域**（`spawn_guarded`
//! 显式 work_root）——不继承业务 generation 的 task-local CommandContext，
//! 业务代次退出/清理不收束它；owner 退出时由 owner 域的 guardian 清理
//! 兜底。直跑形态（`app-cli run`，无 owner 会话作用域）不走本模块——
//! 单次编排的进程域维持会话树托管（树外进程会在 run() 返回后成为孤儿）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use process_utils::guardian::{self, OwnedChild};
use tokio::sync::Mutex;

use crate::proxy::admin_probe;

use super::RuntimeArgs;

pub(super) struct ResidentProxy {
    /// owner 域 guardian 托管的入口进程（非业务会话树成员）。
    child: OwnedChild,
}

static RESIDENT: Mutex<Option<ResidentProxy>> = Mutex::const_new(None);

/// owner 会话作用域根（serve 形态恒有；直跑形态 None → 调用方走会话树）。
pub(super) fn owner_scope_root() -> Option<PathBuf> {
    if let Some(scope) = runtime_supervisor::current_scope() {
        return Some(scope.work_root);
    }
    // 旧 worker 链（env 声明）：root 的祖父即工作范围（与引擎探测同语义）。
    std::env::var_os(runtime_supervisor::WORKER_ENV)
        .map(PathBuf::from)
        .and_then(|root| root.parent().and_then(Path::parent).map(Path::to_path_buf))
}

/// 确保常驻 pingap 存活并确认 expected_hash 生效：
/// - 槽位在且 admin 可达 → 复用进程（配置由 `--autoreload` 拾取 active 发布）；
/// - 槽位空或 admin 不可达（进程死亡/僵死）→ owner 域重拉；
/// - hash 确认失败：**保留进程**（spec 决策点 6——旧配置继续服务），操作失败。
pub(super) async fn ensure_resident(args: &RuntimeArgs, expected_hash: &str) -> Result<()> {
    let endpoint = admin_probe::ensure_admin_endpoint().clone();
    let active = crate::proxy::compiler::active_config_path(&crate::proxy::compiler::runtime_root(
        &args.log_dir,
    ));
    let mut slot = RESIDENT.lock().await;
    let reusable = match slot.as_ref() {
        Some(_resident) => admin_reachable(&endpoint).await,
        None => false,
    };
    if !reusable {
        let Some(root) = owner_scope_root() else {
            bail!("resident proxy requires an owner supervision scope");
        };
        if let Some(resident) = slot.as_ref()
            && let Some(pid) = resident.child.id()
        {
            tracing::warn!(
                "resident pingap (pid {pid}) unresponsive; respawning under owner scope"
            );
        }
        let mut command = tokio::process::Command::new(&args.pingap_bin);
        command
            .arg("-c")
            .arg(&active)
            .arg("--autoreload")
            .env("PINGAP_ADMIN_ADDR", &endpoint.addr)
            .env("PINGAP_ADMIN_USER", &endpoint.user)
            .env("PINGAP_ADMIN_PASSWORD", &endpoint.password);
        let child = guardian::spawn_guarded(command, &root, None, true)
            .await
            .context("spawn resident pingap under owner scope")?;
        tracing::info!(
            "🛰️  resident pingap spawned under owner scope (config {})",
            active.display()
        );
        *slot = Some(ResidentProxy { child });
    }
    // 无论复用或新拉：确认本次发布的配置生效（2s 轮询 + CONFIRM_BUDGET）。
    // 逃生开关与树模式同款（本地开发/集成测试的 fake pingap 无 admin 面）。
    if std::env::var_os("APP_CLI_SKIP_PINGAP_CONFIRM").is_some() {
        tracing::warn!(
            "⏭  APP_CLI_SKIP_PINGAP_CONFIRM set; skipping resident config confirmation (dev only)"
        );
        return Ok(());
    }
    admin_probe::wait_for_config_hash(&endpoint, expected_hash, admin_probe::CONFIRM_BUDGET)
        .await
        .context("confirm resident pingap active config")?;
    Ok(())
}

/// owner 退出收束：TERM（宽限内）→ 超时 KILL 常驻进程并清空槽位。
/// 幂等（槽空 no-op——直跑形态/未编排过）。Unconfirmed 记警告不阻塞退出。
pub(crate) async fn shutdown() {
    let mut slot = RESIDENT.lock().await;
    if let Some(mut resident) = slot.take() {
        let grace = std::time::Duration::from_secs(crate::supervision::STOP_GRACE_SECONDS);
        match resident.child.stop(grace).await {
            process_utils::managed_tree::StopOutcome::Graceful(status) => {
                tracing::info!("resident pingap stopped on owner exit: {status}");
            }
            outcome => {
                tracing::warn!("resident pingap stop outcome on owner exit: {outcome:?}");
            }
        }
    }
}

/// admin 端口 TCP 可达（进程存活判定；协议层健康由 hash 确认把关）。
async fn admin_reachable(endpoint: &admin_probe::AdminEndpoint) -> bool {
    let addr: std::net::SocketAddr = match endpoint.addr.parse() {
        Ok(addr) => addr,
        Err(_) => return false,
    };
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::net::TcpStream::connect(addr),
        )
        .await,
        Ok(Ok(_))
    )
}
