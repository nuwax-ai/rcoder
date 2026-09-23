//! 终端路由 query 参数契约（`/computer/ttyd/...` 与 userapp dev ttyd 共用）。
//!
//! 浏览器原生 WebSocket API 无法设置自定义请求头，业务场景（service_type）与
//! 终端初始目录（cwd）只能经 URL query 传递：
//!
//! - `service_type`：值须能解析为 [`shared_types::ServiceType`]；computer 路由
//!   进一步要求 `is_computer_family()`，userapp dev 路由不允许出现（定位键恒为
//!   app_id，服务类型不由客户端改写）。
//! - `cwd`：绝对路径，走 [`shared_types::normalize_absolute_dir`] 多平台归一
//!   （POSIX/盘符/UNC、拒点段/控制字符、限长），归一值经 `X-Ttyd-Cwd` header
//!   传给 agent_runner 的 ws_terminal（浏览器场景 header 不可用，query 是唯一
//!   载体）。
//!
//! 解码语义：form 形态（`+` = 空格、`%2B` = 字面 `+`），与 URLSearchParams/
//! Java 客户端编码器一致。重复键 last-wins；`?cwd=` 空值按非绝对路径 400。
//! query 整体按 [`crate::service::utils::rewrite_uri`] 现状原样透传上游
//! （ws_terminal 不解析 query，到不了 ttyd 本体）。

use percent_encoding::percent_decode_str;

/// 终端 query 参数（仅记录本契约关心的两个键；None = 未出现）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalQueryParams {
    pub service_type: Option<String>,
    pub cwd: Option<String>,
}

/// 解析终端路由 query 串中的 `service_type` / `cwd`。
///
/// 非法 percent 序列或解码后非 UTF-8 返回 `Err`（调用方回 400——显式给了
/// 参数但不可读，fail fast 不静默忽略）。
pub fn parse_terminal_query(query: Option<&str>) -> Result<TerminalQueryParams, String> {
    let mut params = TerminalQueryParams::default();
    let Some(query) = query else {
        return Ok(params);
    };
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let decoded = form_decode(value)?;
        match key {
            "service_type" => params.service_type = Some(decoded),
            "cwd" => params.cwd = Some(decoded),
            _ => {}
        }
    }
    Ok(params)
}

/// form 语义解码：`+` → 空格，`%XX` → 字节；`%2B` 还原字面 `+`。
fn form_decode(value: &str) -> Result<String, String> {
    let plus_to_space = value.replace('+', " ");
    percent_decode_str(&plus_to_space)
        .decode_utf8()
        .map(|cow| cow.into_owned())
        .map_err(|_| format!("query parameter is not valid UTF-8: {value}"))
}

/// computer 路由的 service_type 合并规则。
///
/// | header | query | 结果 |
/// |---|---|---|
/// | 无 | 无 | 默认 `ComputerAgentRunner` |
/// | 合法 | 无 | header 值（现状语义） |
/// | 非法 | 无 | 默认（header 维持既有宽容：中间层兜底） |
/// | 任意 | 合法 Computer 族 | query 值 |
/// | 任意 | 不可解析 / 非 Computer 族 | `Err`（显式错误输入 fail fast） |
/// | 合法 | 合法且相等 | 该值 |
/// | 合法 | 合法但不等 | `Err`（两种意图矛盾，不静默择一） |
pub fn resolve_computer_service_type(
    query: Option<&str>,
    header: Option<&str>,
) -> Result<shared_types::ServiceType, String> {
    use shared_types::ServiceType;
    use std::str::FromStr;

    // header 维持既有宽容语义（非法/非 Computer 族回落默认），与
    // handle_ttyd_request 原实现一致——仅新增的 query 输入从紧。
    let header_st = header
        .and_then(|text| text.parse::<ServiceType>().ok())
        .filter(|st| st.is_computer_family());

    let query_st = match query {
        Some(raw) => {
            let st = ServiceType::from_str(raw)
                .map_err(|_| format!("invalid service_type query parameter: {raw}"))?;
            if !st.is_computer_family() {
                return Err(format!(
                    "service_type query parameter is not supported on this route: {raw}"
                ));
            }
            Some(st)
        }
        None => None,
    };

    Ok(match (header_st, query_st) {
        (None, None) => ServiceType::ComputerAgentRunner,
        (Some(header_value), None) => header_value,
        (None, Some(query_value)) => query_value,
        (Some(header_value), Some(query_value)) if header_value == query_value => query_value,
        (Some(header_value), Some(query_value)) => {
            return Err(format!(
                "conflicting service_type: header={header_value}, query={query_value}"
            ));
        }
    })
}

