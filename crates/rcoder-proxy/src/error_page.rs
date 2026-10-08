//! UserApp 代理失败的统一友好表示：内置自包含页 + 外部页快照 + 表示协商 +
//! 协议正确的错误写出。
//!
//! 边界（proxy-error-page.md §模块边界）：本 crate 只做呈现——模板快照经
//! [`ErrorPageSource`] 由 rcoder-engine 的存储/加载器注入（IO 不进代理热路径）；
//! 只覆盖 UserApp app 代理路由，不改变状态码（真实 502/503/504 保留）、
//! 不重放请求、不把错误转成成功。
//!
//! 协议规则（§3/§4）：
//! - GET 页面/iframe 导航 → HTML 页（Fetch Metadata 优先；缺失时按显式
//!   `Accept: text/html` 且 q≠0 判断；`*/*` 不代表页面）；
//! - 明确的 fetch/资源请求（Sec-Fetch-Dest: script/image/empty 等）→ 结构化
//!   JSON（不被 Accept 覆盖，不把 HTML 当脚本或业务 JSON）；
//! - HEAD → 对应状态和响应头，无正文；POST 等写请求 → 结构化错误，不重发；
//! - 已开始的响应（SSE/WebSocket/流）不注入——本模块只在响应未开始的
//!   失败出口调用；
//! - `Cache-Control: no-store` + 准确 Content-Type/Content-Length；诊断编号
//!   一次生成，写响应头、页面变量与同一条结构化日志。

use std::sync::Arc;

use opentelemetry::propagation::Extractor;
use pingora::http::ResponseHeader;
use pingora_proxy::Session;
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// 诊断编号响应头（与页面变量、错误日志同一编号）。
pub const DIAGNOSTIC_HEADER: &str = "x-rcoder-diagnostic-id";

/// 内置默认页（自包含样式与交互；支持窄 iframe；不依赖应用资源）。
const BUILTIN_PAGE: &str = include_str!("../assets/userapp-error-default.html");

/// 有限文本变量（HTML 转义后替换；不做模板表达式执行）。
pub const VAR_TITLE: &str = "{{RCODER_TITLE}}";
pub const VAR_MESSAGE: &str = "{{RCODER_MESSAGE}}";
pub const VAR_DIAGNOSTIC_ID: &str = "{{RCODER_DIAGNOSTIC_ID}}";
pub const VAR_STATUS: &str = "{{RCODER_STATUS}}";
/// `<html lang>` 用语言标记（如 zh-CN）；自定义页未包含该占位符则不受影响。
pub const VAR_LANG: &str = "{{RCODER_LANG}}";
/// 结构化失败档位 slug（如 starting/stopped/platform_unavailable）——内置
/// 页脚本按它驱动 UI 状态（R6：不再解析翻译标题文案）；自定义页可选。
pub const VAR_CAUSE: &str = "{{RCODER_CAUSE}}";
/// 内置页固定标签的本地化占位符（键名与 userapp_error_page.ui.* 对应；
/// 渲染时按 locale 注入，自定义页不强制包含）。
pub const VAR_UI_BROWSER: &str = "{{RCODER_UI_BROWSER}}";
pub const VAR_UI_CONNECTED: &str = "{{RCODER_UI_CONNECTED}}";
pub const VAR_UI_GATEWAY: &str = "{{RCODER_UI_GATEWAY}}";
pub const VAR_UI_NORMAL: &str = "{{RCODER_UI_NORMAL}}";
pub const VAR_UI_APP: &str = "{{RCODER_UI_APP}}";
pub const VAR_UI_UNAVAILABLE: &str = "{{RCODER_UI_UNAVAILABLE}}";
pub const VAR_UI_STARTING: &str = "{{RCODER_UI_STARTING}}";
pub const VAR_UI_STOPPED: &str = "{{RCODER_UI_STOPPED}}";
pub const VAR_UI_FAILED: &str = "{{RCODER_UI_FAILED}}";
pub const VAR_UI_RELOAD: &str = "{{RCODER_UI_RELOAD}}";
pub const VAR_UI_HINT: &str = "{{RCODER_UI_HINT}}";
pub const VAR_UI_COPY: &str = "{{RCODER_UI_COPY}}";
pub const VAR_UI_COPIED: &str = "{{RCODER_UI_COPIED}}";
pub const VAR_UI_ARIA: &str = "{{RCODER_UI_ARIA}}";

/// UI 标签占位符 → i18n key（userapp_error_page.ui.<key>；渲染期按 locale
/// 注入——固定文案跟随页面语言，脚本状态文案不再依赖标题子串）。
const UI_LABELS: [(&str, &str); 14] = [
    (VAR_UI_BROWSER, "browser"),
    (VAR_UI_CONNECTED, "connected"),
    (VAR_UI_GATEWAY, "gateway"),
    (VAR_UI_NORMAL, "normal"),
    (VAR_UI_APP, "app_service"),
    (VAR_UI_UNAVAILABLE, "unavailable"),
    (VAR_UI_STARTING, "starting"),
    (VAR_UI_STOPPED, "stopped"),
    (VAR_UI_FAILED, "failed"),
    (VAR_UI_RELOAD, "reload"),
    (VAR_UI_HINT, "hint"),
    (VAR_UI_COPY, "copy"),
    (VAR_UI_COPIED, "copied"),
    (VAR_UI_ARIA, "aria_chain"),
];

