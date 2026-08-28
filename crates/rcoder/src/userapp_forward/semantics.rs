//! 容器懒启动语义分派：容器不在时按路径短路（停止/取消语义 200 成功信封）、
//! 报错（查询语义 CONTAINER_NOT_FOUND 信封）、ensure（使用语义）三态；连同
//! query 入参提取校验（tasks app_id / static user_id）与透传层自身的
//! HttpResult 错误信封（保留真实状态码——区别于恒 200 的业务信封）。

use axum::response::{IntoResponse, Response};

use shared_types::error_codes;

use crate::router::AppState;
use crate::userapp_builder::registered_builder;

use super::upstream::USERAPP_API_PREFIX;

// ── 容器懒启动语义分派（容器不在时按路径短路/报错/ensure）────────────────────

/// dev 容器不在时该路径应采取的行为（纯函数，按 path 判定）。
pub(super) enum DevAbsentAction {
    /// 使用语义（默认）：容器不在 → ensure 创建后转发
    Ensure,
    /// 停止/取消语义：容器不在 = 目标态已达成（服务停着/任务不可能还在跑），
    /// 短路 200 成功——不 ensure（起容器为了停它是反语义）
    SkipSuccess(SkipKind),
    /// 查询语义：容器不在 → 直接报错。任务/进程态在容器内存，重建容器也
    /// 救不回；且空列表/任务 404 会与「业务上确实没有」混淆——报
    /// CONTAINER_NOT_FOUND 让调用方按 code 区分真实原因
    Unavailable,
}

pub(super) enum SkipKind {
    /// tasks/{task_id}/cancel：终态幂等成功（同容器侧 already_terminal 形状）
    CancelTask(String),
    /// dev/stop：无进程即停（同容器侧 "No running process found" 形状）
    DevStop,
}

pub(super) fn classify_dev_absent(path: &str) -> DevAbsentAction {
    let rest = path.strip_prefix(USERAPP_API_PREFIX).unwrap_or(path);
    if let Some(tail) = rest.strip_prefix("/tasks/") {
        let segs: Vec<&str> = tail.split('/').collect();
        return match segs.as_slice() {
            [_task_id] => DevAbsentAction::Unavailable,
            [task_id, "cancel"] => {
                DevAbsentAction::SkipSuccess(SkipKind::CancelTask(task_id.to_string()))
            }
            [_task_id, "logs"] | [_task_id, "logs", "stream"] => DevAbsentAction::Unavailable,
            // 未知子路径兜底 ensure（容器自答 404，不在此拦截）
            _ => DevAbsentAction::Ensure,
        };
    }
    match rest {
        "/dev/stop" => DevAbsentAction::SkipSuccess(SkipKind::DevStop),
        "/dev/list" => DevAbsentAction::Unavailable,
        _ => DevAbsentAction::Ensure,
    }
}

/// 短路语义的容器在否判定（peek）：只读探测，不 ensure、不写探活缓存、
/// 不触发自愈——短路路径必须零副作用。
/// 短路语义的容器在否判定（peek）：**仅以注册表 miss 为 absent 信号**，
/// 不做探活——3s 探测在容器内重量构建（多服务 build/依赖安装）下会假阴性，
/// 把进行中的 tasks 进度轮询误杀（轮询恰是该接口的核心场景）。注册命中即
/// 视为在：慢/死 IP 由转发路径兜底（send 失败 502 + 下请求探活自愈）。
/// 零副作用承诺不变：只读注册表，不 ensure 不自愈不清缓存。
pub(super) fn dev_container_absent(state: &AppState, app_id: &str) -> bool {
    registered_builder(state, app_id).is_none()
}

/// 全站 HttpResult 信封短路响应（HTTP 恒 200，调用方按信封 code 判断：
/// "0000"=成功、非 0000=失败）。
fn envelope_response(code: &str, message: &str, data: serde_json::Value) -> Response {
    let payload = serde_json::json!({
        "code": code,
        "message": message,
        "data": data,
        "tid": null,
        "success": code == error_codes::SUCCESS,
    });
    (axum::http::StatusCode::OK, axum::Json(payload)).into_response()
}

