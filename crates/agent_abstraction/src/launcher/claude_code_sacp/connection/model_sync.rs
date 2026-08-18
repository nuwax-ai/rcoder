//! resume 后的 session 模型引用同步（ACP v1 `session/set_config_option`）
//!
//! 从 `setup.rs` 拆出：模型同步是独立的协议加固职责，与连接初始化/会话创建
//! 生命周期正交。值形态从 agent 自己声明的 configOptions 匹配，
//! rcoder 不持有任何 agent 内部命名约定。

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionValue, SessionConfigSelectOption,
    SessionId, SetSessionConfigOptionRequest,
};
use agent_client_protocol::{Agent, ConnectionTo};
use tracing::{debug, info, warn};

/// set_config_option(model) 超时时间（秒）——同步是加固步骤，超时降级继续
pub(super) const SET_CONFIG_OPTION_TIMEOUT_SECS: u64 = 10;

/// 🆕 resume 后同步 session 模型引用（ACP v1 标准方法 `session/set_config_option`）
///
/// 触发条件：resume 的 LoadSession 响应带 config_options（agent 声明了会话配置项）
/// 且本次请求有模型配置（model_provider）。新会话的引用由 agent 按当前 env 建立，
/// 无需同步；仅 resume 的历史 session 可能持久化了旧模型引用（切模型场景：
/// agent_runner 停旧进程 → 新进程只注册新模型 → 旧引用解析不到 →
/// ProviderModelNotFoundError）。显式对齐引用正是"切换模型"的协议语义。
///
/// 值形态从 agent 自己声明的可用模型列表（configOptions.options）匹配——
/// 裸模型名（acp-ts 系）或 `<provider>/<model>`（opencode 系，如
/// `openai-compatible/deepseek-v4-pro`），rcoder 不持有任何 agent 内部命名约定。
///
/// 容错：任何失败（agent 未实现 Method not found / 拒绝参数 Invalid params /
/// 超时）仅 warn 后继续——模型主通道是 spawn env 注入，本调用是引用对齐加固，
/// 不引入新故障面。
pub(super) async fn sync_session_model_config(
    cx: &ConnectionTo<Agent>,
    model_id: &Option<String>,
    config_options: &Option<Vec<SessionConfigOption>>,
    session_id: &SessionId,
) {
    let Some(model) = model_id else {
        return;
    };
    let Some(options) = config_options else {
        // NewSession 路径（含 LoadSession 降级）或 agent 未声明配置项：跳过
        return;
    };
    // 匹配不到时退化为裸模型名（agent 拒绝则按容错路径降级）
    let value = select_model_config_value(options, model).unwrap_or_else(|| model.clone());
    // SessionConfigValueId 包装 Arc<str>（仅接受 'static 借用），传 owned String
    let request = SetSessionConfigOptionRequest::new(
        session_id.clone(),
        "model",
        SessionConfigOptionValue::value_id(value.clone()),
    );
    debug!("[SACP] set_config_option request: {:?}", request);
    match tokio::time::timeout(
        std::time::Duration::from_secs(SET_CONFIG_OPTION_TIMEOUT_SECS),
        cx.send_request(request).block_task(),
    )
    .await
    {
        Ok(Ok(_)) => info!(
            "[SACP] Session model synced via set_config_option: session_id={}, model={}",
            session_id, value
        ),
        Ok(Err(e)) => warn!(
            "[SACP] set_config_option(model) rejected, continuing with env-injected model: session_id={}, model={}, error={}",
            session_id, value, e
        ),
        Err(_) => warn!(
            "[SACP] set_config_option(model) timeout ({}s), continuing with env-injected model: session_id={}, model={}",
            SET_CONFIG_OPTION_TIMEOUT_SECS, session_id, value
        ),
    }
}

/// 在 agent 声明的 model 选项列表中匹配目标模型，返回该 agent 命名空间下的完整值。
///
/// 匹配规则：先精确（裸模型名，acp-ts 系），再按 `<provider>/<model>` 尾匹配
/// （opencode 系）。支持平铺（Ungrouped）与分组（Grouped）两种选项形态。
fn select_model_config_value(options: &[SessionConfigOption], model: &str) -> Option<String> {
    use agent_client_protocol::schema::v1::SessionConfigSelectOptions;

    let select = options.iter().find_map(|opt| match &opt.kind {
        SessionConfigKind::Select(sel) if &*opt.id.0 == "model" => Some(sel),
        _ => None,
    })?;
    let candidates: Vec<&SessionConfigSelectOption> = match &select.options {
        SessionConfigSelectOptions::Ungrouped(list) => list.iter().collect(),
        SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|g| g.options.iter()).collect()
        }
        // #[non_exhaustive]：未知形态视为无可匹配项
        _ => Vec::new(),
    };
    let suffix = format!("/{model}");
    candidates
        .iter()
        .find(|o| &*o.value.0 == model)
        .or_else(|| candidates.iter().find(|o| (*o.value.0).ends_with(&suffix)))
        .map(|o| o.value.to_string())
}

