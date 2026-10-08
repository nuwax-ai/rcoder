//! run 客户端的 journal → stdout 事件桥。
//!
//! spawn 路径（file-server 管道）的 stdout 是编排事件的唯一通道
//! （`orchestration_events` 模块契约）；0.3.16 起编排跑在常驻 serve 内、
//! 事件经 R06 桥进 journal——客户端把 journal 事件重新呈现在自己的
//! stdout 上。终局信号绑定**原操作的权威终态**（观察循环在视图终态时
//! 恰好一次地发出 Done）；journal 的 `orchestration_done` 只是编排循环
//! 的完成信号，早于操作状态机提交终局（R1 反例：转发空失败清单时原
//! 操作仍为 Accepted，随后被 Stop 取消）——它被捕获而非立即转发。

/// journal 事件 → EVT 行转发（游标推进）。`orchestration_done` 不立即
/// 发出：捕获其 wire 形态返回，由调用方在原操作终态确认后经
/// [`captured_done_matches`] 校验再发出（保留原服务明细）。返回本轮
/// 捕获到的 done wire 对象（跨轮持留时以最近一轮为准）。
pub(super) async fn forward_operation_events(
    client: &reqwest::Client,
    admin_addr: &str,
    token: &str,
    operation_id: &str,
    after_seq: &mut u64,
) -> Option<serde_json::Value> {
    let url = format!(
        "http://{admin_addr}/v1/runtime/operations/{operation_id}/events?after_seq={after_seq}"
    );
    let Ok(response) = client
        .get(&url)
        .header("X-Deploy-Token", token)
        .send()
        .await
    else {
        return None;
    };
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return None;
    };
    let events = body
        .get("data")
        .and_then(|data| data.get("events"))
        .and_then(|events| events.as_array())?;
    let (lines, done) = rebuild_events(events, after_seq);
    if !lines.is_empty() {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        for line in lines {
            let _ = writeln!(out, "{}{}", shared_types::APP_CLI_EVT_PREFIX, line);
        }
        let _ = out.flush();
    }
    done
}

/// 纯转换：journal 事件数组 →（要立即发出的 EVT 行, 捕获的 done wire）。
/// wire 重建与 R06 桥的入库映射（bridge_event_fields）互逆：
/// `{"event": name, "service"?: .., **payload}`。
fn rebuild_events(
    events: &[serde_json::Value],
    after_seq: &mut u64,
) -> (Vec<String>, Option<serde_json::Value>) {
    let mut lines = Vec::new();
    let mut done = None;
    for event in events {
        if let Some(sequence) = event.get("sequence").and_then(|s| s.as_u64()) {
            *after_seq = (*after_seq).max(sequence);
        }
        let Some(name) = event.get("event_name").and_then(|n| n.as_str()) else {
            continue;
        };
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
        let value = serde_json::Value::Object(line);
        if name == "orchestration_done" {
            // 终局由权威视图决定（见模块注释）；仅捕获，不发出。
            done = Some(value);
            continue;
        }
        let Ok(json) = serde_json::to_string(&value) else {
            continue;
        };
        lines.push(json);
    }
    (lines, done)
}

/// 捕获的 journal done 与权威终态的一致性校验：空失败清单 ⇔ Succeeded。
/// 不一致（如取消前一轮的空清单 done、或成功操作旁路的失败清单）一律
/// 弃用，改按视图合成——对外终局永不与权威结果矛盾。
pub(super) fn captured_done_matches(
    done: &serde_json::Value,
    state: &shared_types::RuntimeOperationState,
) -> bool {
    use shared_types::RuntimeOperationState;
    let empty_failed = done
        .get("failed")
        .and_then(|failed| failed.as_array())
        .is_none_or(|failed| failed.is_empty());
    match state {
        RuntimeOperationState::Succeeded => empty_failed,
        _ => !empty_failed,
    }
}

/// 发出经权威校验的捕获 done（保留 journal 的原服务失败明细）。
pub(super) fn emit_captured_done(done: &serde_json::Value) {
    use std::io::Write as _;
    let Ok(json) = serde_json::to_string(done) else {
        return;
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}{}", shared_types::APP_CLI_EVT_PREFIX, json);
    let _ = out.flush();
}

/// 终态 → EVT orchestration_done 兜底行（终局时无一致捕获件则按视图
/// 合成；失败清单取自操作视图，错误面向操作者、无凭据）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::RuntimeOperationState;

    fn journal_event(sequence: u64, name: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "sequence": sequence,
            "event_name": name,
            "payload": payload,
        })
    }

    /// R1 反例的失败先行断言：journal 在原操作非终态时给出的
    /// orchestration_done 不得出现在发出的行里——只捕获。
    #[test]
    fn done_is_captured_not_emitted() {
        let events = vec![
            journal_event(1, "service_starting", serde_json::json!({"service": "web"})),
            journal_event(2, "orchestration_done", serde_json::json!({"failed": []})),
            journal_event(3, "service_start_ok", serde_json::json!({"service": "web"})),
        ];
        let mut cursor = 0u64;
        let (lines, done) = rebuild_events(&events, &mut cursor);
        assert_eq!(
            lines,
            vec![
                "{\"event\":\"service_starting\",\"service\":\"web\"}".to_string(),
                "{\"event\":\"service_start_ok\",\"service\":\"web\"}".to_string(),
            ],
            "progress 行照发（含 done 之后游标推进的事件）"
        );
        let done = done.expect("done 必须被捕获");
        assert_eq!(done["event"], "orchestration_done");
        assert_eq!(done["failed"], serde_json::json!([]));
        assert_eq!(cursor, 3, "游标推进越过 done（下轮不重取）");
    }

    /// 终局一致性：空失败清单 ⇔ Succeeded——其余组合一律弃用捕获件。
    #[test]
    fn captured_done_matches_authoritative_state() {
        let empty = serde_json::json!({"event": "orchestration_done", "failed": []});
        let failed = serde_json::json!({"event": "orchestration_done", "failed": [
            {"service": "web", "error": "boom"}
        ]});
        assert!(captured_done_matches(
            &empty,
            &RuntimeOperationState::Succeeded
        ));
        assert!(!captured_done_matches(
            &failed,
            &RuntimeOperationState::Succeeded
        ));
        assert!(!captured_done_matches(
            &empty,
            &RuntimeOperationState::Failed
        ));
        assert!(captured_done_matches(
            &failed,
            &RuntimeOperationState::Failed
        ));
        assert!(!captured_done_matches(
            &empty,
            &RuntimeOperationState::Cancelled
        ));
    }

    /// payload 字段平铺进 wire 行（R06 桥互逆契约保持）。
    #[test]
    fn payload_fields_flatten_into_wire() {
        let events = vec![journal_event(
            7,
            "service_start_fail",
            serde_json::json!({"service": "api", "error": "exit 3"}),
        )];
        let mut cursor = 0u64;
        let (lines, done) = rebuild_events(&events, &mut cursor);
        assert_eq!(cursor, 7);
        assert!(done.is_none());
        let line: serde_json::Value =
            serde_json::from_str(&lines[0]).expect("emitted line is valid json");
        assert_eq!(line["event"], "service_start_fail");
        assert_eq!(line["service"], "api");
        assert_eq!(line["error"], "exit 3");
    }
}