/// 外部页快照（存储/加载器发布；内容不可变，热路径零 IO）。
#[derive(Debug, Clone)]
pub struct ErrorPageSnapshot {
    /// 完整模板文本（UTF-8；已通过上传校验）
    pub template: Arc<str>,
    /// 内容摘要（sha256 hex；管理状态与多副本对账用）
    pub sha256: String,
}

/// 页面来源（rcoder-engine 实现：file/ConfigMap 后端 + 单刷新执行者）。
pub trait ErrorPageSource: Send + Sync {
    /// 当前生效的外部页；None = 内置页兜底（未配置/读取失败已降级）。
    fn current(&self) -> Option<Arc<ErrorPageSnapshot>>;

    /// 错误页被消费时的按需刷新触发（fire-and-forget；默认无操作——
    /// 纯内存源无需刷新）。
    fn request_refresh(&self) {}
}

/// 呈现器：模板选择 + 变量渲染。Pingora 与 Axum 管理面共享同一实例。
pub struct ErrorPageRenderer {
    source: Option<Arc<dyn ErrorPageSource>>,
}

impl ErrorPageRenderer {
    pub fn new(source: Option<Arc<dyn ErrorPageSource>>) -> Self {
        Self { source }
    }

    /// 消费方在错误出口调用：触发源的按需刷新（不阻塞渲染）。
    pub fn request_refresh(&self) {
        if let Some(source) = self.source.as_ref() {
            source.request_refresh();
        }
    }

    /// 当前模板（外部页优先；未配置/快照缺失用内置页）。
    fn template(&self) -> Arc<str> {
        self.source
            .as_ref()
            .and_then(|source| source.current())
            .map(|snapshot| Arc::clone(&snapshot.template))
            .unwrap_or_else(|| Arc::from(BUILTIN_PAGE))
    }

    /// 渲染（变量全部 HTML 转义；未知占位符不解析——上传侧已校验拒绝）。
    pub fn render(&self, vars: &ErrorPageVars, locale: &str) -> Vec<u8> {
        let template = self.template();
        let mut rendered = template
            .replace(VAR_TITLE, &html_escape(&vars.title))
            .replace(VAR_MESSAGE, &html_escape(&vars.message))
            .replace(VAR_DIAGNOSTIC_ID, &html_escape(&vars.diagnostic_id))
            .replace(VAR_STATUS, &html_escape(&vars.status))
            .replace(VAR_LANG, &html_escape(locale))
            .replace(VAR_CAUSE, &html_escape(&vars.cause_slug));
        for (placeholder, key) in UI_LABELS {
            let text = shared_types::t(&format!("userapp_error_page.ui.{key}"), locale);
            rendered = rendered.replace(placeholder, &html_escape(&text));
        }
        rendered.into_bytes()
    }
}

/// 错误页变量（值来自平台预设文案、状态码与本次诊断编号；不掺入原始
/// error/URL/Cookie）。
#[derive(Debug, Clone)]
pub struct ErrorPageVars {
    pub title: String,
    pub message: String,
    pub diagnostic_id: String,
    pub status: String,
    /// 失败档位 slug（[`crate::error_page::ErrorPageCause::i18n_slug`]）；
    /// 空串 = 未分类（generic 兜底）。
    pub cause_slug: String,
}

/// 失败原因分类（决定文案；确定性上下文才用确定档，证据不足一律通用文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPageCause {
    /// 有当前实例的启动证据
    Starting,
    /// 已确认停止
    Stopped,
    /// 有明确启动失败证据
    Failed,
    /// 计算资源不存在（未部署/已删除/被回收）——不可访问，重试无意义
    Missing,
    /// 生命周期操作进行中/被围栏（删除中、停止中）
    Blocked,
    /// 恢复门禁（ERR_RECOVERY_REQUIRED）——需显式恢复或重新部署
    RecoveryRequired,
    /// 平台侧组件不可用——非应用自身问题
    PlatformUnavailable,
    /// 操作结果未知（不虚报成败，也不凭连接失败断言）
    OutcomeUnknown,
    /// 连接失败/状态未知：不凭连接拒绝宣称"正在启动"
    Generic,
}

/// 档位 slug（对外：[`ErrorPageVars::cause_slug`] 注入用）。
pub fn cause_slug(cause: ErrorPageCause) -> &'static str {
    cause.i18n_slug()
}

