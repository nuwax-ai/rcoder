//! 热部署（deploy_mode=hot）：调容器内 app-cli server 的 `/v1/deploy` 端点
//! 原地换应用——不换 Pod、PG/ttyd/dbx 不断连，仅应用服务切换。
//!
//! 定位是**优化路径**：一切前置不满足（app 不存在/不在跑/容器未配
//! `APP_CLI_DEPLOY_TOKEN`/明确缺少协议能力）自动回退换 Pod 权威链；
//! 受理后失败（部署失败/超时）保留现场报错（旧制品 URL 重发即回滚，对齐
//! activate 失败语义）。
//!
//! 等待边界与冷部署链（[`crate::lifecycle`] wait_deploy_stage，部署段完成即
//! 返回）不同：热部署须等匹配操作 `Running`、持久化确认和 `/ready` 业务就绪，
//! 再复核操作身份。准备期间旧服务继续运行；激活会停止旧进程，允许短暂不可用。
//!
//! 成功后把部署三元组经 `update_env_configmap` 收敛进 ConfigMap（K8s-only，
//! 不触碰 Deployment → 无 Recreate）：Pod 重建时 server 按 env 恢复最新版本，
//! 热部署的换代效果不因重建丢失。

use std::{sync::Arc, time::Duration};

use crate::config::AppAccessMode;
use container_runtime_api::UserAppRuntime;

use shared_types::AppCliDeployPhase;
use tracing::{info, warn};

use crate::error::AppOperationError;
use crate::error::AppResult;
use crate::models::{AppRuntimeInfo, StartAppRequest};
use crate::service::AppService;

/// 热部署受理/轮询端口（app-cli 管理 API 常量对齐）。
const APP_CLI_ADMIN_PORT: u16 = shared_types::APP_CLI_ADMIN_PORT;
/// 轮询间隔（对齐冷部署链部署段等待的量级）。
const POLL_INTERVAL: Duration = Duration::from_secs(3);
#[allow(dead_code)]
const HOT_DEPLOY_BUDGET: Duration = Duration::from_secs(300);

/// Dropping the JoinHandle detaches the owned coordinator; it does not cancel
/// the accepted deployment or its conditional configuration commit.
#[cfg(test)]
async fn await_hot_coordinator(
    task: tokio::task::JoinHandle<AppResult<Option<()>>>,
    budget: Duration,
) -> AppResult<Option<()>> {
    tokio::time::timeout(budget, task).await.map_err(|_| {
        AppOperationError::Backend("hot deployment wait timed out; coordinator continues; inspect operation status before retry".into())
    })?.map_err(|error| {
        AppOperationError::Backend(format!("hot deployment coordinator failed: {error}"))
    })?
}

/// 热部署成功后的 env 收敛：live env 读回 → 剥离历史三元组 → 注入本次
/// 三元组 → 条件写入 ConfigMap。任何冲突或失败均显式报告，禁止伪装完全成功。
async fn converge_deploy_env_after_hot(
    runtime: &dyn UserAppRuntime,
    access_mode: AppAccessMode,
    app_id: &str,
    snapshot: &shared_types::AppEnvSnapshot,
    env: &std::collections::HashMap<String, String>,
    _operation: &crate::service::AppOperationGuard,
) -> AppResult<()> {
    // Docker environment is immutable. The matched protocol-4 Running status
    // is accepted only after durable journal readback, checked by the observer.
    if access_mode == AppAccessMode::Docker {
        return Ok(());
    }
    runtime
        .update_env_configmap_if_version(app_id, env, snapshot)
        .await
        .map_err(|e| {
            // Activation has already changed the running release. A rejected
            // metadata CAS does not undo that effect or prove full completion;
            // retain ownership and the durable recovery record.
            AppOperationError::Backend(format!(
                "application activated but deployment env convergence failed: {e}"
            ))
        })?;
    Ok(())
}

fn validate_requested_hot_env(
    requested: &std::collections::HashMap<String, String>,
    current: &std::collections::HashMap<String, String>,
) -> AppResult<()> {
    use crate::release_flow::identity::business_env;
    if business_env(requested.clone()) != business_env(current.clone()) {
        return Err(AppOperationError::Validation(
            "hot deployment cannot change business environment; use pod mode".into(),
        ));
    }
    Ok(())
}

fn admin_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|error| {
            AppOperationError::Backend(format!("create hot deployment admin client: {error}"))
        })
}

fn supports_hot_protocol(document: &serde_json::Value) -> bool {
    document
        .pointer("/data/protocol_version")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| {
            version == u64::from(shared_types::app_cli_deploy::APP_CLI_UNIFIED_DEPLOY_PROTOCOL)
        })
}

#[cfg(test)]
async fn finish_hot_operation(
    operation: crate::service::AppOperationGuard,
    result: AppResult<Option<()>>,
) -> AppResult<Option<()>> {
    if result.is_ok() || !operation.has_unfinished_mutation() {
        operation.finish().await?;
    }
    result
}

/// Only a matched, durable and quiescent failure completes ownership. A failed
/// operation can coexist with unfinished shutdown, so its phase alone is insufficient.
fn observe_hot_operation(
    document: &serde_json::Value,
    operation_id: &str,
    release_id: &str,
    generation_id: &str,
    guard: &crate::service::AppOperationGuard,
) -> Option<shared_types::AppDeploymentOperation> {
    let operation: shared_types::AppDeploymentOperation =
        serde_json::from_value(document.pointer("/data/operation")?.clone()).ok()?;
    if !supports_hot_protocol(document)
        || operation.operation_id != operation_id
        || operation.request_release_id != release_id
        || operation.deployment_generation_id != generation_id
    {
        return None;
    }
    if operation.phase == AppCliDeployPhase::Running
        && (operation.deploy_stage != shared_types::AppDeploymentStage::Succeeded
            || !operation.persisted)
    {
        return None;
    }
    if operation.phase == AppCliDeployPhase::Failed {
        if !operation.persisted {
            return None;
        }
        if document
            .pointer("/data/protocol_version")
            .and_then(serde_json::Value::as_u64)?
            < u64::from(shared_types::app_cli_deploy::APP_CLI_QUIESCENT_DEPLOY_PROTOCOL)
        {
            return None;
        }
        let server_phase: AppCliDeployPhase =
            serde_json::from_value(document.pointer("/data/phase")?.clone()).ok()?;
        let recovery_complete = operation
            .recovery
            .as_ref()
            .is_none_or(|recovery| matches!(recovery.status.as_str(), "restored" | "failed"));
        if !matches!(
            server_phase,
            AppCliDeployPhase::Running | AppCliDeployPhase::Failed
        ) || !recovery_complete
        {
            return None;
        }
        guard.mark_completed();
    }
    Some(operation)
}

// 拆分（file-server 大文件范式）：`admit` 受理前置与 HotArtifact /
// `task` HotDeploymentTask 执行与状态观察；共享自由函数/常量留本文件，
// 测试独立 `tests`。旧路径 `hot_deploy::{HotArtifact, HotDeploymentTask}`
// 经重导出保持不变（deploy_control 以 `super::hot_deploy::HotArtifact` 引用）。

mod admit;
mod task;
#[cfg(test)]
mod tests;

pub(crate) use admit::*;
pub(crate) use task::*;