/// 查询类短路：容器不在，报 CONTAINER_NOT_FOUND（message 只陈述事实）。
pub(super) fn unavailable_response(app_id: &str) -> Response {
    envelope_response(
        error_codes::ERR_CONTAINER_NOT_FOUND,
        &format!("userApp dev container not running: app_id={app_id}"),
        serde_json::Value::Null,
    )
}

/// cancel 短路成功（容器侧终态幂等同款形状）。
pub(super) fn cancel_skip_response(task_id: &str) -> Response {
    envelope_response(
        error_codes::SUCCESS,
        "success",
        serde_json::json!({
            "task_id": task_id,
            "status": null,
            "already_terminal": true,
        }),
    )
}

/// dev/stop 短路成功（容器侧无进程即停的同款形状）。
pub(super) fn dev_stop_skip_response(app_id: &str) -> Response {
    envelope_response(
        error_codes::SUCCESS,
        "success",
        serde_json::json!({
            "message": "No running process found",
            "app_id": app_id,
            "pid": null,
            "killed_pids": [],
        }),
    )
}

/// 从 raw query string 提取单值参数（值均为白名单字符集，无需 percent-decode）。
fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// tasks 族定位（签名自描述）：query `app_id` 必填——该族不消费 X-App-Id
/// header（接口签名上不可见的隐式依赖，本批显式化）。
pub(super) fn require_query_app_id(query: Option<&str>) -> Result<String, HttpResultError> {
    let raw = query_param(query, "app_id")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Err(HttpResultError::bad_request(
            "missing required query parameter `app_id` for tasks endpoints",
        ));
    };
    shared_types::validate_identifier(raw, "app_id")
        .map(|_| raw.to_string())
        .map_err(HttpResultError::bad_request)
}

/// static/{app_id} 的 query `user_id` 必填（🟢 ensure 显式档：懒创建容器
/// 宿主树分区直取，不依赖 metadata 注册）。非 static 路径返回 None（不要求）。
/// static/{app_id} 的 query `user_id` 必填（🟢 ensure 显式档：懒创建容器
/// 宿主树分区直取，不依赖 metadata 注册）。调用方已按 static 前缀分派。
pub(super) fn require_static_user_id(query: Option<&str>) -> Result<String, HttpResultError> {
    let raw = query_param(query, "user_id")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Err(HttpResultError::bad_request(
            "missing required query parameter `user_id` for static artifact download",
        ));
    };
    shared_types::validate_identifier(raw, "user_id")
        .map(|_| raw.to_string())
        .map_err(HttpResultError::bad_request)
}

// ── HttpResult 错误响应（透传层自身错误；上游业务响应原样透传不重包装） ──────────

/// 轻量错误值（Result 大 Err 侧禁用 Response 本体；测试 unwrap 需 Debug）。
#[derive(Debug)]
pub(super) struct HttpResultError {
    status: axum::http::StatusCode,
    message: String,
    /// 503 唤醒类错误的 Retry-After 秒数（对齐 proxy_http 流量唤醒面）。
    retry_after_secs: Option<u32>,
}

impl HttpResultError {
    pub(super) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::BAD_REQUEST,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    pub(super) fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::BAD_GATEWAY,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::NOT_FOUND,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    pub(super) fn service_unavailable(message: impl Into<String>, retry_after_secs: u32) -> Self {
        Self {
            status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
            retry_after_secs: Some(retry_after_secs),
        }
    }