impl ErrorPageCause {
    /// i18n key 段（userapp_error_page.<slug>.{title,message}）。
    fn i18n_slug(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Missing => "missing",
            Self::Blocked => "blocked",
            Self::RecoveryRequired => "recovery_required",
            Self::PlatformUnavailable => "platform_unavailable",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Generic => "generic",
        }
    }

    /// 平台预设文案（按 locale 经 shared_types_i18n 取；公共页面不含内部
    /// 地址/凭据/操作细节）。
    pub fn copywriting(self, locale: &str) -> (String, String) {
        let slug = self.i18n_slug();
        (
            shared_types::t(&format!("userapp_error_page.{slug}.title"), locale),
            shared_types::t(&format!("userapp_error_page.{slug}.message"), locale),
        )
    }
}

/// 只有原操作的已确认启动写入进入观察阶段，才可宣称正在启动。
/// wake_wait 包裹整个 ensure_running，wake_follower_wait 的 leader 也可能
/// 仍在预检；这两个外围等待阶段及单独的操作 ID 都不证明启动已经受理。
fn timeout_indicates_starting(failure: &shared_types::WakeFailure) -> bool {
    failure.stage.as_ref() == "wake_observation"
        && failure
            .operation_id
            .as_ref()
            .is_some_and(|operation_id| !operation_id.trim().is_empty())
}

/// 唤醒失败 → 失败页文案档位。分组与 `status_from_code` 的状态码
/// 分组对齐（文案档位与真实状态码一致）；错误码来自持久准入链，是终局
/// 证据——映射出的档位不再被就绪观察改写。超时类按结构化 stage 分类
/// （见 [`timeout_indicates_starting`]），不解析消息文本。
pub fn wake_failure_cause(failure: &shared_types::WakeFailure) -> ErrorPageCause {
    use shared_types::{
        ERR_APP_NOT_FOUND, ERR_CONFLICT, ERR_CONTAINER_ADDRESS_NOT_READY, ERR_CONTAINER_NOT_FOUND,
        ERR_CONTAINER_START_FAILED, ERR_DATABASE_NOT_READY, ERR_IMAGE_PULL_FAILED,
        ERR_OPERATION_IN_PROGRESS, ERR_OPERATION_OUTCOME_UNKNOWN, ERR_PROXY_SERVICE_UNAVAILABLE,
        ERR_RECOVERY_REQUIRED, ERR_RESOURCE_EXHAUSTED, ERR_RUNTIME_TIMEOUT,
        ERR_RUNTIME_UNAVAILABLE, ERR_SERVICE_UNAVAILABLE, ERR_USERAPP_WAIT_TIMEOUT,
    };
    match failure.code.as_ref() {
        ERR_APP_NOT_FOUND | ERR_CONTAINER_NOT_FOUND => ErrorPageCause::Missing,
        ERR_CONFLICT | ERR_OPERATION_IN_PROGRESS => ErrorPageCause::Blocked,
        ERR_RECOVERY_REQUIRED => ErrorPageCause::RecoveryRequired,
        ERR_IMAGE_PULL_FAILED | ERR_CONTAINER_START_FAILED => ErrorPageCause::Failed,
        ERR_RUNTIME_TIMEOUT if timeout_indicates_starting(failure) => {
            // 请求确实在等一次已捕获的启动执行（可重试，504 + Retry-After），
            // 不凭连接失败猜测
            ErrorPageCause::Starting
        }
        ERR_RUNTIME_TIMEOUT => {
            // 无启动证据的超时（前置查询/身份/配置读取）：平台侧观察失败，
            // 非应用正在启动
            ErrorPageCause::PlatformUnavailable
        }
        ERR_USERAPP_WAIT_TIMEOUT => ErrorPageCause::Starting,
        ERR_RUNTIME_UNAVAILABLE
        | ERR_SERVICE_UNAVAILABLE
        | ERR_DATABASE_NOT_READY
        | ERR_RESOURCE_EXHAUSTED
        | ERR_CONTAINER_ADDRESS_NOT_READY
        | ERR_PROXY_SERVICE_UNAVAILABLE => ErrorPageCause::PlatformUnavailable,
        ERR_OPERATION_OUTCOME_UNKNOWN => ErrorPageCause::OutcomeUnknown,
        _ => ErrorPageCause::Generic,
    }
}

/// UserApp 代理失败诊断提示（失败路径只读观察的浓缩形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAppProxyFailureHint {
    /// app-cli 顶层业务状态（用于失败页文案：starting/stopped/failed）
    pub readiness_status: Option<shared_types::UserAppReadinessStatus>,
    /// 当前实例实际生效的 Pingap 配置已确认具备错误来源剥离规则
    /// （`X-Pingap-EType` 只可能来自代理自产错误）
    pub error_origin_confirmed: bool,
}

