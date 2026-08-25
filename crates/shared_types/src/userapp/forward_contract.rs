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

/// dev/prod 阶段分派 header（文件操作转发目标）：`dev`（缺省，开发容器 builder）
/// / `prod`（生产运行容器，唤醒后转发其 :60000）。与 pod 接口族 `app_stage`
/// 字段同词表——同一 app_id 可同时存在 builder 与生产 Deployment，必须显式区分。
pub const APP_STAGE_HEADER: &str = "x-app-stage";

/// [`APP_STAGE_HEADER`] 的值：开发阶段（UserAppBuilder 开发容器）。
pub const APP_STAGE_DEV: &str = "dev";

/// [`APP_STAGE_HEADER`] 的值：生产阶段（UserApp 运行容器）。
pub const APP_STAGE_PROD: &str = "prod";

#[cfg(test)]
mod tests {
    use super::*;

    /// chat body 的 service_type 枚举 wire 值必须与 X-Service-Type header 值同词表
    /// （chat 分支与转发分流共用 `userapp` 标记）——一处改名另一处漂移即在此报红。
    #[test]
    fn chat_scope_wire_matches_header_value() {
        let wire = serde_json::to_value(crate::ChatServiceScope::Userapp).expect("serialize");
        assert_eq!(
            wire.as_str().expect("string variant"),
            SERVICE_TYPE_USERAPP,
            "ChatServiceScope::Userapp wire 值与 SERVICE_TYPE_USERAPP 漂移"
        );
    }

    /// pod 接口族 app_stage 值域与转发层 x-app-stage 值域同词表（dev/prod）。
    #[test]
    fn app_stage_values_match_pod_field_vocabulary() {
        assert_eq!(APP_STAGE_DEV, "dev");
        assert_eq!(APP_STAGE_PROD, "prod");
    }
}
