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
        if let Some(parent) = active.parent().and_then(Path::parent) {
            *STATE_RUNTIME_ROOT.lock().unwrap() = Some(parent.to_path_buf());
        }
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

/// 常驻是否在服务（槽位在 + admin 可达）——编排侧重载兼容校验的门条件。
pub(crate) async fn is_serving() -> bool {
    let serving = {
        let slot = RESIDENT.lock().await;
        slot.is_some()
    };
    serving && admin_reachable(admin_probe::ensure_admin_endpoint()).await
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

/// 会话收束前的 standby 摘流（builtin 引擎）：常驻在服务时发布 standby
/// 并确认热载。supervisord 引擎的等价逻辑在 `stop_all`；两处语义一致——
/// 发布/确认失败由调用方决定（会话收束记警告不阻塞；Stop 语义整体失败）。
pub(crate) async fn publish_standby_if_serving() -> Result<()> {
    let endpoint = admin_probe::ensure_admin_endpoint().clone();
    {
        let slot = RESIDENT.lock().await;
        if slot.is_none() {
            return Ok(()); // 槽空（直跑形态/未编排）——无入口可摘流
        }
        if !admin_reachable(&endpoint).await {
            return Ok(()); // 入口已死——无服务可摘
        }
    }
    let publication = uuid::Uuid::new_v4().simple().to_string();
    // runtime_root 无法从这里获得（槽内未存）——经全局 log 目录推导：
    // 与 ensure_resident 的调用方使用同一 runtime_root 解析（env 覆盖一致）。
    let runtime_root = current_runtime_root()?;
    let hash = crate::proxy::compiler::publish_standby(&runtime_root, &publication).await?;
    admin_probe::wait_for_config_hash(&endpoint, &hash, admin_probe::CONFIRM_BUDGET)
        .await
        .context("confirm standby hot-reload")?;
    tracing::info!("🛑 standby confirmed for builtin session shutdown (publication {publication})");
    Ok(())
}

fn current_runtime_root() -> Result<PathBuf> {
    // 与编排调用方同源的 runtime_root 解析；serve 进程内 log 目录经全局槽
    // 不可得——由 ensure_resident 在拉起时记录，供此处复用。
    match STATE_RUNTIME_ROOT.lock().unwrap().clone() {
        Some(root) => Ok(root),
        None => Err(anyhow::anyhow!(
            "resident runtime root unrecorded (ensure_resident never ran)"
        )),
    }
}

static STATE_RUNTIME_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

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