/// 校验并归一化终端 cwd（多平台绝对路径规则，与 chat 链 `agent_work_dir`
/// 同一词汇表）；未出现返回 `None`。
pub fn resolve_terminal_cwd(query: Option<&str>) -> Result<Option<String>, String> {
    match query {
        None => Ok(None),
        Some(raw) => shared_types::normalize_absolute_dir(raw, "cwd").map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_params_with_form_semantics() {
        let params = parse_terminal_query(Some(
            "service_type=computer-normal-project&cwd=%2Fhome%2Fuser%2Fmy+dir&x=1",
        ))
        .expect("parse");
        assert_eq!(
            params,
            TerminalQueryParams {
                service_type: Some("computer-normal-project".into()),
                cwd: Some("/home/user/my dir".into()),
            }
        );
    }

    #[test]
    fn missing_query_yields_empty_params() {
        assert_eq!(
            parse_terminal_query(None).expect("parse"),
            Default::default()
        );
        assert_eq!(
            parse_terminal_query(Some("")).expect("parse"),
            Default::default()
        );
        assert_eq!(
            parse_terminal_query(Some("other=1")).expect("parse"),
            Default::default()
        );
    }

    #[test]
    fn duplicate_keys_last_wins() {
        let params = parse_terminal_query(Some("cwd=%2Fa&cwd=%2Fb")).expect("parse");
        assert_eq!(params.cwd.as_deref(), Some("/b"));
    }

    #[test]
    fn invalid_utf8_rejected() {
        let err = parse_terminal_query(Some("cwd=%FF%FE")).expect_err("invalid utf8");
        assert!(err.contains("UTF-8"), "{err}");
    }

    #[test]
    fn service_type_matrix() {
        use shared_types::ServiceType;
        fn st(s: &str) -> Option<&str> {
            Some(s)
        }
        // 无输入 → 默认
        assert_eq!(
            resolve_computer_service_type(None, None).expect("ok"),
            ServiceType::ComputerAgentRunner
        );
        // 仅 header（合法/非法）
        assert_eq!(
            resolve_computer_service_type(None, st("computer-normal-project")).expect("ok"),
            ServiceType::ComputerNormalProject
        );
        assert_eq!(
            resolve_computer_service_type(None, st("garbage")).expect("ok"),
            ServiceType::ComputerAgentRunner
        );
        // 仅 query 合法
        assert_eq!(
            resolve_computer_service_type(st("computer-normal-project"), None).expect("ok"),
            ServiceType::ComputerNormalProject
        );
        // query 非法 / 非 Computer 族 → Err
        assert!(resolve_computer_service_type(st("garbage"), None).is_err());
        assert!(resolve_computer_service_type(st("web-agent-runner"), None).is_err());
        // header + query 相等（变体词表归一同枚举）
        assert_eq!(
            resolve_computer_service_type(
                st("computer-normal-project"),
                st("ComputerNormalProject")
            )
            .expect("ok"),
            ServiceType::ComputerNormalProject
        );
        // header + query 冲突
        assert!(
            resolve_computer_service_type(
                st("computer-normal-project"),
                st("computer-agent-runner")
            )
            .is_err()
        );
        // header 非法 + query 合法 → query 生效
        assert_eq!(
            resolve_computer_service_type(st("computer-normal-project"), st("garbage"))
                .expect("ok"),
            ServiceType::ComputerNormalProject
        );
    }

    #[test]
    fn cwd_resolution_rules() {
        // 未出现 → None
        assert_eq!(resolve_terminal_cwd(None).expect("ok"), None);
        // 归一：反斜杠 → 正斜杠、折叠段间重复分隔符
        // （前导 // 是既有归一器的 UNC 保留语义，尾 / 同样保留——按其输出为契约）
        assert_eq!(
            resolve_terminal_cwd(Some("/home//user/x"))
                .expect("ok")
                .as_deref(),
            Some("/home/user/x")
        );
        assert_eq!(
            resolve_terminal_cwd(Some("\\home\\user\\x"))
                .expect("ok")
                .as_deref(),
            Some("/home/user/x")
        );
        // 空格/中文保留（传输由 agent_runner 编码承担）
        assert_eq!(
            resolve_terminal_cwd(Some("/home/user/我的 dir"))
                .expect("ok")
                .as_deref(),
            Some("/home/user/我的 dir")
        );
        // 相对路径 / 点段 / 空值 → Err
        assert!(resolve_terminal_cwd(Some("home/user")).is_err());
        assert!(resolve_terminal_cwd(Some("/home/../etc")).is_err());
        assert!(resolve_terminal_cwd(Some("/home/./x")).is_err());
        assert!(resolve_terminal_cwd(Some("")).is_err());
    }
}
