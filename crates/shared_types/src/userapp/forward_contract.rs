//! userApp 转发分流契约常量（rcoder 转发层与容器内 file-server 共用——
//! 按"跨 crate 契约定义在 shared_types"约定收口，消除两侧各自定义的漂移面）。

/// userApp 场景标记 header（反向代理/Java 注入）：值 [`SERVICE_TYPE_USERAPP`] 时
/// rcoder 拦截层短路转发到该 app 的开发容器，容器内 computer handler 据此切换
/// workspace 到开发卷。HTTP header 名小写（HTTP/1.1 大小写不敏感）。
pub const SERVICE_TYPE_HEADER: &str = "x-service-type";

/// userApp 场景标记值（与 /api/v1/userapp 前缀对齐；chat body 的 `service_type`
/// 字段同词表）。匹配不区分大小写（[`is_userapp_service_type_value`] 归一后比较），
/// 但推荐 wire 上一律传全小写 `userapp`。
pub const SERVICE_TYPE_USERAPP: &str = "userapp";

/// pageApp 场景标记值（对齐 TS 1.4.5 `SERVICE_TYPE.PAGEAPP`，驼峰值）。
pub const SERVICE_TYPE_PAGE_APP: &str = "pageApp";

/// normalProject（常规项目，主容器共享工作区）标记值（对齐 TS 1.4.5
/// `SERVICE_TYPE.NORMAL_PROJECT`；工作区 `{CWS}/{userId}/NormalProject/{projectId}`，
/// projectId 复用 `x-app-id`/`appId` 通道传递）。
pub const SERVICE_TYPE_NORMAL_PROJECT: &str = "normalProject";

/// taskAgent（通用智能体/缺省类型）标记值（对齐 TS 1.4.5
/// `SERVICE_TYPE.TASK_AGENT`；TS 终态无 `general` 兼容，未匹配值回落本档）。
pub const SERVICE_TYPE_TASK_AGENT: &str = "taskAgent";

/// `x-service-type` 值归一化后的服务场景类型（对齐 TS `resolveServiceContext`
/// 的 `normalizedType`：大小写不敏感匹配四值词表，未匹配 → None，调用方按
/// 缺省 [`ComputerServiceKind::TaskAgent`] 处理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputerServiceKind {
    /// userApp（子容器，工作区切开发卷 `{UWS}/{appId}`）。
    Userapp,
    /// pageApp（主容器，工作区沿用 `{CWS}/{userId}/{cId}`）。
    PageApp,
    /// normalProject（主容器共享工作区 `{CWS}/{userId}/NormalProject/{projectId}`）。
    NormalProject,
    /// taskAgent（通用智能体，`{CWS}/{userId}/{cId}`；缺省档）。
    TaskAgent,
}

/// 把 `x-service-type` 原始值归一化为 [`ComputerServiceKind`]。
///
/// trim + ASCII 小写后与四值词表匹配（`PageApp`/`NormalProject` 等驼峰变体
/// 与全小写变体均命中，对齐 TS `Object.values(SERVICE_TYPE).find(...)` 大小写
/// 不敏感归一）。未匹配（含空值）返回 `None`——由调用方决定缺省语义（TS 缺省
/// taskAgent，无 general 兼容）。
pub fn normalize_computer_service_type(value: &str) -> Option<ComputerServiceKind> {
    let key = value.trim().to_ascii_lowercase();
    [
        (SERVICE_TYPE_USERAPP, ComputerServiceKind::Userapp),
        (SERVICE_TYPE_PAGE_APP, ComputerServiceKind::PageApp),
        (
            SERVICE_TYPE_NORMAL_PROJECT,
            ComputerServiceKind::NormalProject,
        ),
        (SERVICE_TYPE_TASK_AGENT, ComputerServiceKind::TaskAgent),
    ]
    .into_iter()
    .find(|(wire, _)| key == wire.to_ascii_lowercase())
    .map(|(_, kind)| kind)
}

/// 判断 `x-service-type` header 值是否标记 userApp 场景。
///
/// 值经 trim + ASCII 小写归一后与 [`SERVICE_TYPE_USERAPP`] 比较——`userapp` /
/// `Userapp` / `USERAPP` 等大小写变体均命中（Java 侧约定不区分大小写）。
/// 三个消费点（rcoder computer_intercept、file-server-proxy 60000 分流、
/// 容器内 file-server scope 注入）统一走本函数，禁止各自重写比较逻辑。
pub fn is_userapp_service_type_value(value: &str) -> bool {
    normalize_computer_service_type(value) == Some(ComputerServiceKind::Userapp)
}

/// 开发容器定位 header：Java 调 rcoder 主服务的所有 userApp 请求统一携带，
/// rcoder 零 body 解析定位 per-app 开发容器（multipart/SSE 全覆盖）。
pub const APP_ID_HEADER: &str = "x-app-id";

