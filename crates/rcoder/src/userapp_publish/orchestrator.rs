//! 发布/构建编排:rcoder 正向调 agent-runner build(HTTP :60000 + 订阅进度 SSE)
//! → 同进程调 app_manager(prepare/activate/create_app/confirm)。
//!
//! - `run_build`:仅触发 agent-runner build + 透传进度(独立 build 接口)。
//! - `run_publish`:全流程 build → ensure_app → prepare → activate → 轮询就绪 → confirm。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use app_manager::models::commons::{AppStatus, ExposeType, HealthCheckType};
use app_manager::models::{CreateAppRequest, HealthCheckConfig, PortConfig, PrepareReleaseRequest};
use shared_types::build_backend_addr;

use crate::router::AppState;

use super::client;
use super::task::{PublishEvent, PublishTask};

/// agent-runner 内嵌 file-server 端口(与 k8s_service.rs AGENT_FILE_SERVER_PORT 一致)。
const FILE_SERVER_PORT: u16 = 60_000;
/// app-runtime 容器公网端口(pingap 监听,对外 Service + PortConfig 用)。
const APP_HTTP_PORT: u16 = 9080;
/// app-cli 管理 API 端口(K8s 探针打这里:app-cli 自身提供 /health+/ready,不强依赖后端 app)。
const APP_CLI_ADMIN_PORT: u16 = 3010;
/// app-cli 提供的探针路径(liveness=进程活,readiness=初始化完成/可选桥接后端)。
const APP_LIVENESS_PATH: &str = "/health";
const APP_READINESS_PATH: &str = "/ready";
/// 就绪轮询间隔。
const READY_POLL_INTERVAL_SECS: u64 = 3;
/// 就绪轮询总超时(activate 后 app 启动 + 健康检查窗口)。
const APP_READY_TIMEOUT_SECS: u64 = 600;

/// build 等待结果(消费 agent-runner build SSE 终态事件得出)。
enum BuildOutcome {
    Completed { release_id: String },
    Failed(String),
    Cancelled,
}

/// 独立 build 入口(spawn 调):触发 agent-runner build + 透传进度,终态 emit。
pub async fn run_build(
    task: Arc<PublishTask>,
    state: Arc<AppState>,
    project_id: String,
    app_id: String,
) {
    let result = run_build_inner(&task, &state, &project_id, &app_id).await;
    if let Err(e) = result
        && !task.is_terminal().await
    {
        task.emit(PublishEvent::Failed {
            error: e.to_string(),
        })
        .await;
    }
}

/// 全流程发布入口(spawn 调):build → ensure_app → prepare → activate → 轮询 → confirm。
pub async fn run_publish(
    task: Arc<PublishTask>,
    state: Arc<AppState>,
    project_id: String,
    app_id: String,
) {
    let result = run_publish_inner(&task, &state, &project_id, &app_id).await;
    if let Err(e) = result
        && !task.is_terminal().await
    {
        task.emit(PublishEvent::Failed {
            error: e.to_string(),
        })
        .await;
    }
}

async fn run_build_inner(
    task: &PublishTask,
    state: &AppState,
    project_id: &str,
    app_id: &str,
) -> Result<()> {
    let addr = resolve_agent_addr(state, project_id)?;
    task.emit(PublishEvent::Stage {
        stage: "Build".to_string(),
    })
    .await;
    let build_task_id = client::trigger_build(&addr, app_id).await?;
    match wait_build(&addr, &build_task_id, task).await? {
        BuildOutcome::Completed { release_id } => {
            task.emit(PublishEvent::Completed { release_id }).await;
        }
        BuildOutcome::Failed(err) => {
            task.emit(PublishEvent::Failed { error: err }).await;
        }
        BuildOutcome::Cancelled => {
            task.emit(PublishEvent::Cancelled).await;
        }
    }
    Ok(())
}

