//! UserApp 发布编排：git snapshot commit → build → ensure_app → prepare → activate → 轮询就绪 → confirm。
//!
//! 编排跑在 agent-runner file-server 内（与 build 同进程），子步骤调 rcoder app_manager
//! HTTP API（`/api/v1/apps/...`，rcoder 主服务）。app_id 映射 rcoder 强制 `app-` 前缀。
//!
//! 发布链路语义（调研 rcoder app_manager 结论）：
//! - `create_app` 一次性设定 image(app-runtime 基镜像)/ports/healthCheck，创建计算单元；
//! - 每次 prepare（下载校验包）→ activate（切 code 目录 + 重启容器）→ 轮询 GET → confirm（健康置 Active）。
//!
//! 环境变量（部署时配，Fail Fast：缺失即 Failed）：
//! - `RCODER_API_BASE`：rcoder app_manager API（如 `http://rcoder-service:8087`；测试环境 8086）
//! - `FILE_SERVER_BASE_URL`：本 file-server static 的 rcoder 可达 base（如 `http://{agent-svc}:60000`）
//! - `RCODER_RUNTIME_IMAGE_DIGEST`：app-runtime 基镜像（create_app 的 image，build 前已注入 env）

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::Config;
use crate::error::{AppError, AppResult};
use crate::service::build_manager::BuildManager;
use crate::service::git::write::init_and_commit;
use crate::workspace::{ProjectContext, WorkspaceResolver};

use super::tasks::{BuildProgressEvent, BuildTask, BuildTaskId, BuildTaskKind, BuildTaskStore};
use super::{WorkspaceBuildArtifact, build_workspace_package};

/// app-runtime 容器 pingap 监听端口（固定）。
const APP_HTTP_PORT: u16 = 9080;
/// app-runtime(pingap/app-cli)提供的统一 ready 端点（与业务服务无关，恒有效）。
const APP_READY_PATH: &str = "/api/rust/ready";
/// 就绪轮询间隔。
const READY_POLL_INTERVAL_SECS: u64 = 3;
/// 单次 rcoder HTTP 请求超时。
const RCODER_REQUEST_TIMEOUT_SECS: u64 = 60;

// publish 阶段名（Stage 事件）。
const STAGE_GIT_COMMIT: &str = "GitCommit";
const STAGE_BUILD: &str = "Build";
const STAGE_CREATE_APP: &str = "CreateApp";
const STAGE_PREPARE: &str = "Prepare";
const STAGE_ACTIVATE: &str = "Activate";
const STAGE_WAIT_READY: &str = "WaitReady";
const STAGE_CONFIRM: &str = "Confirm";

/// 发布编排的依赖 + 目标（聚合 start/run 所需参数，避免函数签名过长；SOLID 参数对象）。
pub struct PublishContext {
    pub resolver: Arc<dyn WorkspaceResolver>,
    pub build_manager: Arc<BuildManager>,
    pub config: Arc<Config>,
    pub app_id: String,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub timeout_secs: u64,
}

/// 异步发起 publish 任务（不阻塞，立即返 taskId）。全流程进度经 task 流出（SSE/轮询）。
///
/// 失败语义：任一阶段失败 emit `Failed`；activate 后失败的回滚由 rcoder releases 兜底。
/// cancel：步骤间检查 `is_cancelled`；build 阶段的取消见 [`build_workspace_package`]。
pub async fn start_publish_task(store: &BuildTaskStore, ctx: PublishContext) -> BuildTaskId {
    let task = store
        .create(ctx.app_id.clone(), BuildTaskKind::Publish)
        .await;
    // 预 resolve workspace 根并存入 task（logs/SSE 路径解析用）。失败 emit Failed，不 spawn。
    match ctx
        .resolver
        .resolve_project(&ProjectContext {
            project_id: ctx.app_id.clone(),
            tenant_id: ctx.tenant_id.clone(),
            space_id: ctx.space_id.clone(),
            isolation_type: None,
        })
        .await
    {
        Ok(ws) => task.set_workspace_root(ws).await,
        Err(e) => {
            task.emit(BuildProgressEvent::Failed {
                error: format!("resolve workspace: {e}"),
            })
            .await;
            return task.id.clone();
        }
    }
    let task_spawn = task.clone();
    tokio::spawn(async move {
        let result = run_publish(&task_spawn, &ctx).await;
        match result {
            Ok(release_id) => {
                task_spawn
                    .emit(BuildProgressEvent::Completed { release_id })
                    .await;
            }
            Err(e) => {
                // cancel 的 Cancelled 已由 cancel handler emit；其余 Failed 在此统一置。
                if !task_spawn.is_cancelled() && !task_spawn.is_terminal().await {
                    task_spawn
                        .emit(BuildProgressEvent::Failed {
                            error: e.to_string(),
                        })
                        .await;
                }
            }
        }
    });
    task.id.clone()
}