impl UserAppProxyFailureHint {
    /// 失败页文案分类：有证据才用确定文案，缺证据一律通用（不凭连接拒绝
    /// 宣称"正在启动"）。全枚举显式匹配：ReadinessStatus 新增/改名时编译期
    /// 即暴露此映射。
    pub fn page_cause(&self) -> ErrorPageCause {
        use shared_types::UserAppReadinessStatus as Status;
        match self.readiness_status {
            Some(Status::Starting) => ErrorPageCause::Starting,
            Some(Status::Stopped) => ErrorPageCause::Stopped,
            Some(Status::Failed) => ErrorPageCause::Failed,
            Some(Status::NotDeployed) => ErrorPageCause::Missing,
            // Ready/Degraded 出现在失败出口说明健康证据与本次失败并存，
            // Stopping/Unknown/Unsupported 无确定文案证据——一律通用
            None
            | Some(Status::Stopping)
            | Some(Status::Ready)
            | Some(Status::Degraded)
            | Some(Status::Unknown)
            | Some(Status::Unsupported) => ErrorPageCause::Generic,
        }
    }
}

/// 失败路径只读诊断顾问（rcoder-engine 实现：短 TTL 缓存 + 就绪 reader）。
///
/// 只在失败出口的剩余预算内调用；无观察/超预算返回 None（调用方用通用
/// 提示或保留上游响应）。不是控制操作身份、不触发任何写路径。
#[async_trait::async_trait]
pub trait UserAppProxyFailureAdvisor: Send + Sync {
    async fn advise(
        &self,
        app_id: &str,
        stage: &str,
        budget: std::time::Duration,
    ) -> Option<UserAppProxyFailureHint>;
}

/// 生成一次性诊断编号（uuid v4 简短形态；请求级，不与业务身份复用）。
pub fn new_diagnostic_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// HTML 转义（变量只进普通文本位置；转义后替换）。
fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// 错误表示（响应呈现选择，非认证规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorRepresentation {
    /// HTML 页面（浏览器导航/iframe）
    Document,
    /// 结构化 JSON（fetch/API/资源/写请求/HEAD 伴随的机器形态）
    Machine,
}

/// 表示协商：Fetch Metadata 优先，缺失时按显式 Accept 推断。
///
/// 规则（§3）：已明确是 fetch/资源请求时不被 Accept 覆盖；`Accept: */*`
/// 本身不代表页面；解析 Accept 尊重 q=0。
pub fn negotiate(session: &Session) -> ErrorRepresentation {
    let headers = &session.req_header().headers;
    let method = session.req_header().method.clone();
    if method != pingora::http::Method::GET && method != pingora::http::Method::HEAD {
        return ErrorRepresentation::Machine;
    }
    if let Some(dest) = headers
        .get("sec-fetch-dest")
        .and_then(|value| value.to_str().ok())
        .map(str::trim_ascii)
    {
        // 显式资源/fetch 目标 → 机器形态（不被 Accept 覆盖）
        if matches!(
            dest,
            "script"
                | "style"
                | "image"
                | "font"
                | "audio"
                | "video"
                | "track"
                | "object"
                | "embed"
                | "manifest"
                | "empty"
                | "worker"
                | "serviceworker"
                | "sharedworker"
        ) {
            return ErrorRepresentation::Machine;
        }
        if dest == "document" || dest == "iframe" {
            return ErrorRepresentation::Document;
        }
    }
    if let Some(mode) = headers
        .get("sec-fetch-mode")
        .and_then(|value| value.to_str().ok())
        .map(str::trim_ascii)
    {
        if mode == "navigate" {
            return ErrorRepresentation::Document;
        }
        // 显式 cors/same-origin fetch 语义 → 机器形态
        if mode != "no-cors" {
            return ErrorRepresentation::Machine;
        }
    }
    // Fetch Metadata 缺失：显式接受 text/html 且 q≠0 才按页面呈现
    headers
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .and_then(|accept| accepts_text_html(accept).then_some(ErrorRepresentation::Document))
        .unwrap_or(ErrorRepresentation::Machine)
}

/// Accept 头解析：存在 q≠0 的 `text/html` 项。
fn accepts_text_html(accept: &str) -> bool {
    accept.split(',').any(|item| {
        let mut parts = item.split(';');
        let Some(media_type) = parts.next() else {
            return false;
        };
        if media_type.trim_ascii() != "text/html" {
            return false;
        }
        let mut quality = 1.0f32;
        for parameter in parts {
            let parameter = parameter.trim_ascii();
            if let Some(value) = parameter.strip_prefix("q=") {
                quality = value.trim_ascii().parse().unwrap_or(1.0);
            }
        }
        quality > 0.0
    })
}

/// pingora 请求头的 W3C 传播提取器（只取 traceparent/tracestate）。
struct PingoraHeaderExtractor<'a>(&'a pingora_http::HMap);

impl Extractor for PingoraHeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        vec!["traceparent", "tracestate"]
    }
}

/// 失败出口 span：有入站 traceparent（Java/Gateway 传播）则挂为其子 span
/// ——精确跨服务串联；无则独立根 span（仍导出 Tempo 且日志携带 trace_id）。
/// 只在失败路径创建，成功请求零开销。
fn failure_span(session: &Session, status: u16, context: &str) -> tracing::Span {
    let span = tracing::error_span!(
        "userapp_proxy_failure",
        otel.name = "userapp.proxy.failure",
        otel.kind = "internal",
        http.status_code = status,
        rcoder.context = context,
    );
    let extracted = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&PingoraHeaderExtractor(&session.req_header().headers))
    });
    let _ = span.set_parent(extracted);
    span
}

