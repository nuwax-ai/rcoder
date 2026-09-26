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
    pub fn render(&self, vars: &ErrorPageVars) -> Vec<u8> {
        let template = self.template();
        let rendered = template
            .replace(VAR_TITLE, &html_escape(&vars.title))
            .replace(VAR_MESSAGE, &html_escape(&vars.message))
            .replace(VAR_DIAGNOSTIC_ID, &html_escape(&vars.diagnostic_id))
            .replace(VAR_STATUS, &html_escape(&vars.status));
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
}

/// 失败原因分类（决定文案；T8 首批仅确定性上下文，证据不足一律通用文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPageCause {
    /// 有当前实例的启动证据
    Starting,
    /// 已确认停止
    Stopped,
    /// 有明确启动失败证据
    Failed,
    /// 连接失败/状态未知：不凭连接拒绝宣称"正在启动"
    Generic,
}

impl ErrorPageCause {
    /// 平台预设文案（中文；公共页面不含内部地址/凭据/操作细节）。
    pub fn copywriting(self) -> (&'static str, &'static str) {
        match self {
            Self::Starting => ("应用正在启动", "应用正在启动，请稍后重新访问。"),
            Self::Stopped => ("应用已停止", "应用已停止，请在应用管理页面启动后重试。"),
            Self::Failed => (
                "应用启动失败",
                "应用启动失败，请联系应用维护者查看启动日志。",
            ),
            Self::Generic => (
                "应用暂时无法访问",
                "应用暂时无法访问，请稍后重试；若持续出现，请联系应用维护者。",
            ),
        }
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
    /// 宣称"正在启动"）。
    pub fn page_cause(&self) -> ErrorPageCause {
        match self.readiness_status {
            Some(shared_types::UserAppReadinessStatus::Starting) => ErrorPageCause::Starting,
            Some(shared_types::UserAppReadinessStatus::Stopped) => ErrorPageCause::Stopped,
            Some(shared_types::UserAppReadinessStatus::Failed) => ErrorPageCause::Failed,
            _ => ErrorPageCause::Generic,
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
    span: &tracing::Span,
) {
    // 错误页被消费 → 触发按需刷新（K8s 投射/手工换页的最终收敛路径之一）
    renderer.request_refresh();
    let diagnostic_id = new_diagnostic_id();
    let (title, message) = cause.copywriting();
    let representation = negotiate(session);
    let is_head = session.req_header().method == pingora::http::Method::HEAD;

    let (content_type, body): (&'static str, Vec<u8>) = match representation {
        ErrorRepresentation::Document => {
            let vars = ErrorPageVars {
                title: title.to_string(),
                message: message.to_string(),
                diagnostic_id: diagnostic_id.clone(),
                status: status.to_string(),
            };
            ("text/html; charset=utf-8", renderer.render(&vars))
        }
        ErrorRepresentation::Machine => {
            let payload = serde_json::json!({
                "error": {
                    "code": "USERAPP_PROXY_FAILURE",
                    "status": status,
                    "reason": reason,
                    "message": message,
                    "diagnostic_id": diagnostic_id,
                }
            });
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

    #[test]
    fn builtin_page_renders_all_variables_escaped() {
        let vars = ErrorPageVars {
            title: "应用<启动>".into(),
            message: "请稍后 & 重试".into(),
            diagnostic_id: "abc123".into(),
            status: "503".into(),
        };
        let rendered = String::from_utf8(renderer().render(&vars)).unwrap();
        assert!(rendered.contains("应用&lt;启动&gt;"));
        assert!(rendered.contains("请稍后 &amp; 重试"));
        assert!(rendered.contains("abc123"));
        assert!(rendered.contains("503"));
        assert!(!rendered.contains("{{RCODER_"));
    }

    #[test]
    fn cause_copywriting_covers_all_categories() {
        for cause in [
            ErrorPageCause::Starting,
            ErrorPageCause::Stopped,
            ErrorPageCause::Failed,
            ErrorPageCause::Generic,
        ] {
            let (title, message) = cause.copywriting();
            assert!(!title.is_empty());
            assert!(!message.is_empty());
        }
    }
}