/// 发布全流程编排。返回 release_id 供顶层 emit Completed。
async fn run_publish(task: &BuildTask, ctx: &PublishContext) -> AppResult<String> {
    let base = rcoder_api_base()?;
    let image = env_required("RCODER_RUNTIME_IMAGE_DIGEST")?;
    let rcoder_id = rcoder_app_id(&ctx.app_id);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(RCODER_REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|e| AppError::system(format!("build http client: {e}")))?;

    let ws = ctx
        .resolver
        .resolve_project(&ProjectContext {
            project_id: ctx.app_id.clone(),
            tenant_id: ctx.tenant_id.clone(),
            space_id: ctx.space_id.clone(),
            isolation_type: None,
        })
        .await?;

    // 1. git snapshot commit（本地版本管理；gated git_enabled）。
    fail_if_cancelled(task)?;
    task.emit(BuildProgressEvent::Stage {
        stage: STAGE_GIT_COMMIT.to_string(),
    })
    .await;
    if ctx.config.git_enabled {
        let author = ctx.config.git_default_author_name.clone();
        let email = ctx.config.git_default_author_email.clone();
        let message = format!(
            "release snapshot {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        let ws_git = ws.clone();
        tokio::task::spawn_blocking(move || init_and_commit(&ws_git, &message, &author, &email))
            .await
            .map_err(|e| AppError::system(format!("git commit task join: {e}")))??;
    }

    // 2. build（复用 workspace 打包；进度 Building/BuildOk 经同一 task 流出）。
    fail_if_cancelled(task)?;
    task.emit(BuildProgressEvent::Stage {
        stage: STAGE_BUILD.to_string(),
    })
    .await;
    let artifact = build_workspace_package(
        ctx.resolver.as_ref(),
        ctx.build_manager.as_ref(),
        &ctx.app_id,
        ctx.tenant_id.as_deref(),
        ctx.space_id.as_deref(),
        ctx.timeout_secs,
        Some(task),
    )
    .await?;

    // 3. ensure_app：不存在则 create（设 image/ports/healthCheck）；存在则跳过（image 恒定）。
    fail_if_cancelled(task)?;
    task.emit(BuildProgressEvent::Stage {
        stage: STAGE_CREATE_APP.to_string(),
    })
    .await;
    ensure_app(&client, &base, &rcoder_id, &ctx.app_id, &image).await?;

    // 4. prepare：把整体包暴露成 URL 供 rcoder 下载校验。
    fail_if_cancelled(task)?;
    task.emit(BuildProgressEvent::Stage {
        stage: STAGE_PREPARE.to_string(),
    })
    .await;
    let pkg_url = package_url(&ctx.app_id, &artifact.file_name)?;
    prepare_release(&client, &base, &rcoder_id, &artifact, &pkg_url).await?;

    // 5. activate：切 code 目录 + 重启 app-runtime 容器。
    fail_if_cancelled(task)?;
    task.emit(BuildProgressEvent::Stage {
        stage: STAGE_ACTIVATE.to_string(),
    })
    .await;
    activate_release(
        &client,
        &base,
        &rcoder_id,
        &artifact.release_id,
        ctx.timeout_secs,
    )
    .await?;

    // 6. 轮询就绪：GET app 到 status=Running 且 health 非 Unhealthy。
    fail_if_cancelled(task)?;
    task.emit(BuildProgressEvent::Stage {
        stage: STAGE_WAIT_READY.to_string(),
    })
    .await;
    wait_app_ready(&client, &base, &rcoder_id, task, ctx.timeout_secs).await?;

    // 7. confirm healthy → Active（不健康 rcoder 自动回滚，此处会返错）。
    fail_if_cancelled(task)?;
    task.emit(BuildProgressEvent::Stage {
        stage: STAGE_CONFIRM.to_string(),
    })
    .await;
    confirm_release(&client, &base, &rcoder_id, &artifact.release_id).await?;

    Ok(artifact.release_id)
}

fn fail_if_cancelled(task: &BuildTask) -> AppResult<()> {
    if task.is_cancelled() {
        return Err(AppError::business("publish cancelled by user"));
    }
    Ok(())
}

/// file-server project_id → rcoder app_id（强制 `app-` 前缀；已带则原样）。
fn rcoder_app_id(project_id: &str) -> String {
    if project_id.starts_with("app-") {
        project_id.to_string()
    } else {
        format!("app-{project_id}")
    }
}

fn rcoder_api_base() -> AppResult<String> {
    env_required("RCODER_API_BASE")
}

/// 整体包下载 URL（rcoder prepare 据此拉取；base 为 file-server static 的 rcoder 可达地址）。
fn package_url(project_id: &str, file_name: &str) -> AppResult<String> {
    let base = env_required("FILE_SERVER_BASE_URL")?;
    Ok(format!(
        "{base}/api/userapp/static/{project_id}/{file_name}"
    ))
}

fn env_required(name: &str) -> AppResult<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| AppError::system(format!("env {name} not set (required for publish)")))
}