fn attach_wake_diagnostic(
    payload: &mut serde_json::Value,
    failure: &shared_types::WakeFailure,
    locale: &str,
) {
    payload["error"]["operation_id"] = serde_json::json!(failure.operation_id);
    payload["error"]["blocker"] = serde_json::json!(failure.blocker);
    payload["error"]["error_detail"] = serde_json::json!(
        shared_types::ErrorDetail::new(
            failure.cause_code.as_ref(),
            failure.stage.as_ref(),
            failure.message.clone(),
        )
        .with_retryable(failure.retryable)
        .localized(locale)
    );
}

/// 写出 UserApp 代理失败响应（在下游最终响应尚未开始时调用一次）。
///
/// - 保留真实状态码；503 携带 `Retry-After` 建议（非恢复保证）；
/// - HEAD：对应状态和响应头，无正文；
/// - 写失败只记录——绝不二次发送第二份响应（连接由 Pingora 收尾）；
/// - 渲染从内存快照读取，不做任何 IO，不延长已耗尽的代理 deadline。
#[allow(clippy::too_many_arguments)]
pub async fn write_error_response(
    session: &mut Session,
    renderer: &ErrorPageRenderer,
    status: u16,
    cause: ErrorPageCause,
    reason: &str,
    retry_after_secs: Option<u64>,
    context: &str,
    detail: &str,
    wake_failure: Option<&shared_types::WakeFailure>,
) -> () {
    // 响应已开始（上游中途断流等）：绝不再写第二份响应——正文追加会污染
    // 截断的原始流。守卫语义与 pingora-core write_error_response（server.rs
    // 630-637）逐条对齐：已写的是**终态**响应才跳过；1xx 临时响应放行
    // （101 除外——按 RFC 9110 §15.2.2 视为终态），否则 100-continue 的
    // POST 中途失败会只剩一个 100 然后挂住。
    let already_final = session
        .as_downstream_mut()
        .response_written()
        .is_some_and(|written| {
            !written.status.is_informational() || written.status.as_u16() == 101
        });
    if already_final {
        tracing::debug!(
            %status,
            "downstream response already started; skip friendly error write"
        );
        return;
    }
    let span = failure_span(session, status, context);
    write_error_response_inner(
        session,
        renderer,
        status,
        cause,
        reason,
        retry_after_secs,
        context,
        detail,
        wake_failure,
        &span,
    )
    .instrument(span.clone())
    .await;
}

