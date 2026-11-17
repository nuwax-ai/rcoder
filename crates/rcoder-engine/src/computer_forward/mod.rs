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

pub use intercept::computer_rbd_intercept;

use shared_types::{ServiceType, USER_ID_HEADER};

/// computer 域转发透传清单（path 前缀/模板，method 透传不分类）。
///
/// 与 userapp `CONTAINER_PASS_THROUGH_PATHS` 同治理模式：显式枚举 + 守卫测试
/// 双向闭包（容器 file-server 加接口不登记当场报红）。来源 = plan 文档 §3
/// 逐路由分类表（29 computer 全量转发 + build 域 computer 会话 7 条；
/// `parse-build-error`/`get-log-cache-stats`/`clear-all-log-cache` 不碰用户
/// 工作区留本地）。
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
    // /api/build（computer 会话 7——web 会话仍留本地, 会话维度分流由接线层判）
    "/api/build/start-dev",
    "/api/build/stop-dev",
    "/api/build/restart-dev",
    "/api/build/list-dev",
    "/api/build/keep-alive",
    "/api/build/port-pool-status",
    "/api/build/get-dev-log",
    "/api/build/build-project",
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

/// 三级 fallback 目标定位。输入零 body——GET query 与路径段解析不缓冲请求体,
/// multipart/SSE 全覆盖。
///
/// 校验: `shared_types::validate_identifier` 白名单（user_id 原样进容器标识与
/// PVC 名拼接, 含 `/` 即越界）, 非法值返回 `None`（调用方 400, 不静默降级）。
pub fn resolve_target(
    user_id_header: Option<&str>,
    raw_query: Option<&str>,
    path: &str,
) -> Option<ComputerForwardTarget> {
    // ① header（最终契约）
    if let Some(raw) = user_id_header.map(str::trim).filter(|s| !s.is_empty()) {
        if valid_identifier(raw) {
            return Some(ComputerForwardTarget {
                user_id: raw.to_string(),
                source: TargetSource::Header,
            });
        }
        return None;
    }
    // ② GET query userId（现有 Java 流量兜底; 解析零缓冲）
    if let Some(query) = raw_query {
        if let Some(raw) = query_param(query, "userId") {
            if valid_identifier(&raw) {
                return Some(ComputerForwardTarget {
                    user_id: raw,
                    source: TargetSource::Query,
                });
            }
            return None;
        }
    }
    // ③ static 路径段: /api/computer/static/{user_id}/...（分享链路, 无 header 场景）
    static_path_user_id(path).map(|user_id| ComputerForwardTarget {
        user_id,
        source: TargetSource::StaticPath,
    })
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
    COMPUTER_PASS_THROUGH_PATHS.iter().any(|tmpl| path_matches(tmpl, path))
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

    #[test]
    fn target_from_header_wins() {
        let t = resolve_target(
            Some(" u_1001 "),
            Some("userId=u_999&x=1"),
            "/api/computer/get-file-list",
        )
        .expect("header target");
        assert_eq!(t.user_id, "u_1001");
        assert_eq!(t.source, TargetSource::Header);
    }

    #[test]
    fn target_falls_back_to_query() {
        let t = resolve_target(None, Some("userId=6&cId=1696379"), "/api/computer/resolve-file")
            .expect("query target");
        assert_eq!(t.user_id, "6");
        assert_eq!(t.source, TargetSource::Query);
    }

    #[test]
    fn target_from_static_path_segment() {
        // 分享链路: 浏览器直出, 无 header 无 query userId
        let t = resolve_target(None, Some("sk=abc"), "/api/computer/static/6/1696379/test.txt")
            .expect("static target");
        assert_eq!(t.user_id, "6");
        assert_eq!(t.source, TargetSource::StaticPath);
    }

    #[test]
    fn static_path_requires_trailing_segments() {
        // 只有 user 段没有后续路径 → 不是合法 static 请求
        assert!(resolve_target(None, None, "/api/computer/static/6").is_none());
    }

    #[test]
    fn invalid_identifier_rejected_not_degraded() {
        // 越界值（路径穿越/空）→ None（调用方 400）, 绝不静默降级到下一级
        assert!(resolve_target(Some("../etc"), None, "/x").is_none());
        assert!(resolve_target(Some(""), None, "/x").is_none());
    }

    #[test]
    fn pass_through_table_matches_and_rejects() {
        assert!(is_pass_through_path("/api/computer/get-file-list"));
        assert!(is_pass_through_path("/api/build/build-project"));
        assert!(is_pass_through_path("/api/computer/static/6/1696379/deep/dir/f.txt"));
        // 未登记路径不被前缀吞并
        assert!(!is_pass_through_path("/api/computer/staticx/6"));
        assert!(!is_pass_through_path("/api/computer/unknown-route"));
        assert!(!is_pass_through_path("/api/build/parse-build-error")); // 留本地
    }

    #[test]
    fn service_type_of_target_domain() {
        // 记录目标族语义: computer 域目标即 ComputerAgentRunner（PVC 命名键）
        let st = ServiceType::ComputerAgentRunner;
        assert_eq!(st.to_string(), "computer-agent-runner");
    }
}
