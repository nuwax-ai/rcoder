//! userApp 文件域透传层入口：rcoder 主服务（8086）→ 目标容器内 file-server（60000）。
//!
//! **dev/prod 两阶段分派**（`X-App-Stage` header，缺省 dev——同一 app_id 可同时
//! 存在开发容器与生产 Deployment，必须显式区分）：
//! - `dev`：UserAppBuilder 开发容器（注册表定位 + 探活自愈，miss 幂等 ensure）
//! - `prod`：UserApp 生产运行容器（存在性检查 + 唤醒 + 确定性命名定位）
//!
//! 三类入口：
//! - [`forward_userapp`]：`/api/v1/userapp/*` 显式透传（容器懒启动语义分派见
//!   [`super::semantics`]）
//! - [`computer_intercept`]：`/api/computer/*` 拦截层（`X-Service-Type: userapp`
//!   分流时 TS 老路径原样转发，body 零解析——multipart 在代理层不可解）
//! - 门面折叠：剥 `{app_id}/{app_stage}` 段还原容器平铺契约（构建链 dev-only）
//!
//! 定位与转发内核在 [`super::upstream`]。

use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::info;

use shared_types::HttpResult;
use shared_types::UserappStage;
pub use shared_types::{APP_STAGE_DEV, APP_STAGE_HEADER, APP_STAGE_PROD, SERVICE_TYPE_HEADER};

use crate::router::AppState;

use super::semantics::{
    DevAbsentAction, HttpResultError, SkipKind, cancel_skip_response, classify_dev_absent,
    dev_container_absent, dev_stop_skip_response, require_query_app_id, require_query_user_id,
    require_static_user_id, unavailable_response,
};
use super::upstream::{
    STATIC_PATH_PREFIX, TASKS_PATH_PREFIX, forward_to_dev, forward_to_prod,
    missing_app_id_response, require_app_id,
};

/// dev/prod 阶段分派解析：缺省 dev（向后兼容既有无 header 调用）；
/// 未知值 fail-fast 400（header 拼错不该静默落错容器）。
fn parse_app_stage(req: &Request) -> Result<UserappStage, Box<Response>> {
    let Some(value) = req
        .headers()
        .get(APP_STAGE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(UserappStage::Dev);
    };
    match value.to_ascii_lowercase().as_str() {
        v if v == APP_STAGE_DEV => Ok(UserappStage::Dev),
        v if v == APP_STAGE_PROD => Ok(UserappStage::Prod),
        other => Err(Box::new(
            HttpResultError::bad_request(format!(
                "invalid `{APP_STAGE_HEADER}` value '{other}'; expected `{APP_STAGE_DEV}` or `{APP_STAGE_PROD}`"
            ))
            .into_response(),
        )),
    }
}

