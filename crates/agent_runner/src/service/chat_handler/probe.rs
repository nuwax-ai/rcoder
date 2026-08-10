//! 阶段0: 模型探活
//!
//! 仅新建会话(session_id 为空)且有模型配置时,用 `model_probe` 发一次
//! `max_tokens:1` 轻量探活。策略 fail-open:只拦 5xx + 连接/DNS/TLS 失败。

use shared_types::error_codes;
use tracing::{debug, warn};

use model_probe::ProbeResult;

use super::types::{ChatHandlerInput, ChatHandlerOutput};

/// 模型探活：仅新建会话(session_id 为空)且有模型配置时触发。
///
/// 返回 `Some(output)` = 应拦截返回该错误；`None` = 继续(fail-open 或可用)。
pub(super) async fn run_model_probe(
    input: &ChatHandlerInput,
    project_id: &str,
    session_id: &Option<String>,
) -> Option<ChatHandlerOutput> {
    // resume(session_id 非空)不探活 —— agent 已启动说明模型之前可用
    if session_id.is_some() {
        return None;
    }
    let provider = input.model_config.as_ref()?;

    match model_probe::check_model_available(provider).await {
        ProbeResult::Unavailable(reason) => {
            warn!(
                "[ChatHandler] [MODEL_PROBE] blocked agent start: endpoint={}, model={}, reason={}",
                shared_types::mask_url(&provider.base_url),
                provider.default_model,
                reason
            );
            Some(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.clone().unwrap_or_default(),
                error_codes::get_i18n_message_default("error.model_unavailable"),
                error_codes::ERR_MODEL_UNAVAILABLE.to_string(),
            ))
        }
        ProbeResult::Inconclusive => {
            warn!(
                "[ChatHandler] [MODEL_PROBE] inconclusive (fail-open): endpoint={}, model={}",
                shared_types::mask_url(&provider.base_url),
                provider.default_model
            );
            None
        }
        ProbeResult::Available => {
            debug!(
                "[ChatHandler] [MODEL_PROBE] model available, proceeding: model={}",
                provider.default_model
            );
            None
        }
    }
}
