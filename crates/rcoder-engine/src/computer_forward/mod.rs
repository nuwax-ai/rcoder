//! computer 域 per-agent RBD 转发层——目标定位与透传清单（P1 纯逻辑核心）。
//!
//! 架构（plan 文档 computer-rbd-file-server-plan.md §5.1）：
//! RBD 用户的文件/构建请求不再由 rcoder 挂卷直读，而是转发到该用户 agent
//! 容器内嵌 file-server（:8086，LocalWorkspaceResolver=/home/user 本地视角）。
//! 本模块提供转发前置的两项纯决策：
//! - [`resolve_target`]：三级 fallback 定位目标用户（header > query > 路径段）
//! - [`COMPUTER_PASS_THROUGH_PATHS`]：显式透传清单（守卫测试对齐容器路由文档）
//!
//! 转发内核（流式 + hop-by-hop 剥离）复用 `userapp_forward::upstream` 同款
//! 语义；混住判据（per-agent PVC 存在性 + TTL 缓存）与 wake-on-forward
//! （ensure STS → 等 Ready ≤8s → 转发, 超时 503+Retry-After）随 P1 接线落地。

pub mod intercept;
pub mod orchestrate;

pub use intercept::computer_rbd_intercept;

/// computer 域转发透传清单（path 前缀/模板，method 透传不分类）。
///
/// 与 userapp `CONTAINER_PASS_THROUGH_PATHS` 同治理模式：显式枚举。来源 =
/// plan 文档 §3 逐路由分类表（computer 27 条全量 + static 分享链路终点）。
///
/// ⚠️ `/api/build/*` **暂不登记**：build 域需按会话维度分流（computer 会话
/// 转发、web 会话留本地），而接线层当前只判 userapp 让行——web 会话一旦带
/// user 维度定位参数就会被误转发进 computer agent 容器（错工作区）。
/// 待会话维度判据落地后按 plan §3.3 补登记（含 7 条转发 + 3 条本地）。
///
/// 不碰用户工作区留本地：`parse-build-error`/`get-log-cache-stats`/
/// `clear-all-log-cache`（纯计算/进程内缓存）。
pub const COMPUTER_PASS_THROUGH_PATHS: &[&str] = &[
    // /api/computer（file-server computer_router 全量, 29）
    "/api/computer/fs-roots",
    "/api/computer/fs-children",
    "/api/computer/fs-mkdir",
    "/api/computer/fs-rename",
    "/api/computer/get-file-list",
    "/api/computer/resolve-file",
    "/api/computer/search-files",
    "/api/computer/get-file-meta",
    "/api/computer/delete-workspace",
    "/api/computer/get-logs",
    "/api/computer/execute-command",
    "/api/computer/install-project",
    "/api/computer/zip-workspace",
    "/api/computer/download-all-files",
    "/api/computer/files-update",
    "/api/computer/upload-file",
    "/api/computer/upload-files",
    "/api/computer/generate-file",
    "/api/computer/import-project",
    "/api/computer/cleanup-build-artifacts",
    "/api/computer/create-workspace",
    "/api/computer/create-workspace-v2",
    "/api/computer/push-skills-to-workspace",
    "/api/computer/push-skills-to-workspace-v2",
    "/api/computer/init-project-template",
    "/api/computer/build-agent-package",
    "/api/computer/static/{user_id}/{c_id}/{*rest}", // 分享链路终点/产物下载/打包读取
];

/// 转发目标（per-agent RBD 用户的 agent 容器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerForwardTarget {
    /// 平台 user_id（per-agent PVC 名与共享卷子目录同键）。
    pub user_id: String,
    /// 定位来源（观测/日志用）。
    pub source: TargetSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSource {
    /// `x-user-id` header（最终契约, Java 集成阶段注入; 开发期 curl 模拟）。
    Header,
    /// GET query `userId`（现有 Java 流量已带——resolve-file/get-file-list 等）。
    Query,
    /// `/api/computer/static/{user_id}/...` 路径段（分享链路浏览器直出,
    /// 天然无自定义 header; 与 userapp `/static/{app_id}` 同款模式）。
    StaticPath,
}