/// 发 rcoder 请求，校验统一 HttpResult（{success,data,code,message}），返回 data；裸 payload 原样。
async fn rcoder_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    path: &str,
    body: Option<Value>,
) -> AppResult<Value> {
    let url = format!("{base}{path}");
    let mut req = client.request(method, &url);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| AppError::network(format!("rcoder {url}: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| AppError::network(format!("rcoder {url} read body: {e}")))?;
    if !status.is_success() {
        return Err(AppError::network(format!(
            "rcoder {url} -> HTTP {status}: {text}"
        )));
    }
    let payload: Value = serde_json::from_str(&text)
        .map_err(|e| AppError::system(format!("rcoder {url} parse json: {e} (body: {text})")))?;
    // 统一 HttpResult：success=false → 业务错；success=true → 取 data。
    match payload.get("success").and_then(|v| v.as_bool()) {
        Some(true) => Ok(payload.get("data").cloned().unwrap_or(Value::Null)),
        Some(false) => {
            let code = payload
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN");
            let msg = payload
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("rcoder error");
            Err(AppError::network(format!("rcoder {url} {code}: {msg}")))
        }
        None => Ok(payload), // 非 HttpResult（裸 payload）
    }
}

/// GET app 判断是否存在（HTTP 404 或 HttpResult success=false 视作不存在）。
async fn app_exists(client: &reqwest::Client, base: &str, rcoder_app_id: &str) -> AppResult<bool> {
    let url = format!("{base}/api/v1/apps/{rcoder_app_id}");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| AppError::network(format!("rcoder GET {url}: {e}")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 404 {
        return Ok(false);
    }
    if !status.is_success() {
        return Err(AppError::network(format!(
            "rcoder GET {url} -> HTTP {status}: {text}"
        )));
    }
    if let Ok(v) = serde_json::from_str::<Value>(&text)
        && v.get("success").and_then(|s| s.as_bool()) == Some(false)
    {
        return Ok(false);
    }
    Ok(true)
}

/// 确保 app 计算单元存在：不存在则 create_app（image=app-runtime，端口 9080，health ready 端点）。
async fn ensure_app(
    client: &reqwest::Client,
    base: &str,
    rcoder_app_id: &str,
    name: &str,
    image: &str,
) -> AppResult<()> {
    if app_exists(client, base, rcoder_app_id).await? {
        return Ok(()); // 已存在：image/ports 首次设定后恒定，无需重建。
    }
    let body = json!({
        "appId": rcoder_app_id,
        "name": name,
        "image": image,
        "ports": [{ "name": "http", "port": APP_HTTP_PORT, "exposeType": "Http" }],
        "healthCheck": { "checkType": "Http", "path": APP_READY_PATH, "port": APP_HTTP_PORT },
    });
    rcoder_request(
        client,
        reqwest::Method::POST,
        base,
        "/api/v1/apps",
        Some(body),
    )
    .await?;
    Ok(())
}

async fn prepare_release(
    client: &reqwest::Client,
    base: &str,
    rcoder_app_id: &str,
    artifact: &WorkspaceBuildArtifact,
    pkg_url: &str,
) -> AppResult<()> {
    let body = json!({
        "releaseId": artifact.release_id,
        "url": pkg_url,
        "sha256": artifact.sha256,
        "sizeBytes": artifact.size_bytes,
    });
    let path = format!("/api/v1/apps/{rcoder_app_id}/releases/prepare");
    rcoder_request(client, reqwest::Method::POST, base, &path, Some(body)).await?;
    Ok(())
}

async fn activate_release(
    client: &reqwest::Client,
    base: &str,
    rcoder_app_id: &str,
    release_id: &str,
    readiness_timeout_secs: u64,
) -> AppResult<()> {
    let body = json!({ "readinessTimeoutSeconds": readiness_timeout_secs });
    let path = format!("/api/v1/apps/{rcoder_app_id}/releases/{release_id}/activate");
    rcoder_request(client, reqwest::Method::POST, base, &path, Some(body)).await?;
    Ok(())
}

/// 轮询 GET app 直到 status=Running 且 health 非 Unhealthy；超时或进入 Error 则失败。
async fn wait_app_ready(
    client: &reqwest::Client,
    base: &str,
    rcoder_app_id: &str,
    task: &BuildTask,
    timeout_secs: u64,
) -> AppResult<()> {
    let path = format!("/api/v1/apps/{rcoder_app_id}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if task.is_cancelled() {
            return Err(AppError::business("publish cancelled by user"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::system(format!(
                "app readiness poll timed out after {timeout_secs}s"
            )));
        }
        let info = rcoder_request(client, reqwest::Method::GET, base, &path, None).await?;
        let app_status = info
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        let health_status = info
            .get("health")
            .and_then(|h| h.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        if app_status == "Error" {
            return Err(AppError::system(format!(
                "app entered Error state (health={health_status}): {info}"
            )));
        }
        if app_status == "Running" && health_status != "Unhealthy" {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(READY_POLL_INTERVAL_SECS)).await;
    }
}

async fn confirm_release(
    client: &reqwest::Client,
    base: &str,
    rcoder_app_id: &str,
    release_id: &str,
) -> AppResult<()> {
    let body = json!({ "healthy": true, "message": "publish auto-confirm" });
    let path = format!("/api/v1/apps/{rcoder_app_id}/releases/{release_id}/confirm");
    rcoder_request(client, reqwest::Method::POST, base, &path, Some(body)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::rcoder_app_id;

    #[test]
    fn rcoder_app_id_prepends_app_prefix_when_missing() {
        assert_eq!(rcoder_app_id("userapp-e2e"), "app-userapp-e2e");
    }

    #[test]
    fn rcoder_app_id_is_idempotent_when_already_prefixed() {
        assert_eq!(rcoder_app_id("app-userapp-e2e"), "app-userapp-e2e");
    }
}