    pub(super) fn system(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// 装箱响应（`Result` 的 Err 侧按指针传播——`Response` >128B，
    /// resolve_dev_addr 等深链函数避免按值 move）。
    pub(super) fn into_boxed_response(self) -> Box<Response> {
        Box::new(self.into_response())
    }
}

impl IntoResponse for HttpResultError {
    fn into_response(self) -> Response {
        // 与 shared_types::HttpResult 同形态(code=字符串错误码/message/data/tid/success),
        // 但保留真实 HTTP 状态码(400/404/502/503 对代理与客户端有语义; HttpResult 的
        // IntoResponse 恒 200, 不适用于透传层的传输级错误)
        let payload = serde_json::json!({
            "code": error_code_for(self.status),
            "message": self.message,
            "data": serde_json::Value::Null,
            "success": false,
        });
        let mut response = (self.status, axum::Json(payload)).into_response();
        if let Some(secs) = self.retry_after_secs
            && let Ok(value) = axum::http::HeaderValue::from_str(&secs.to_string())
        {
            response.headers_mut().insert("retry-after", value);
        }
        response
    }
}

/// HTTP 状态码 → 全站字符串错误码(对齐 shared_types::error_codes 词表)。
pub(super) fn error_code_for(status: axum::http::StatusCode) -> &'static str {
    match status {
        axum::http::StatusCode::BAD_REQUEST => error_codes::ERR_VALIDATION,
        axum::http::StatusCode::NOT_FOUND => error_codes::ERR_CONTAINER_NOT_FOUND,
        axum::http::StatusCode::SERVICE_UNAVAILABLE | axum::http::StatusCode::BAD_GATEWAY => {
            error_codes::ERR_BACKEND_ERROR
        }
        _ => error_codes::ERR_INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_kind(path: &str) -> &'static str {
        match classify_dev_absent(path) {
            DevAbsentAction::Ensure => "ensure",
            DevAbsentAction::SkipSuccess(SkipKind::CancelTask(_)) => "cancel-ok",
            DevAbsentAction::SkipSuccess(SkipKind::DevStop) => "dev-stop-ok",
            DevAbsentAction::Unavailable => "unavailable",
        }
    }

    /// 路径语义分派全模式：tasks 族四形态 + dev server 短路两条 + 默认 ensure。
    #[test]
    fn classify_dev_absent_covers_all_semantics() {
        assert_eq!(classify_kind("/api/v1/userapp/tasks/t-1"), "unavailable");
        assert_eq!(
            classify_kind("/api/v1/userapp/tasks/t-1/logs"),
            "unavailable"
        );
        assert_eq!(
            classify_kind("/api/v1/userapp/tasks/t-1/logs/stream"),
            "unavailable"
        );
        assert_eq!(
            classify_kind("/api/v1/userapp/tasks/t-1/cancel"),
            "cancel-ok"
        );
        assert_eq!(classify_kind("/api/v1/userapp/dev/stop"), "dev-stop-ok");
        assert_eq!(classify_kind("/api/v1/userapp/dev/list"), "unavailable");
        // 使用语义默认 ensure（起容器）
        for path in [
            "/api/v1/userapp/build",
            "/api/v1/userapp/dev/start",
            "/api/v1/userapp/dev/restart",
            "/api/v1/userapp/dev/logs",
            "/api/v1/userapp/ensure-workspace",
            "/api/v1/userapp/static/app-1",
            "/api/v1/userapp/get-file-list",
        ] {
            assert_eq!(classify_kind(path), "ensure", "{path}");
        }
        // 未知 tasks 子路径兜底 ensure（容器自答 404，不在此拦截）
        assert_eq!(classify_kind("/api/v1/userapp/tasks/t-1/unknown"), "ensure");
    }

    #[test]
    fn query_param_extracts_single_value() {
        let q = Some("app_id=app-1&from_seq=3&service=web");
        assert_eq!(query_param(q, "app_id"), Some("app-1"));
        assert_eq!(query_param(q, "from_seq"), Some("3"));
        assert_eq!(query_param(q, "missing"), None);
        assert_eq!(query_param(None, "app_id"), None);
        // 前缀相似键不误匹配
        assert_eq!(query_param(Some("app_idx=1"), "app_id"), None);
        // 无值键
        assert_eq!(query_param(Some("app_id"), "app_id"), None);
    }

