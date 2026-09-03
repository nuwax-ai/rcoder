//! 编排事件（EVT）——builtin 引擎启动过程的机器可读事件行（stdout）。
//!
//! 行协议：`APP-CLI-EVT {json}`（一行一条，serde tag="event" snake_case）——
//! wire 与平台侧 `BuildProgressEvent` 的 `service_*` 变体同构（跨进程契约，
//! 两端各自测试锁同一组字符串）。dev 链路 file-server 的 stdout 管道识别
//! 此前缀转发任务 SSE（`/tasks/{id}/logs/stream`）。
//!
//! stdout 无 tracing 噪声（日志只配 stderr + 文件层），EVT 行不混杂；
//! 生产 supervisord 引擎不输出（无 stdout 消费者，启动判定语义另有约定）。

use serde::Serialize;

/// EVT 行前缀（file-server 管道按行首匹配识别）。
pub const EVT_PREFIX: &str = "APP-CLI-EVT ";

/// 启动失败服务条目（orchestration_done 汇总）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FailedService {
    pub service: String,
    pub error: String,
}

/// builtin 启动编排事件（wire tag 与 BuildProgressEvent 的 service_* 变体一致）。
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum OrchestrationEvent {
    /// 开始启动某服务（spawn 前）。
    ServiceStarting { service: String },
    /// 某服务启动成功（readiness 探测通过）。
    ServiceStartOk { service: String },
    /// 某服务启动失败（spawn io 错误 / migrate 失败 / 探测超时）——不阻塞其余服务。
    ServiceStartFail { service: String, error: String },
    /// 启动编排终局（pingap 就绪确认后）：`failed` 为失败清单（空 = 全部成功）。
    OrchestrationDone { failed: Vec<FailedService> },
}

/// 输出一条 EVT 行到 stdout（单行 + 立即 flush 保证行完整性；写失败静默——
/// 事件是尽力而为的观测通道，不得影响编排主流程）。
pub fn emit(event: &OrchestrationEvent) {
    use std::io::Write;
    let Ok(json) = serde_json::to_string(event) else {
        return;
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{EVT_PREFIX}{json}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 字符串契约：与 shared_types::BuildProgressEvent 的 service_* 变体
    /// 测试锁同一组字符串（跨进程 stdout 行协议，两端无类型共享）。
    #[test]
    fn wire_tags_match_platform_contract() {
        let starting = serde_json::to_string(&OrchestrationEvent::ServiceStarting {
            service: "frontend".into(),
        })
        .unwrap();
        assert_eq!(
            starting, r#"{"event":"service_starting","service":"frontend"}"#,
            "wire must match BuildProgressEvent::ServiceStarting exactly"
        );

        let ok = serde_json::to_string(&OrchestrationEvent::ServiceStartOk {
            service: "backend-go".into(),
        })
        .unwrap();
        assert_eq!(
            ok, r#"{"event":"service_start_ok","service":"backend-go"}"#,
            "wire must match BuildProgressEvent::ServiceStartOk exactly"
        );

        let fail = serde_json::to_string(&OrchestrationEvent::ServiceStartFail {
            service: "backend-java".into(),
            error: "probe timeout".into(),
        })
        .unwrap();
        assert_eq!(
            fail,
            r#"{"event":"service_start_fail","service":"backend-java","error":"probe timeout"}"#,
            "wire must match BuildProgressEvent::ServiceStartFail exactly"
        );

        let done = serde_json::to_string(&OrchestrationEvent::OrchestrationDone {
            failed: vec![FailedService {
                service: "s".into(),
                error: "e".into(),
            }],
        })
        .unwrap();
        assert_eq!(
            done,
            r#"{"event":"orchestration_done","failed":[{"service":"s","error":"e"}]}"#
        );
    }
}