async fn run_publish_inner(
    task: &PublishTask,
    state: &AppState,
    project_id: &str,
    app_id: &str,
) -> Result<()> {
    let addr = resolve_agent_addr(state, project_id)?;

    // 1. build(透传 agent-runner 进度;拿 release_id)。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "Build".to_string(),
    })
    .await;
    let build_task_id = client::trigger_build(&addr, app_id).await?;
    let release_id = match wait_build(&addr, &build_task_id, task).await? {
        BuildOutcome::Completed { release_id } => release_id,
        BuildOutcome::Failed(err) => {
            task.emit(PublishEvent::Failed { error: err }).await;
            return Ok(());
        }
        BuildOutcome::Cancelled => {
            task.emit(PublishEvent::Cancelled).await;
            return Ok(());
        }
    };
    // build 产物摘要(sha/size/file_name)从 agent-runner task 快照取(file-server build 完成写入)。
    let snap = client::get_build_snapshot(&addr, &build_task_id).await?;
    let sha256 = snap
        .get("sha256")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("build snapshot missing sha256"))?
        .to_string();
    let size_bytes = snap
        .get("sizeBytes")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("build snapshot missing sizeBytes"))?;
    let file_name = snap
        .get("fileName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("build snapshot missing fileName"))?
        .to_string();

    let rcoder_app_id = rcoder_app_id(app_id);
    let image = std::env::var("RCODER_RUNTIME_IMAGE_DIGEST")
        .context("RCODER_RUNTIME_IMAGE_DIGEST env not set (app-runtime image for create_app)")?;

    // 2. prepare(包 url 指向 agent-runner file-server,app_manager 据此下载校验)。
    //    prepare 自带 ensure_app_workspace_ready + ensure_release_dirs,无需先 create_app。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "Prepare".to_string(),
    })
    .await;
    let url = client::package_url(&addr, app_id, &file_name);
    state
        .app_service
        .prepare_release(
            &rcoder_app_id,
            PrepareReleaseRequest {
                release_id: release_id.clone(),
                url,
                sha256,
                size_bytes,
                retention: None,
            },
        )
        .await
        .map_err(|e| anyhow!("prepare_release: {e}"))?;

    // 3. activate(切 code 目录 + 重启 app-runtime 容器;新 app 无容器则只解压 code)。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "Activate".to_string(),
    })
    .await;
    state
        .app_service
        .activate_release(&rcoder_app_id, &release_id)
        .await
        .map_err(|e| anyhow!("activate_release: {e}"))?;

    // 4. ensure_app:create app 运行时容器(image=app-runtime,端口 9080,health ready 端点)。
    //    顺序硬约束:必须在 activate 之后 —— create_app_runtime 读 code/release.lock.toml
    //    注入 build identity,而 release.lock.toml 是 activate 从 release 包解压到 code/ 的。
    //    幂等:已存在的 app(重发)get_app 命中 → no-op;activate 的 stop/swap/restart 照常。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "EnsureApp".to_string(),
    })
    .await;
    ensure_app(state, &rcoder_app_id, app_id, &image).await?;

    // 5. 轮询就绪:status=Running 且 health 非 Unhealthy。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "WaitReady".to_string(),
    })
    .await;
    wait_app_ready(state, &rcoder_app_id, task).await?;

    // 6. confirm healthy → Active(不健康 rcoder 自动回滚,此处返错)。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "Confirm".to_string(),
    })
    .await;
    state
        .app_service
        .confirm_release(
            &rcoder_app_id,
            &release_id,
            true,
            Some("publish auto-confirm".to_string()),
        )
        .await
        .map_err(|e| anyhow!("confirm_release: {e}"))?;

    task.emit(PublishEvent::Completed {
        release_id: release_id.clone(),
    })
    .await;
    Ok(())
}

/// 解析 agent-runner project_id → file-server addr(`http://{host}:60000`)。
/// 复用 `build_backend_addr`(K8s 自动走 `{container_name}-svc.{ns}.svc.{domain}`,Docker 走 container_ip)。
fn resolve_agent_addr(state: &AppState, project_id: &str) -> Result<String> {
    let info = state
        .projects
        .get(project_id)
        .and_then(|p| p.container_info())
        .ok_or_else(|| anyhow!("agent-runner not found for project_id={project_id}"))?;
    let host = build_backend_addr(
        &info.container_name,
        &info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    );
    Ok(format!("http://{host}:{FILE_SERVER_PORT}"))
}

/// 消费 agent-runner build SSE:透传进度到 task,终态返 BuildOutcome。
/// 期间检查 task.is_cancelled → cancel_build + Cancelled。
async fn wait_build(addr: &str, build_task_id: &str, task: &PublishTask) -> Result<BuildOutcome> {
    let mut rx = client::subscribe_build_progress(addr, build_task_id);
    while let Some(data) = rx.recv().await {
        if task.is_cancelled() {
            let _ = client::cancel_build(addr, build_task_id).await;
            return Ok(BuildOutcome::Cancelled);
        }
        let event = data
            .get("event")
            .and_then(|e| e.as_str())
            .unwrap_or_default()
            .to_string();
        // 终态字段先取出(emit 会 move data)。
        let release_id = if event == "completed" {
            // agent-runner BuildProgressEvent enum 级 serde rename_all 只作用于 variant 名,
            // struct-variant field 未 rename(仍 snake_case),故 release_id 用 snake 取。
            // (BuildTaskSnapshot 是 struct,camelCase 正常;sha/size/file 走 snapshot 不受影响。)
            data.get("release_id")
                .and_then(|r| r.as_str())
                .unwrap_or_default()
                .to_string()
        } else {
            String::new()
        };
        let error = if event == "failed" {
            data.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("build failed")
                .to_string()
        } else {
            String::new()
        };
        // 透传(含终态事件,前端可见 build 完整进度)。
        task.emit(PublishEvent::BuildProgress { data }).await;
        match event.as_str() {
            "completed" => return Ok(BuildOutcome::Completed { release_id }),
            "failed" => return Ok(BuildOutcome::Failed(error)),
            "cancelled" => return Ok(BuildOutcome::Cancelled),
            _ => {}
        }
    }
    Err(anyhow!(
        "agent-runner build stream ended without terminal event"
    ))
}