    #[test]
    fn require_query_app_id_validates() {
        assert_eq!(require_query_app_id(Some("app_id=app-1")).unwrap(), "app-1");
        // 缺失 / 空串 / 非法字符（含路径穿越）→ Err（400 响应）
        assert!(require_query_app_id(None).is_err());
        assert!(require_query_app_id(Some("user_id=u1")).is_err());
        assert!(require_query_app_id(Some("app_id=")).is_err());
        assert!(require_query_app_id(Some("app_id=../evil")).is_err());
    }

    #[test]
    fn static_user_id_required_and_validated() {
        // 必填（缺失 / 只有其他参数 → 400）
        assert!(require_static_user_id(None).is_err());
        assert!(require_static_user_id(Some("release_id=r1")).is_err());
        assert_eq!(
            require_static_user_id(Some("release_id=r1&user_id=u1")).unwrap(),
            "u1"
        );
        // 白名单校验（含 / 即拒）
        assert!(require_static_user_id(Some("user_id=../evil")).is_err());
    }

    /// 短路信封形状：cancel 幂等终态（与容器侧 CancelData 同构）。
    #[tokio::test]
    async fn cancel_skip_response_matches_container_shape() {
        let resp = cancel_skip_response("t-1");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], error_codes::SUCCESS);
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["task_id"], "t-1");
        assert_eq!(v["data"]["status"], serde_json::Value::Null);
        assert_eq!(v["data"]["already_terminal"], true);
    }

    /// 报错信封：HTTP 200 + CONTAINER_NOT_FOUND + message 只陈述事实。
    #[tokio::test]
    async fn unavailable_response_is_enveloped_container_not_found() {
        let resp = unavailable_response("app-1");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], error_codes::ERR_CONTAINER_NOT_FOUND);
        assert_eq!(v["success"], false);
        assert_eq!(v["data"], serde_json::Value::Null);
        assert_eq!(
            v["message"],
            "userApp dev container not running: app_id=app-1"
        );
    }

    /// dev/stop 短路信封（与容器侧 UserappDevStopped 同构）。
    #[tokio::test]
    async fn dev_stop_skip_response_matches_container_shape() {
        let resp = dev_stop_skip_response("app-1");
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], error_codes::SUCCESS);
        assert_eq!(v["data"]["message"], "No running process found");
        assert_eq!(v["data"]["app_id"], "app-1");
        assert_eq!(v["data"]["pid"], serde_json::Value::Null);
        assert_eq!(v["data"]["killed_pids"], serde_json::json!([]));
    }

    /// 透传清单 ↔ 语义分类闭包：tasks 族必须命中短路/报错类（query app_id
    /// 定位的前提），dev/stop、dev/list 必须命中各自类——路径改形/新增 tasks
    /// 子族忘同步 classify 当场报红。
    #[test]
    fn pass_through_paths_have_expected_absent_semantics() {
        use crate::userapp_forward::CONTAINER_PASS_THROUGH_PATHS;
        for pattern in CONTAINER_PASS_THROUGH_PATHS {
            // 模式串占位符替换为样例值后分类
            let sample = pattern
                .replace("{task_id}", "t-1")
                .replace("{app_id}", "app-1");
            let kind = classify_kind(&sample);
            let expected = match *pattern {
                "/api/v1/userapp/tasks/{task_id}" => "unavailable",
                "/api/v1/userapp/tasks/{task_id}/logs" => "unavailable",
                "/api/v1/userapp/tasks/{task_id}/logs/stream" => "unavailable",
                "/api/v1/userapp/tasks/{task_id}/cancel" => "cancel-ok",
                "/api/v1/userapp/dev/stop" => "dev-stop-ok",
                "/api/v1/userapp/dev/list" => "unavailable",
                _ => "ensure",
            };
            assert_eq!(kind, expected, "{pattern} 语义分类漂移");
        }
    }
}
