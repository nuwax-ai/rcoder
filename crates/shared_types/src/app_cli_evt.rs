//! `APP-CLI-EVT` 跨进程事件契约（UA-07 唯一事实源）。
//!
//! 行协议：`APP-CLI-EVT {json}`（一行一条，serde tag="event" snake_case）。
//! 三方共用：生产者（app-cli 内置编排 `orchestration_events`）、转发管道
//! （file-server stdout 行首识别）、消费者（平台侧 `map_app_cli_evt` typed
//! 解码）与 owner 运行事件适配（`RuntimeEventRecord` → 事件行）。
//! wire 与平台侧 `BuildProgressEvent` 的 `service_*` 变体同构；两端各自
//! 测试锁同一组字符串。

use serde::{Deserialize, Serialize};

/// EVT 行前缀（file-server 管道按行首匹配识别）。
pub const APP_CLI_EVT_PREFIX: &str = "APP-CLI-EVT ";

/// 启动失败服务条目（orchestration_done 汇总）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppCliFailedService {
    pub service: String,
    pub error: String,
}

/// app-cli 内置启动编排事件（wire tag 与 BuildProgressEvent 的 service_*
/// 变体一致）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AppCliOrchestrationEvent {
    /// 开始启动某服务（spawn 前）。
    ServiceStarting { service: String },
    /// 某服务启动成功（readiness 探测通过）。
    ServiceStartOk { service: String },
    /// 某服务启动失败（spawn io 错误 / migrate 失败 / 探测超时）——不阻塞其余服务。
    ServiceStartFail { service: String, error: String },
    /// 启动编排终局：`failed` 为失败清单（空 = 全部成功）。
    OrchestrationDone { failed: Vec<AppCliFailedService> },
}

/// 事件行解码结果的三类失败——未知扩展与已知事件损坏分开
/// （spec §3.3：消费方对未知扩展与已知损坏的处置可以不同）。
#[derive(Debug, PartialEq, Eq)]
pub enum AppCliEvtDecodeError {
    /// 行不是合法 JSON。
    InvalidJson(String),
    /// 合法 JSON 但无 `event` 字符串 tag。
    MissingEventTag,
    /// 契约外的新事件名（前向扩展——旧消费端按能力忽略, 不等于损坏）。
    UnknownEvent { event: String },
    /// 契约内事件名但字段形态损坏（缺字段/类型错误）。
    MalformedKnownEvent { event: String, error: String },
}

impl AppCliOrchestrationEvent {
    /// typed 解码（入口为去掉前缀后的 JSON 行）。
    pub fn decode(json: &str) -> Result<Self, AppCliEvtDecodeError> {
        let peek: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| AppCliEvtDecodeError::InvalidJson(error.to_string()))?;
        let tag = peek
            .get("event")
            .and_then(serde_json::Value::as_str)
            .ok_or(AppCliEvtDecodeError::MissingEventTag)?
            .to_string();
        match tag.as_str() {
            "service_starting" | "service_start_ok" | "service_start_fail"
            | "orchestration_done" => serde_json::from_str(json).map_err(|error| {
                AppCliEvtDecodeError::MalformedKnownEvent {
                    event: tag,
                    error: error.to_string(),
                }
            }),
            other => Err(AppCliEvtDecodeError::UnknownEvent {
                event: other.to_string(),
            }),
        }
    }

    /// 序列化为单行 JSON（生产者 emit 与 owner 适配共用; 不含前缀）。
    pub fn encode(&self) -> String {
        // Self 序列化由 serde 保证为紧凑单行（无换行注入字段——字段为 String,
        // serde_json 转义换行）。
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 字符串锁定：两端（app-cli 生产 / 平台消费）各自测试锁同一组字符串。
    #[test]
    fn wire_shapes_are_locked() {
        assert_eq!(
            AppCliOrchestrationEvent::ServiceStarting {
                service: "frontend".into()
            }
            .encode(),
            r#"{"event":"service_starting","service":"frontend"}"#
        );
        assert_eq!(
            AppCliOrchestrationEvent::ServiceStartFail {
                service: "api".into(),
                error: "boom".into(),
            }
            .encode(),
            r#"{"event":"service_start_fail","service":"api","error":"boom"}"#
        );
        assert_eq!(
            AppCliOrchestrationEvent::OrchestrationDone {
                failed: vec![AppCliFailedService {
                    service: "api".into(),
                    error: "boom".into()
                }]
            }
            .encode(),
            r#"{"event":"orchestration_done","failed":[{"service":"api","error":"boom"}]}"#
        );
    }

    /// round-trip + 三类失败区分。
    #[test]
    fn decode_round_trip_and_error_taxonomy() {
        let event = AppCliOrchestrationEvent::OrchestrationDone { failed: Vec::new() };
        assert_eq!(AppCliOrchestrationEvent::decode(&event.encode()), Ok(event));

        assert!(matches!(
            AppCliOrchestrationEvent::decode("{not json"),
            Err(AppCliEvtDecodeError::InvalidJson(_))
        ));
        assert_eq!(
            AppCliOrchestrationEvent::decode(r#"{"payload":1}"#),
            Err(AppCliEvtDecodeError::MissingEventTag)
        );
        assert_eq!(
            AppCliOrchestrationEvent::decode(r#"{"event":"future_extension","x":1}"#),
            Err(AppCliEvtDecodeError::UnknownEvent {
                event: "future_extension".into()
            })
        );
        assert!(matches!(
            AppCliOrchestrationEvent::decode(r#"{"event":"service_start_ok"}"#),
            Err(AppCliEvtDecodeError::MalformedKnownEvent { event, .. }) if event == "service_start_ok"
        ));
        assert!(matches!(
            AppCliOrchestrationEvent::decode(r#"{"event":"service_start_fail","service":"x"}"#),
            Err(AppCliEvtDecodeError::MalformedKnownEvent { event, .. }) if event == "service_start_fail"
        ));
    }
}