/// dev/prod 阶段分派 header（文件操作转发目标）：`dev`（缺省，开发容器 builder）
/// / `prod`（生产运行容器，唤醒后转发其 :60000）。与 pod 接口族 `app_stage`
/// 字段同词表——同一 app_id 可同时存在 builder 与生产 Deployment，必须显式区分。
pub const APP_STAGE_HEADER: &str = "x-app-stage";

/// userApp owner 显式档 header（Java 出站统一携带）：rcoder 拦截/透传层零 body
/// 解析即可拿到懒创建开发容器的 owner user_id（owner 三档解析的显式档）。
/// 缺失/空白 = 未传（降级 metadata 兜底）；值须过 identifier 白名单
/// （进宿主树路径 `dev/{user_id}/{app_id}` 拼接，防逃逸——与 app_id 同源）。
pub const USER_ID_HEADER: &str = "x-user-id";

/// 用户维度工作目录 header（对齐 TS nuwax-file-server 1.4.5 `resolveServiceContext`，
/// 原名 `x-workspace-dir` 随 f979df7→00134aa 改名）：Java/前端注入，值为一跨平台
/// 绝对路径（POSIX `/a/b`、Windows 盘符 `C:/a/b`、UNC `//server/share/a/b`）。
/// computer 域工作区定位收口（file-server `computer_root_for_request`）据此把
/// 工作区切到该目录——优先认传入（高于 `X-Service-Type` 分流与默认规则）；
/// 合法性（绝对/无点段/长度/控制字符）由收口处 `normalize_workspace_path`
/// （file-server 侧）fail-fast 校验，header 本身不做归一。与 body/query 的
/// `workspacePath` 字段同语义（header 优先）。
pub const WORKSPACE_PATH_HEADER: &str = "x-workspace-path";

/// [`APP_STAGE_HEADER`] 的值：开发阶段（UserappBuilder 开发容器）。
pub const APP_STAGE_DEV: &str = "dev";

/// [`APP_STAGE_HEADER`] 的值：生产阶段（Userapp 运行容器）。
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

    /// x-service-type 值匹配：大小写不敏感 + 前后空白容忍；非 userapp 值不命中。
    #[test]
    fn service_type_value_matching_is_case_insensitive() {
        assert!(is_userapp_service_type_value("userapp"));
        assert!(is_userapp_service_type_value("Userapp"));
        assert!(is_userapp_service_type_value("USERAPP"));
        assert!(is_userapp_service_type_value("  userapp  "));
        assert!(!is_userapp_service_type_value("user-app"));
        assert!(!is_userapp_service_type_value("computer-agent-runner"));
        assert!(!is_userapp_service_type_value(""));
    }

    /// 四值词表归一化：驼峰与全小写变体均命中，未匹配（含 general 旧值）→ None
    /// （对齐 TS 1.4.5 `normalizedType`——general 兼容已在 TS 6321f7e 删除）。
    #[test]
    fn normalize_computer_service_type_matches_full_vocabulary() {
        use ComputerServiceKind as K;
        for (raw, expected) in [
            ("userapp", K::Userapp),
            ("Userapp", K::Userapp),
            (" pageApp ", K::PageApp),
            ("pageapp", K::PageApp),
            ("PageApp", K::PageApp),
            ("normalProject", K::NormalProject),
            ("NormalProject", K::NormalProject),
            ("normalproject", K::NormalProject),
            ("taskAgent", K::TaskAgent),
            ("TASKAGENT", K::TaskAgent),
        ] {
            assert_eq!(
                normalize_computer_service_type(raw),
                Some(expected),
                "raw value {raw:?} must normalize to {expected:?}"
            );
        }
        // 未匹配（含 general 旧值、编排层枚举值、空串）→ None（缺省由调用方定）
        for raw in ["general", "generalAgent", "user-app", "", "  "] {
            assert_eq!(
                normalize_computer_service_type(raw),
                None,
                "raw value {raw:?} must not match the vocabulary"
            );
        }
    }

    /// userApp 分派 header 名族统一小写 `x-` 前缀（HTTP/1.1 大小写不敏感，
    /// HeaderMap 归一小写比较——一处改名另一处漂移即在此报红）。
    #[test]
    fn userapp_dispatch_headers_share_x_prefix() {
        assert_eq!(SERVICE_TYPE_HEADER, "x-service-type");
        assert_eq!(APP_ID_HEADER, "x-app-id");
        assert_eq!(APP_STAGE_HEADER, "x-app-stage");
        assert_eq!(USER_ID_HEADER, "x-user-id");
    }

    /// 用户维度工作目录 header 名与 TS `resolveServiceContext` 读取的 header 名
    /// 逐字一致（对齐 nuwax-file-server 1.4.5，00134aa 自 `x-workspace-dir`
    /// 改名）——一侧改名另一侧漂移即在此报红。
    #[test]
    fn workspace_path_header_matches_ts_contract() {
        assert_eq!(WORKSPACE_PATH_HEADER, "x-workspace-path");
    }
}
