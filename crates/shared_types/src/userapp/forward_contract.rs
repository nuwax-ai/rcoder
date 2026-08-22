//! userApp 转发分流契约常量（rcoder 转发层与容器内 file-server 共用——
//! 按"跨 crate 契约定义在 shared_types"约定收口，消除两侧各自定义的漂移面）。

/// userApp 场景标记 header（反向代理/Java 注入）：值 [`SERVICE_TYPE_USERAPP`] 时
/// rcoder 拦截层短路转发到该 app 的开发容器，容器内 computer handler 据此切换
/// workspace 到开发卷。HTTP header 名小写（HTTP/1.1 大小写不敏感）。
pub const SERVICE_TYPE_HEADER: &str = "x-service-type";

/// userApp 场景标记值（与 /api/userapp 前缀对齐；chat body 的 `service_type`
/// 字段同词表）。
pub const SERVICE_TYPE_USERAPP: &str = "userapp";

/// 开发容器定位 header：Java 调 rcoder 主服务的所有 userApp 请求统一携带，
/// rcoder 零 body 解析定位 per-app 开发容器（multipart/SSE 全覆盖）。
pub const APP_ID_HEADER: &str = "x-app-id";