/// 注意：本函数不自带 instrument——外层 [`failure_span`] 经 `.instrument()`
/// 提供上下文（span 对象经参数传入用于记录 diagnostic_id 等属性）。
#[allow(clippy::too_many_arguments)]
async fn write_error_response_inner(
    session: &mut Session,
    renderer: &ErrorPageRenderer,
    status: u16,
    cause: ErrorPageCause,
    reason: &str,
    retry_after_secs: Option<u64>,
    context: &str,
    detail: &str,
    wake_failure: Option<&shared_types::WakeFailure>,
    span: &tracing::Span,
) {
    // 错误页被消费 → 触发按需刷新（K8s 投射/手工换页的最终收敛路径之一）
    renderer.request_refresh();
    let diagnostic_id = new_diagnostic_id();
    // 语言协商：与机器 JSON 路径同一函数（Accept-Language；缺失回落默认语言）
    let locale = shared_types::parse_accept_language(
        session
            .req_header()
            .headers
            .get("accept-language")
            .and_then(|value| value.to_str().ok()),
    );
    let (title, message) = cause.copywriting(locale);
    let representation = negotiate(session);
    let is_head = session.req_header().method == pingora::http::Method::HEAD;

    let (content_type, body): (&'static str, Vec<u8>) = match representation {
        ErrorRepresentation::Document => {
            let vars = ErrorPageVars {
                title,
                message,
                diagnostic_id: diagnostic_id.clone(),
                status: status.to_string(),
                cause_slug: cause.i18n_slug().to_string(),
            };
            ("text/html; charset=utf-8", renderer.render(&vars, locale))
        }
        ErrorRepresentation::Machine => {
            let mut payload = serde_json::json!({
                "error": {
                    "code": "USERAPP_PROXY_FAILURE",
                    "status": status,
                    "reason": reason,
                    "message": message,
                    "diagnostic_id": diagnostic_id,
                }
            });
            if let Some(failure) = wake_failure {
                attach_wake_diagnostic(&mut payload, failure, locale);
            }
            ("application/json", payload.to_string().into_bytes())
        }
    };

    let mut response = match ResponseHeader::build(status, None) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                diagnostic_id = %diagnostic_id,
                %status,
                "build userapp error response header failed: {error}"
            );
            return;
        }
    };
    // 字面量头名内联插入（insert_header 的名字生命周期与响应绑定；失败仅
    // 记日志——个别头写失败不撤销整个错误响应）
    macro_rules! insert {
        ($name:expr, $value:expr) => {
            if let Err(error) = response.insert_header($name, $value) {
                tracing::warn!(
                    "insert {} into error response failed: {error}",
                    stringify!($name)
                );
            }
        };
    }
    if let Some(failure) = wake_failure {
        insert!("x-rcoder-error-code", failure.code.as_ref());
        if let Some(operation_id) = &failure.operation_id {
            insert!("x-rcoder-operation-id", operation_id);
        }
    }
    insert!("content-type", content_type);
    insert!("content-length", body.len().to_string());
    insert!("cache-control", "no-store");
    insert!(DIAGNOSTIC_HEADER, diagnostic_id.as_str());
    if let Some(seconds) = retry_after_secs {
        insert!("retry-after", seconds.to_string());
    }
    // diagnostic_id 落 span 属性（Tempo 里按编号可直接查到该失败节点）
    span.record("rcoder.diagnostic_id", &diagnostic_id);
    span.record("rcoder.reason", reason);
    // detail 只进日志（错误链可能含内部地址/上游细节，不进页面与响应体）：
    // 用户报障给编号 → grep 一条命中即可读到根因线索，无须二次排查。
    // trace_id 与 span 同源：编号 → 本日志行 → Tempo 精确串联。
    tracing::error!(
        diagnostic_id = %diagnostic_id,
        trace_id = shared_types::current_otel_trace_id(),
        %status,
        reason,
        detail,
        context,
        representation = ?representation,
        "userapp proxy failure response"
    );
    if is_head {
        // HEAD：对应状态和响应头，无正文。
        if let Err(error) = session
            .write_response_header(Box::new(response), true)
            .await
        {
            tracing::warn!("write HEAD error response failed: {error}");
        }
        return;
    }
    if let Err(error) = session
        .write_response_header(Box::new(response), false)
        .await
    {
        tracing::warn!("write error response header failed: {error}");
        return;
    }
    if let Err(error) = session
        .write_response_body(Some(bytes::Bytes::from(body)), true)
        .await
    {
        tracing::warn!("write error response body failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer() -> ErrorPageRenderer {
        ErrorPageRenderer::new(None)
    }

    fn all_causes() -> [ErrorPageCause; 9] {
        [
            ErrorPageCause::Starting,
            ErrorPageCause::Stopped,
            ErrorPageCause::Failed,
            ErrorPageCause::Missing,
            ErrorPageCause::Blocked,
            ErrorPageCause::RecoveryRequired,
            ErrorPageCause::PlatformUnavailable,
            ErrorPageCause::OutcomeUnknown,
            ErrorPageCause::Generic,
        ]
    }

    /// R6：内置页按结构化档位驱动 UI——data-cause 注入档位 slug；固定
    /// 标签按 locale 渲染（默认英文页不再是中文标签；脚本不再解析标题）。
    #[test]
    fn builtin_page_renders_cause_slug_and_localized_ui_labels() {
        let renderer = renderer();
        let vars = ErrorPageVars {
            title: "t".into(),
            message: "m".into(),
            diagnostic_id: "d".into(),
            status: "503".into(),
            cause_slug: "starting".into(),
        };
        let zh = String::from_utf8(renderer.render(&vars, "zh-CN")).unwrap();
        assert!(
            zh.contains("data-cause=\"starting\""),
            "data-cause 必须携带档位 slug（脚本据此分支）"
        );
        assert!(zh.contains(">重新访问</button>"));
        assert!(zh.contains("链路诊断"));

        // 默认英文页：固定标签与 aria 全英文，无中文残留；档位标签三语一致结构。
        let en = String::from_utf8(renderer.render(&vars, shared_types::DEFAULT_LOCALE)).unwrap();
        assert!(en.contains("data-cause=\"starting\""));
        assert!(en.contains(">Reload</button>"), "英文按钮文案: {en}");
        assert!(en.contains(">Browser</span>"));
        assert!(en.contains("Path diagnostics"));
        assert!(
            !en.contains(">重新访问<")
                && !en.contains(">浏览器<")
                && !en.contains(">复制<")
                && !en.contains(">已连接<"),
            "默认英文页不得残留中文固定标签（可见文本）"
        );

        let tw = String::from_utf8(renderer.render(&vars, "zh-TW")).unwrap();
        assert!(tw.contains(">重新存取</button>"));
        assert!(tw.contains("data-cause=\"starting\""));
    }

    #[test]
    fn builtin_page_renders_all_variables_escaped() {
        let vars = ErrorPageVars {
            title: "应用<启动>".into(),
            message: "请稍后 & 重试".into(),
            diagnostic_id: "abc123".into(),
            status: "503".into(),
            cause_slug: "starting".into(),
        };
        let rendered = String::from_utf8(renderer().render(&vars, "zh-CN")).unwrap();
        assert!(rendered.contains("应用&lt;启动&gt;"));
        assert!(rendered.contains("请稍后 &amp; 重试"));
        assert!(rendered.contains("abc123"));
        assert!(rendered.contains("503"));
        assert!(rendered.contains("<html lang=\"zh-CN\">"));
        assert!(!rendered.contains("{{RCODER_"));
    }

    #[test]
    fn zh_cn_copywriting_matches_approved_matrix() {
        let cases = [
            (
                ErrorPageCause::Starting,
                "应用正在启动",
                "应用正在启动，耗时较长，请稍后重新访问。",
            ),
            (
                ErrorPageCause::Stopped,
                "应用已停止",
                "应用已停止，请在应用管理页面启动后重试。",
            ),
            (
                ErrorPageCause::Failed,
                "应用启动失败",
                "应用启动失败，请重新部署或稍后重试。",
            ),
            (
                ErrorPageCause::Missing,
                "应用不存在或已被回收",
                "访问的应用不存在或已被删除，无法访问；请确认应用状态或重新部署。",
            ),
            (
                ErrorPageCause::Blocked,
                "应用操作处理中",
                "应用正在执行停止或删除等操作，暂时无法访问，请稍后重试。",
            ),
            (
                ErrorPageCause::RecoveryRequired,
                "应用待恢复",
                "应用需要完成恢复处理后才能访问，请重新部署或联系管理员。",
            ),
            (
                ErrorPageCause::PlatformUnavailable,
                "平台服务暂不可用",
                "平台服务暂时不可用，请稍后重试；这与应用本身无关。",
            ),
            (
                ErrorPageCause::OutcomeUnknown,
                "应用状态确认中",
                "应用操作结果正在确认，请稍后重试；如持续出现请重新部署。",
            ),
            (
                ErrorPageCause::Generic,
                "应用暂时无法访问",
                "应用暂时无法访问，请稍后重试。",
            ),
        ];
        for (cause, title, message) in cases {
            let (actual_title, actual_message) = cause.copywriting("zh-CN");
            assert_eq!(actual_title, title, "title for {cause:?}");
            assert_eq!(actual_message, message, "message for {cause:?}");
        }
    }

    #[test]
    fn copywriting_covers_all_causes_in_all_locales() {
        for locale in ["zh-CN", "zh-TW", "en-US"] {
            for cause in all_causes() {
                let (title, message) = cause.copywriting(locale);
                // t() 缺条目时归一为裸 key——两条都不能是裸 key 形状（防漏
                // yml 条目，参照 2026-09-22 app-105 泄露事故的防线）
                for text in [&title, &message] {
                    assert!(!text.is_empty(), "{cause:?}/{locale} empty copy");
                    assert!(
                        !text.starts_with("userapp_error_page"),
                        "{cause:?}/{locale} leaked bare key: {text}"
                    );
                }
            }
        }
    }

    #[test]
    fn wake_failure_cause_maps_codes_to_aligned_causes() {
        let cases = [
            (shared_types::ERR_APP_NOT_FOUND, ErrorPageCause::Missing),
            (
                shared_types::ERR_CONTAINER_NOT_FOUND,
                ErrorPageCause::Missing,
            ),
            (shared_types::ERR_CONFLICT, ErrorPageCause::Blocked),
            (
                shared_types::ERR_OPERATION_IN_PROGRESS,
                ErrorPageCause::Blocked,
            ),
            (
                shared_types::ERR_RECOVERY_REQUIRED,
                ErrorPageCause::RecoveryRequired,
            ),
            (shared_types::ERR_IMAGE_PULL_FAILED, ErrorPageCause::Failed),
            (
                shared_types::ERR_CONTAINER_START_FAILED,
                ErrorPageCause::Failed,
            ),
            (
                shared_types::ERR_RUNTIME_TIMEOUT,
                ErrorPageCause::PlatformUnavailable,
            ),
            (
                shared_types::ERR_USERAPP_WAIT_TIMEOUT,
                ErrorPageCause::Starting,
            ),
            (
                shared_types::ERR_RUNTIME_UNAVAILABLE,
                ErrorPageCause::PlatformUnavailable,
            ),
            (
                shared_types::ERR_SERVICE_UNAVAILABLE,
                ErrorPageCause::PlatformUnavailable,
            ),
            (
                shared_types::ERR_DATABASE_NOT_READY,
                ErrorPageCause::PlatformUnavailable,
            ),
            (
                shared_types::ERR_RESOURCE_EXHAUSTED,
                ErrorPageCause::PlatformUnavailable,
            ),
            (
                shared_types::ERR_CONTAINER_ADDRESS_NOT_READY,
                ErrorPageCause::PlatformUnavailable,
            ),
            (
                shared_types::ERR_PROXY_SERVICE_UNAVAILABLE,
                ErrorPageCause::PlatformUnavailable,
            ),
            (
                shared_types::ERR_OPERATION_OUTCOME_UNKNOWN,
                ErrorPageCause::OutcomeUnknown,
            ),
            ("ERR_SOMETHING_ELSE", ErrorPageCause::Generic),
            (
                shared_types::ERR_USERAPP_WAKE_FAILED,
                ErrorPageCause::Generic,
            ),
        ];
        for (code, expected) in cases {
            let failure =
                shared_types::WakeFailure::new(code, "wake_wait", "mapping-table fixture");
            assert_eq!(
                wake_failure_cause(&failure),
                expected,
                "code {code} (stage wake_wait)"
            );
        }
    }

    /// R5 反例回归：前置查询/探测/配置读取的超时没有启动证据——按 stage
    /// 分类，不显示"正在启动"；有启动证据的等待段保持 Starting。
    #[test]
    fn timeout_stage_decides_starting_claim() {
        let pre_stages = [
            "wake_preflight",
            "wake_runtime_probe",
            "file_credentials_configuration",
        ];
        for stage in pre_stages {
            let failure = shared_types::WakeFailure::new(
                shared_types::ERR_RUNTIME_TIMEOUT,
                stage,
                "preflight timeout",
            );
            assert_eq!(
                wake_failure_cause(&failure),
                ErrorPageCause::PlatformUnavailable,
                "前置超时 stage={stage} 不得宣称正在启动"
            );
        }
        let failure = shared_types::WakeFailure::timeout(
            "wake_observation",
            Some("original-wake".into()),
            true,
        );
        assert_eq!(wake_failure_cause(&failure), ErrorPageCause::Starting);
    }

    #[test]
    fn broad_wake_timeout_does_not_claim_starting() {
        for stage in ["wake_wait", "wake_follower_wait"] {
            let failure = shared_types::WakeFailure::timeout(stage, None, false);
            assert_eq!(
                wake_failure_cause(&failure),
                ErrorPageCause::PlatformUnavailable,
                "{stage} 包含 leader 尚未受理的前置查询，不能宣称正在启动"
            );
            let with_attempt = shared_types::WakeFailure::timeout(
                stage,
                Some("attempt-not-yet-confirmed".into()),
                true,
            );
            assert_eq!(
                wake_failure_cause(&with_attempt),
                ErrorPageCause::PlatformUnavailable,
                "外围等待即便有尝试 ID，也不是已确认启动写入的观察阶段"
            );
        }
        let observation = shared_types::WakeFailure::timeout("wake_observation", None, true);
        assert_eq!(
            wake_failure_cause(&observation),
            ErrorPageCause::PlatformUnavailable,
            "观察阶段缺失原操作身份不能作为启动证据"
        );
    }

    #[test]
    fn page_cause_maps_not_deployed_to_missing() {
        let hint = UserAppProxyFailureHint {
            readiness_status: Some(shared_types::UserAppReadinessStatus::NotDeployed),
            error_origin_confirmed: false,
        };
        assert_eq!(hint.page_cause(), ErrorPageCause::Missing);
        let unknown = UserAppProxyFailureHint {
            readiness_status: None,
            error_origin_confirmed: false,
        };
        assert_eq!(unknown.page_cause(), ErrorPageCause::Generic);
    }

    #[test]
    fn cause_copywriting_covers_all_categories() {
        for cause in all_causes() {
            let (title, message) = cause.copywriting(shared_types::DEFAULT_LOCALE);
            assert!(!title.is_empty());
            assert!(!message.is_empty());
        }
    }
}

