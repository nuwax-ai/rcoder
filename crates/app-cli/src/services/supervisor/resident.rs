//! P1 常驻 pingap（builtin 引擎）：serve 进程生命周期持有，业务编排代次
//! 重建不再杀/拉入口进程。
//!
//! 归属（V2-03/RV-03/C9）：进程注册到 **owner 会话作用域**——经
//! [`guardian::spawn_guarded_owner`]（declared_root 显式 None：owner 进程
//! 自身即授权方，不携带业务 generation 声明，规避"command work root
//! differs from the supervised generation"的跨域拒绝）。业务代次退出/
//! 清理不收束它；owner 退出由 [`shutdown`] 显式收束（startup 关机汇合点）。
//!
//! 直跑形态（`app-cli run`，无 serve 回执）不走本模块——单次编排的进程
//! 域维持会话树托管（树外进程会在 run() 返回后孤儿化）。

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

/// runtime_root 记录槽（[`ensure_resident`] 写、[`publish_standby_if_serving`]
/// 读）。std::sync::Mutex 只做单值替换，中毒时覆盖写即恢复（与
/// command_authority::set_session_work_root 同款理由）。
static STATE_RUNTIME_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn record_runtime_root(root: Option<PathBuf>) {
    let mut slot = STATE_RUNTIME_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = root;
}

fn recorded_runtime_root() -> Option<PathBuf> {
    STATE_RUNTIME_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// owner 会话作用域根（serve 形态恒有；直跑形态 None → 调用方走会话树）。
///
/// C9：不取 WORKER_ENV 祖先——那是旧 worker 链的**业务代次**范围
/// （command_authority::resolve_root 的 env 分支），冒充 owner 域会让
/// 常驻进程登记到业务 generation 的清理范围。仅认显式 session scope。
pub(super) fn owner_scope_root() -> Option<PathBuf> {
    runtime_supervisor::current_scope().map(|scope| scope.work_root)
}

/// 确保常驻 pingap 存活并确认 expected_hash 生效：
/// - 槽位在且 admin 认证探测成功 → 复用进程（配置由 `--autoreload` 拾取
///   active 发布；认证探测 = 带凭证的 admin 哈希读取，非裸 TCP——C10）；
/// - 槽位空或探测失败（进程死亡/僵死）→ owner 域重拉；
/// - hash 确认失败：**保留进程**（spec 决策点 6——旧配置继续服务），操作
///   失败；已确认则登记 expected_hash（C8：与树模式/宿主引擎同款，
///   业务就绪观察据此核对）。
pub(super) async fn ensure_resident(args: &RuntimeArgs, expected_hash: &str) -> Result<()> {
    let endpoint = admin_probe::ensure_admin_endpoint().clone();
    let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir);
    let active = crate::proxy::compiler::active_config_path(&runtime_root);
    let needs_spawn = {
        let slot = RESIDENT.lock().await;
        match slot.as_ref() {
            None => true,
            Some(_) => !admin_auth_reachable(&endpoint).await,
        }
    };
    if needs_spawn {
        let Some(root) = owner_scope_root() else {
            bail!("resident proxy requires an owner supervision scope");
        };
        {
            let slot = RESIDENT.lock().await;
            if let Some(resident) = slot.as_ref()
                && let Some(pid) = resident.child.id()
            {
                tracing::warn!(
                    "resident pingap (pid {pid}) unresponsive; respawning under owner scope"
                );
            }
        }
        let mut command = tokio::process::Command::new(&args.pingap_bin);
        command
            .arg("-c")
            .arg(&active)
            .arg("--autoreload")
            .env("PINGAP_ADMIN_ADDR", &endpoint.addr)
            .env("PINGAP_ADMIN_USER", &endpoint.user)
            .env("PINGAP_ADMIN_PASSWORD", &endpoint.password);
        // C12：capture=false（输出继承 serve 的 stdout/stderr）——piped 而
        // 不 drain 会在日志洪峰下反压卡死入口进程。
        let child = guardian::spawn_guarded_owner(command, &root, false)
            .await
            .context("spawn resident pingap under owner scope")?;
        tracing::info!(
            "🛰️  resident pingap spawned under owner scope (config {})",
            active.display()
        );
        record_runtime_root(
            active
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
        );
        // C12：确认（最长 25s 网络等待）在锁外进行——持全局槽锁跨确认会
        // 拖住 is_serving/shutdown/管理面。
        {
            let mut slot = RESIDENT.lock().await;
            *slot = Some(ResidentProxy { child });
        }
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
    // C8：确认成功才登记（业务就绪观察的 serving hash 口径）。
    crate::proxy::compiler::record_expected_hash(expected_hash);
    Ok(())
}

/// 会话收束前的 standby 摘流（builtin 引擎）：常驻在服务时发布 standby
/// 并确认热载。supervisord 引擎的等价逻辑在 `stop_all`；两处语义一致——
/// 发布/确认失败由调用方决定（会话收束记警告不阻塞；Stop 语义整体失败）。
pub(crate) async fn publish_standby_if_serving() -> Result<()> {
    let endpoint = admin_probe::ensure_admin_endpoint().clone();
    if !is_serving().await {
        return Ok(()); // 槽空（直跑形态/未编排）或入口已死——无流量可摘
    }
    let Some(runtime_root) = recorded_runtime_root() else {
        return Ok(()); // 未记录（ensure_resident 未跑过）——与槽空同义
    };
    let publication = uuid::Uuid::new_v4().simple().to_string();
    let hash = crate::proxy::compiler::publish_standby(&runtime_root, &publication).await?;
    admin_probe::wait_for_config_hash(&endpoint, &hash, admin_probe::CONFIRM_BUDGET)
        .await
        .context("confirm standby hot-reload")?;
    tracing::info!("🛑 standby confirmed for builtin session shutdown (publication {publication})");
    Ok(())
}

/// 常驻是否在服务（槽位在 + admin **认证**可达）——编排侧重载兼容校验的
/// 门条件。认证探测 = 带凭证读 admin 哈希（C10：裸 TCP 成功不证明是
/// 我们的 pingap，失败也不足以判死；认证成功是精确的进程+身份证据）。
pub(crate) async fn is_serving() -> bool {
    let serving = {
        let slot = RESIDENT.lock().await;
        slot.is_some()
    };
    serving && admin_auth_reachable(admin_probe::ensure_admin_endpoint()).await
}

/// admin 认证探测：带凭证读取当前 config hash（1s 预算）——成功 = 进程
/// 存活且凭证匹配（区别于任意监听者的裸 TCP 可达）。
async fn admin_auth_reachable(endpoint: &admin_probe::AdminEndpoint) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            admin_probe::fetch_config_hash(&endpoint.addr, &endpoint.user, &endpoint.password),
        )
        .await,
        Ok(Ok(_))
    )
}

/// owner 退出收束：TERM（宽限内）→ 超时 KILL 常驻进程并清空槽位。
/// 幂等（槽空 no-op——直跑形态/未编排过）。Unconfirmed 记警告不阻塞退出。
pub(crate) async fn shutdown() {
    let mut resident = RESIDENT.lock().await.take();
    if let Some(resident) = resident.as_mut() {
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