#[cfg(test)]
mod set_config_option_wire_tests {
    use super::*;

    /// 锁定 wire 格式：method 名、configId、value 的 camelCase 序列化——
    /// 这是与各 ACP agent 实现互操作的基础，防止重构时破坏协议形态
    #[test]
    fn serializes_model_config_option_request() {
        let request = SetSessionConfigOptionRequest::new(
            SessionId::from("ses_test123"),
            "model",
            SessionConfigOptionValue::value_id("openai-compatible/deepseek-v4-pro"),
        );
        let json = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(json["sessionId"], "ses_test123");
        assert_eq!(json["configId"], "model");
        // type 缺省按 select 值 ID 反序列化（v1 wire 约定），序列化为裸 value；
        // method 名由 SDK 宏静态绑定（"session/set_config_option"），编译期已定
        assert_eq!(json["value"], "openai-compatible/deepseek-v4-pro");
    }
}

#[cfg(test)]
mod select_model_config_value_tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        SessionConfigSelect, SessionConfigSelectGroup, SessionConfigSelectOption,
        SessionConfigSelectOptions,
    };

    fn model_option(options: SessionConfigSelectOptions) -> SessionConfigOption {
        SessionConfigOption::new(
            "model",
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new("current", options)),
        )
    }

    fn opt(value: &str) -> SessionConfigSelectOption {
        SessionConfigSelectOption::new(value.to_string(), value.to_string())
    }

    #[test]
    fn exact_bare_match() {
        // acp-ts 系：选项即裸模型名
        let options = vec![model_option(SessionConfigSelectOptions::Ungrouped(vec![
            opt("deepseek-v4-pro"),
            opt("deepseek-v4-flash"),
        ]))];
        assert_eq!(
            select_model_config_value(&options, "deepseek-v4-flash"),
            Some("deepseek-v4-flash".to_string())
        );
    }

    #[test]
    fn suffix_match_with_provider_prefix() {
        // opencode 系：选项为 <provider>/<model>，请求模型为裸名
        let options = vec![model_option(SessionConfigSelectOptions::Ungrouped(vec![
            opt("openai/gpt-4"),
            opt("openai-compatible/deepseek-v4-pro"),
        ]))];
        assert_eq!(
            select_model_config_value(&options, "deepseek-v4-pro"),
            Some("openai-compatible/deepseek-v4-pro".to_string())
        );
    }

    #[test]
    fn exact_match_takes_priority_over_suffix() {
        let options = vec![model_option(SessionConfigSelectOptions::Ungrouped(vec![
            opt("qwen"),
            opt("provider/qwen"),
        ]))];
        assert_eq!(
            select_model_config_value(&options, "qwen"),
            Some("qwen".to_string())
        );
    }

    #[test]
    fn searches_grouped_options() {
        let options = vec![model_option(SessionConfigSelectOptions::Grouped(vec![
            SessionConfigSelectGroup::new(
                "openai",
                "OpenAI",
                vec![opt("openai-compatible/deepseek-v4-pro")],
            ),
        ]))];
        assert_eq!(
            select_model_config_value(&options, "deepseek-v4-pro"),
            Some("openai-compatible/deepseek-v4-pro".to_string())
        );
    }

    #[test]
    fn no_match_returns_none() {
        let options = vec![model_option(SessionConfigSelectOptions::Ungrouped(vec![
            opt("openai/gpt-4"),
        ]))];
        assert_eq!(select_model_config_value(&options, "qwen"), None);
    }

    #[test]
    fn ignores_non_model_options() {
        let thinking = SessionConfigOption::new(
            "thinking",
            "Thinking",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "off",
                SessionConfigSelectOptions::Ungrouped(vec![opt("qwen")]),
            )),
        );
        assert_eq!(select_model_config_value(&[thinking], "qwen"), None);
    }
}