#[cfg(test)]
mod wake_diagnostic_tests {
    #[test]
    fn wake_page_keeps_original_operation_stage_and_safe_detail() {
        let mut payload = serde_json::json!({"error":{"code":"USERAPP_PROXY_FAILURE"}});
        let blocker = shared_types::UserAppOperationBlocker {
            scope: shared_types::UserAppOperationScope::Prod,
            operation_id: "original-blocking-stop".into(),
            kind: shared_types::UserAppOperationKind::Stop,
            state: shared_types::UserAppOperationState::Running,
            step: "stop".into(),
        };
        let failure = shared_types::WakeFailure {
            operation_id: Some("original-wake".into()),
            blocker: Some(Box::new(blocker.clone())),
            ..shared_types::WakeFailure::new(
                shared_types::ERR_RUNTIME_CONFIGURATION,
                "wake_runtime",
                "POSTGRES_PASSWORD=private_marker",
            )
        };
        super::attach_wake_diagnostic(&mut payload, &failure, "en-US");
        assert_eq!(payload["error"]["code"], "USERAPP_PROXY_FAILURE");
        assert_eq!(payload["error"]["operation_id"], "original-wake");
        assert_eq!(payload["error"]["blocker"], serde_json::json!(blocker));
        assert_eq!(payload["error"]["error_detail"]["stage"], "wake_runtime");
        assert_eq!(
            payload["error"]["error_detail"]["reason_code"],
            shared_types::ERR_RUNTIME_CONFIGURATION
        );
        assert!(!payload.to_string().contains("private_marker"));
    }
}