/// 三级 fallback 目标定位的结果语义。
///
/// - `Absent`：无任何定位来源 → 调用方**放行**（共享用户本地直读）
/// - `Invalid`：有来源但值非法（越界/格式错）→ 调用方 **400**。
///   与 Absent 分开是关键：非法值静默放行会让 RBD 用户的请求落到本地共享
///   卷读到空数据（看似成功的错误响应），必须显式拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetResolution {
    Absent,
    Invalid,
    Found(ComputerForwardTarget),
}

/// 三级 fallback 目标定位。输入零 body——GET query 与路径段解析不缓冲请求体,
/// multipart/SSE 全覆盖。
///
/// 校验: `shared_types::validate_identifier` 白名单（user_id 原样进容器标识与
/// PVC 名拼接, 含 `/` 即越界）。query 值先 percent-decode（`userId=%75_1001`
/// 是合法编码形态, 不解码会把合法用户误判非法）。
pub fn resolve_target(
    user_id_header: Option<&str>,
    raw_query: Option<&str>,
    path: &str,
) -> TargetResolution {
    // ① header（最终契约）
    if let Some(raw) = user_id_header.map(str::trim).filter(|s| !s.is_empty()) {
        return if valid_identifier(raw) {
            TargetResolution::Found(ComputerForwardTarget {
                user_id: raw.to_string(),
                source: TargetSource::Header,
            })
        } else {
            TargetResolution::Invalid
        };
    }
    // ② GET query userId（现有 Java 流量兜底; 解析零缓冲）
    if let Some(query) = raw_query {
        if let Some(raw) = query_param(query, "userId") {
            let decoded = percent_decode(&raw);
            return if valid_identifier(&decoded) {
                TargetResolution::Found(ComputerForwardTarget {
                    user_id: decoded,
                    source: TargetSource::Query,
                })
            } else {
                TargetResolution::Invalid
            };
        }
    }
    // ③ static 路径段: /api/computer/static/{user_id}/...（分享链路, 无 header 场景）
    match static_path_user_id(path) {
        Some(user_id) => TargetResolution::Found(ComputerForwardTarget {
            user_id,
            source: TargetSource::StaticPath,
        }),
        None => TargetResolution::Absent,
    }
}

/// percent-decode（query 值用）: `%75_1001` → `u_1001`。无 `%` 快路径原样返回;
/// 非 UTF-8 序列保留原样（随后被 validate_identifier 拒绝）。
fn percent_decode(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

/// static 路由路径段定位：`/api/computer/static/{user_id}/...` 首段即 user_id。
/// 容器内视角根 = `{COMPUTER_WORKSPACE_DIR}/{user_id}/{c_id}`（与 rcoder 侧
/// 三段路径同构），转发时原样透传路径（容器 file-server 自己解析）。
fn static_path_user_id(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/api/computer/static/")?;
    let user_id = rest.split('/').next()?;
    (valid_identifier(user_id) && rest.contains('/')).then(|| user_id.to_string())
}

fn valid_identifier(value: &str) -> bool {
    shared_types::validate_identifier(value, "userId").is_ok()
}

/// 极简 query 参数提取（percent-decoding 不做——user_id 词表内无编码字符;
/// 其他参数原样透传, 不在此重写 URL）。
fn query_param<'a>(query: &'a str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key && !v.is_empty()).then(|| v.to_string())
    })
}

/// 该路径是否在透传清单内（转发前过滤; 精确段匹配不前缀吞并——
/// `/api/computer/static-x` 不得被 static 模板误吞）。
pub fn is_pass_through_path(path: &str) -> bool {
    COMPUTER_PASS_THROUGH_PATHS
        .iter()
        .any(|tmpl| path_matches(tmpl, path))
}