/// 确保 app 计算单元存在:不存在则 create_app(幂等;image/ports 首次设定后恒定)。
async fn ensure_app(state: &AppState, rcoder_app_id: &str, name: &str, image: &str) -> Result<()> {
    match state.app_service.get_app(rcoder_app_id).await {
        Ok(_) => return Ok(()),          // 已存在
        Err(e) if is_not_found(&e) => {} // 不存在 → create
        Err(e) => return Err(anyhow!("get_app: {e}")),
    }
    let req = CreateAppRequest {
        app_id: Some(rcoder_app_id.to_string()),
        name: name.to_string(),
        image: image.to_string(),
        command: None,
        env: None,
        secrets: None,
        resources: None,
        ports: Some(vec![PortConfig {
            name: "http".to_string(),
            port: APP_HTTP_PORT,
            expose_type: ExposeType::Http,
            strip_prefix: None,
        }]),
        // 探针打 app-cli 的 3010 管理 API(非 pingap 9080):app-cli 自身提供 /health(liveness,
        // 进程活,后端有 bug 也不杀容器)+ /ready(readiness,默认 app-cli 就绪/可选桥接后端)。
        // 不再硬编码 /api/rust/ready(旧 bug:与实际后端语言无关,且强依赖后端实现该路径)。
        health_check: Some(HealthCheckConfig {
            check_type: HealthCheckType::Http,
            path: Some(APP_READINESS_PATH.to_string()),
            liveness_path: Some(APP_LIVENESS_PATH.to_string()),
            port: Some(APP_CLI_ADMIN_PORT),
        }),
        tenant_id: None,
        space_id: None,
    };
    state
        .app_service
        .create_app(req)
        .await
        .map_err(|e| anyhow!("create_app: {e}"))?;
    Ok(())
}

/// 轮询 app 到 status=Running 且 health 非 Unhealthy;超时或进入 Error 则失败。
async fn wait_app_ready(state: &AppState, rcoder_app_id: &str, task: &PublishTask) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(APP_READY_TIMEOUT_SECS);
    loop {
        if task.is_cancelled() {
            return Err(anyhow!("publish cancelled by user"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "app readiness poll timed out after {APP_READY_TIMEOUT_SECS}s"
            ));
        }
        let info = state
            .app_service
            .get_app(rcoder_app_id)
            .await
            .map_err(|e| anyhow!("get_app poll: {e}"))?;
        if info.status == AppStatus::Error {
            return Err(anyhow!(
                "app entered Error state (health={})",
                info.health.status
            ));
        }
        if info.status == AppStatus::Running && info.health.status != "Unhealthy" {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(READY_POLL_INTERVAL_SECS)).await;
    }
}

fn fail_if_cancelled(task: &PublishTask) -> Result<()> {
    if task.is_cancelled() {
        return Err(anyhow!("publish cancelled by user"));
    }
    Ok(())
}

/// app_manager 错误是否 "app 不存在"(get_app 判存性用)。
fn is_not_found(e: &app_manager::error::AppOperationError) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("does not exist") || msg.contains("not found")
}

/// file-server project_id → rcoder app_id(强制 `app-` 前缀,已带则原样)。
fn rcoder_app_id(app_id: &str) -> String {
    if app_id.starts_with("app-") {
        app_id.to_string()
    } else {
        format!("app-{app_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::rcoder_app_id;

    #[test]
    fn rcoder_app_id_prepends_prefix_when_missing() {
        assert_eq!(rcoder_app_id("userapp-e2e"), "app-userapp-e2e");
    }

    #[test]
    fn rcoder_app_id_is_idempotent_when_prefixed() {
        assert_eq!(rcoder_app_id("app-userapp-e2e"), "app-userapp-e2e");
    }
}