/// `/api/v1/userapp/{*rest}` 通配透传 handler。
///
/// 容器懒启动语义分派：tasks 族 query app_id 自描述定位（不消费 X-App-Id），
/// 容器不在时按 [`classify_dev_absent`] 短路（cancel/dev-stop 成功、查询类
/// CONTAINER_NOT_FOUND）或 ensure 创建；static 族 query user_id 必填（显式档）。
pub(crate) async fn forward_userapp(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);

    // tasks 族：构建链 dev-only（忽略 X-App-Stage——构建任务只存在于 dev
    // builder），query app_id+user_id 定位（user_id 同时是懒创建显式 owner 档）；
    // 容器不在时短路
    if path.starts_with(TASKS_PATH_PREFIX) {
        let app_id = match require_query_app_id(query.as_deref()) {
            Ok(app_id) => app_id,
            Err(e) => return e.into_response(),
        };
        let user_id = match require_query_user_id(query.as_deref()) {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        };
        if dev_container_absent(&state, &app_id) {
            return match classify_dev_absent(&path) {
                DevAbsentAction::SkipSuccess(SkipKind::CancelTask(task_id)) => {
                    info!(
                        "[USERAPP_FORWARD] dev container absent, cancel short-circuit ok: app_id={app_id}, task_id={task_id}"
                    );
                    cancel_skip_response(&task_id)
                }
                _ => {
                    info!(
                        "[USERAPP_FORWARD] dev container absent, task query rejected: app_id={app_id}"
                    );
                    unavailable_response(&app_id)
                }
            };
        }
        info!(
            "[USERAPP_FORWARD] {} {} -> dev container (app_id={app_id}, user_id={user_id}, query-located)",
            req.method(),
            req.uri().path()
        );
        return forward_to_dev(&state, &app_id, req, Some(&user_id)).await;
    }

    // static/{app_id}：构建链 dev-only（制品 zip 在 dev workspace，prod 容器必
    // 404——忽略 X-App-Stage，与 tasks 族同语义），query user_id 必填（🟢 ensure
    // 显式档）。app_id 取 path 段（签名自描述）：制品下载方是 app-cli（容器内
    // 部署下载，URL 由 start{url} 编排注入）——机器调用不设 X-App-Id header，
    // 必填 header 契约会把部署下载拒成 400（模板全链验证实测），path 段已
    // 承载定位。
    if path.starts_with(STATIC_PATH_PREFIX) {
        let user_id = match require_static_user_id(query.as_deref()) {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        };
        let raw_app_id = path
            .strip_prefix(STATIC_PATH_PREFIX)
            .and_then(|rest| rest.split('/').next())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(app_id) = raw_app_id.map(str::to_owned) else {
            return HttpResultError::bad_request(
                "missing path segment `app_id` for static artifact download",
            )
            .into_response();
        };
        if let Err(e) = shared_types::validate_identifier(&app_id, "app_id") {
            return HttpResultError::bad_request(e).into_response();
        }
        info!(
            "[USERAPP_FORWARD] {} {} -> dev container (static, app_id={app_id})",
            req.method(),
            req.uri().path()
        );
        return forward_to_dev(&state, &app_id, req, Some(&user_id)).await;
    }

    let Some(app_id) = require_app_id(&req) else {
        return missing_app_id_response();
    };
    let stage = match parse_app_stage(&req) {
        Ok(stage) => stage,
        Err(resp) => return *resp,
    };
    match stage {
        UserappStage::Dev => {
            // 停止/查询短路：仅容器不在时生效（容器在则照常转发）
            let action = classify_dev_absent(&path);
            let short_circuit =
                !matches!(action, DevAbsentAction::Ensure) && dev_container_absent(&state, &app_id);
            if short_circuit {
                return match action {
                    DevAbsentAction::SkipSuccess(SkipKind::DevStop) => {
                        info!(
                            "[USERAPP_FORWARD] dev container absent, dev/stop short-circuit ok: app_id={app_id}"
                        );
                        dev_stop_skip_response(&app_id)
                    }
                    // cancel 已在 tasks 分支按 query app_id 处理；此处兜底不可达
                    DevAbsentAction::SkipSuccess(SkipKind::CancelTask(task_id)) => {
                        cancel_skip_response(&task_id)
                    }
                    DevAbsentAction::Unavailable => {
                        info!(
                            "[USERAPP_FORWARD] dev container absent, dev/list rejected: app_id={app_id}"
                        );
                        unavailable_response(&app_id)
                    }
                    DevAbsentAction::Ensure => unreachable!("Ensure 已被 short_circuit 条件排除"),
                };
            }
            info!(
                "[USERAPP_FORWARD] {} {} -> dev container (app_id={app_id})",
                req.method(),
                req.uri().path()
            );
            // 透传族 body 内 user_id 流式不解析——显式档仅 static（已前移）传值
            forward_to_dev(&state, &app_id, req, None).await
        }
        UserappStage::Prod => {
            info!(
                "[USERAPP_FORWARD] {} {} -> prod runtime container (app_id={app_id})",
                req.method(),
                req.uri().path()
            );
            forward_to_prod(&state, &app_id, req).await
        }
    }
}
/// `/api/v1/userapp/{app_id}/{app_stage}` 门面（dev-only 构建链公用内核）：
///
/// 两侧契约**同构直转**——容器侧（file-server-userapp）已同形态注册
/// `/{app_id}/{app_stage}/...`，app_id 由路径段承载、body 不含（容器侧
/// body 结构已瘦身），转发零改写（URI/body 原样流式）。
///
/// 1. `{app_stage}` 仅认 `dev`——构建链是开发阶段能力，传 prod 返回 400 明示；
/// 2. 容器定位以 path `app_id` 为准（幂等 ensure builder+探活自愈），无
///    `X-App-Id` header 要求——path 即身份。
async fn fold_env_forward(
    state: Arc<AppState>,
    path: axum::extract::Path<(String, String)>,
    req: Request,
    target: &'static str,
) -> Response {
    use shared_types::UserappStage;
    let (app_id, app_stage) = path.0;
    let Some(app_stage) = UserappStage::parse(&app_stage) else {
        return HttpResultError::bad_request("path segment `app_stage` must be `dev` or `prod`")
            .into_response();
    };
    if app_stage != UserappStage::Dev {
        return HttpResultError::bad_request(format!(
            "`{target}` is a dev (build-chain) capability: pass app_stage=dev"
        ))
        .into_response();
    }
    if let Err(e) = shared_types::validate_identifier(&app_id, "app_id") {
        return HttpResultError::bad_request(e).into_response();
    }

    info!(
        "[USERAPP_FORWARD] {} {} -> dev container (folded app_stage, app_id={app_id})",
        req.method(),
        req.uri().path()
    );
    // 门面 body 携 user_id 但流式不解析——owner 走 metadata 链（create-workspace
    // 前置注册）
    forward_to_dev(&state, &app_id, req, None).await
}