/// 模板匹配: `{seg}` 单段通配, `{*rest}` 尾部通配（对齐 axum 语法子集）。
fn path_matches(tmpl: &str, path: &str) -> bool {
    let mut t = tmpl.split('/').peekable();
    let mut p = path.split('/').peekable();
    loop {
        match (t.peek(), p.peek()) {
            (None, None) => return true,
            (Some(&tt), _) if tt.starts_with('{') && tt.contains("*rest") => return true,
            (Some(&tt), Some(&pp)) => {
                if tt.starts_with('{') || tt == pp {
                    t.next();
                    p.next();
                } else {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::ServiceType;

    fn found(resolution: TargetResolution) -> ComputerForwardTarget {
        match resolution {
            TargetResolution::Found(target) => target,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn target_from_header_wins() {
        let t = found(resolve_target(
            Some(" u_1001 "),
            Some("userId=u_999&x=1"),
            "/api/computer/get-file-list",
        ));
        assert_eq!(t.user_id, "u_1001");
        assert_eq!(t.source, TargetSource::Header);
    }

    #[test]
    fn target_falls_back_to_query() {
        let t = found(resolve_target(
            None,
            Some("userId=6&cId=1696379"),
            "/api/computer/resolve-file",
        ));
        assert_eq!(t.user_id, "6");
        assert_eq!(t.source, TargetSource::Query);
    }

    #[test]
    fn target_from_static_path_segment() {
        // 分享链路: 浏览器直出, 无 header 无 query userId
        let t = found(resolve_target(
            None,
            Some("sk=abc"),
            "/api/computer/static/6/1696379/test.txt",
        ));
        assert_eq!(t.user_id, "6");
        assert_eq!(t.source, TargetSource::StaticPath);
    }

    #[test]
    fn static_path_requires_trailing_segments() {
        // 只有 user 段没有后续路径 → Absent（放行本地）, 不是 Invalid
        assert_eq!(
            resolve_target(None, None, "/api/computer/static/6"),
            TargetResolution::Absent
        );
    }

    #[test]
    fn invalid_identifier_is_invalid_not_absent() {
        // 有来源但值非法 → Invalid（调用方 400）；空 header 值视为"无来源"→ Absent
        assert_eq!(
            resolve_target(Some("../etc"), None, "/x"),
            TargetResolution::Invalid
        );
        assert_eq!(
            resolve_target(Some(""), None, "/x"),
            TargetResolution::Absent
        );
        assert_eq!(
            resolve_target(None, Some("userId=..%2Fetc"), "/x"),
            TargetResolution::Invalid
        );
    }

    #[test]
    fn query_value_is_percent_decoded() {
        // %75_1001 → u_1001（合法编码形态, 不解码会误判非法）
        let t = found(resolve_target(None, Some("userId=%75_1001"), "/x"));
        assert_eq!(t.user_id, "u_1001");
    }

    #[test]
    fn pass_through_table_matches_and_rejects() {
        assert!(is_pass_through_path("/api/computer/get-file-list"));
        assert!(is_pass_through_path(
            "/api/computer/static/6/1696379/deep/dir/f.txt"
        ));
        // 未登记路径不被前缀吞并
        assert!(!is_pass_through_path("/api/computer/staticx/6"));
        assert!(!is_pass_through_path("/api/computer/unknown-route"));
        // build 族本期整体摘出(web 会话分流判据未实现, 防误转发)
        assert!(!is_pass_through_path("/api/build/build-project"));
        assert!(!is_pass_through_path("/api/build/parse-build-error"));
    }

    #[test]
    fn service_type_of_target_domain() {
        // 记录目标族语义: computer 域目标即 ComputerAgentRunner（PVC 命名键）
        let st = ServiceType::ComputerAgentRunner;
        assert_eq!(st.to_string(), "computer-agent-runner");
    }
}
