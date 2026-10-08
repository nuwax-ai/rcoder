//! run 客户端的 journal → stdout 事件桥。
//!
//! spawn 路径（file-server 管道）的 stdout 是编排事件的唯一通道
//! （`orchestration_events` 模块契约）；0.3.16 起编排跑在常驻 serve 内、
//! 事件经 R06 桥进 journal——客户端把 journal 事件重新呈现在自己的
//! stdout 上，终局信号兜底合成，管道契约保持完整。

/// journal 事件 → EVT 行转发（游标推进）。返回是否转发了 orchestration_done
///（serve 的权威终局信号；调用方据此决定是否需要兜底合成）。
pub(super) async fn forward_operation_events(
    client: &reqwest::Client,
    admin_addr: &str,
    token: &str,
    operation_id: &str,
    after_seq: &mut u64,
) -> bool {
    let mut done_forwarded = false;
    let url = format!(
        "http://{admin_addr}/v1/runtime/operations/{operation_id}/events?after_seq={after_seq}"
    );
    let Ok(response) = client
        .get(&url)
        .header("X-Deploy-Token", token)
        .send()
        .await
    else {
        return done_forwarded;
    };
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return done_forwarded;
    };
    let Some(events) = body
        .get("data")
        .and_then(|data| data.get("events"))
        .and_then(|events| events.as_array())
    else {
        return done_forwarded;
    };
    for event in events {
        if let Some(sequence) = event.get("sequence").and_then(|s| s.as_u64()) {
            *after_seq = (*after_seq).max(sequence);
        }
        let Some(name) = event.get("event_name").and_then(|n| n.as_str()) else {
            continue;
        };
        if name == "orchestration_done" {
            done_forwarded = true;
        }
        // 重建 EVT wire：{"event": name, "service"?: .., **payload}——与
        // R06 桥的入库映射（bridge_event_fields）互逆。
        let mut line = serde_json::Map::new();
        line.insert("event".to_string(), serde_json::json!(name));
        if let Some(service) = event.get("service").filter(|s| !s.is_null()) {
            line.insert("service".to_string(), service.clone());
        }
        if let Some(payload) = event.get("payload").and_then(|p| p.as_object()) {
            for (key, value) in payload {
                line.entry(key.clone()).or_insert(value.clone());
            }
        }
        let Ok(json) = serde_json::to_string(&serde_json::Value::Object(line)) else {
            continue;
        };
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}{}", shared_types::APP_CLI_EVT_PREFIX, json);
        let _ = out.flush();
    }
    done_forwarded
}

/// 终态 → EVT orchestration_done 兜底行（仅当 journal 终局前未转发到任何
/// done 时调用；失败清单取自操作视图，错误面向操作者、无凭据）。
pub(super) fn emit_terminal_done_event(view: &shared_types::RuntimeOperationView) {
    use shared_types::RuntimeOperationState;
    let failed = match view.state {
        RuntimeOperationState::Succeeded => Vec::new(),
        state => vec![shared_types::AppCliFailedService {
            service: "orchestrator".to_string(),
            error: format!(
                "[{state:?}] {}",
                view.error_message
                    .as_deref()
                    .unwrap_or("operation ended without success")
            ),
        }],
    };
    crate::orchestration_events::emit(&shared_types::AppCliOrchestrationEvent::OrchestrationDone {
        failed,
    });
}