/// 探测开发容器内的项目类型
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/projects/detect",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `dev`（构建链为开发阶段能力）")
    ),
    request_body(
        content = file_server_userapp::models::ProjectChainBody,
        description = "仅需 `user_id` 与 `project_dir`——`app_id` 由 path 提供并自动注入转发 body（调用方不传）"
    ),
    description = r#"
分析开发容器 workspace 的文件结构，推断项目类型（Node/Python/Java…）与推荐配置，
作为 confirm 的输入。**仅 dev**——构建链是开发阶段能力，传 prod 返回 400。

定位沿用透传面契约：header `X-App-Id` 指定目标开发容器（须与 path 一致）；
URI 折叠为容器内平铺路径 `/api/v1/userapp/projects/detect` 后流式转发。
"#,
    responses(
        (status = 200, description = "探测结果（HttpResult 信封，data 含类型推断与文件清单）", body = HttpResult<serde_json::Value>),
        (status = 400, description = "app_stage 非 dev / 缺或错 X-App-Id / 参数非法", body = HttpResult<String>)
    ),
    tag = "UserApp · dev · 工作区与工具链",
)]
pub(crate) async fn flat_dev_projects_detect(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    path: axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    fold_env_forward(state, path, req, "/api/v1/userapp/projects/detect").await
}

/// 确认开发容器的项目类型
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/projects/confirm",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `dev`（构建链为开发阶段能力）")
    ),
    request_body(
        content = file_server_userapp::models::ProjectChainBody,
        description = "detect 结果的用户修正确认 + 项目基础信息。仅需 `user_id` 与 `project_dir`——`app_id` 由 path 提供并自动注入转发 body（调用方不传）"
    ),
    description = r#"
用户在 detect 推断基础上选择/修正项目类型后提交确认（幂等附带 git init 双开关）。
**仅 dev**；定位与折叠语义同 [`flat_dev_projects_detect`]。
"#,
    responses(
        (status = 200, description = "确认结果（HttpResult 信封）", body = HttpResult<serde_json::Value>),
        (status = 400, description = "app_stage 非 dev / 缺或错 X-App-Id / 参数非法", body = HttpResult<String>)
    ),
    tag = "UserApp · dev · 工作区与工具链",
)]
pub(crate) async fn flat_dev_projects_confirm(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    path: axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    fold_env_forward(state, path, req, "/api/v1/userapp/projects/confirm").await
}

/// 安装项目到开发容器
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/install-project",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `dev`（构建链为开发阶段能力）")
    ),
    request_body(
        content = file_server_userapp::models::UserappInstallBody,
        description = "仅需 `user_id` 与 `programming_language`——`app_id` 由 path 提供并自动注入转发 body（调用方不传）"
    ),
    description = r#"
将项目安装进开发容器工作区（依赖安装等初始化动作的统一入口）。**仅 dev**；
定位与折叠语义同 [`flat_dev_projects_detect`]。
"#,
    responses(
        (status = 200, description = "安装结果（HttpResult 信封）", body = HttpResult<serde_json::Value>),
        (status = 400, description = "app_stage 非 dev / 缺或错 X-App-Id / 参数非法", body = HttpResult<String>)
    ),
    tag = "UserApp · dev · 工作区与工具链",
)]
pub(crate) async fn flat_dev_install_project(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    path: axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    fold_env_forward(state, path, req, "/api/v1/userapp/install-project").await
}
/// `/api/computer/*` 拦截层：header `X-Service-Type: userapp` 即短路转发该 app
/// 目标容器**同路径**（TS 路径原样、body 零解析，header 随请求透传供容器内
/// computer handler 消费做 workspace 切换）；无该 header 落本地移植 handler。
/// `X-App-Stage` 同样生效（缺省 dev，与 /api/v1/userapp/* 分派一致）。
pub(crate) async fn computer_intercept(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let is_userapp = req
        .headers()
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(shared_types::is_userapp_service_type_value);
    if !is_userapp {
        return next.run(req).await;
    }
    let Some(app_id) = require_app_id(&req) else {
        return missing_app_id_response();
    };
    let stage = match parse_app_stage(&req) {
        Ok(stage) => stage,
        Err(resp) => return *resp,
    };
    match stage {
        UserappStage::Dev => {
            info!(
                "[USERAPP_FORWARD] intercepted computer request {} -> dev container (app_id={app_id})",
                req.uri().path()
            );
            // TS 老族 body 携 user_id（camelCase 契约）但流式不解析——metadata 链
            forward_to_dev(&state, &app_id, req, None).await
        }
        UserappStage::Prod => {
            info!(
                "[USERAPP_FORWARD] intercepted computer request {} -> prod runtime container (app_id={app_id})",
                req.uri().path()
            );
            forward_to_prod(&state, &app_id, req).await
        }
    }
}
