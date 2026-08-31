//! Userapp build 进度事件 —— file-server(发送)与 rcoder(接收)共享的类型化 DTO。
//!
//! 历史:file-server 定义、rcoder 以字符串键(`event`/`release_id`/`error`)重复解析,
//! 字段重命名会静默断链。统一到此模块,两端共用同一类型,消除漂移。
//!
//! wire casing 统一 snake_case(tag 值 + 字段)——userApp Java 契约全 snake 定案,
//! 且 tag 值与 SSE `event:` 名(build_ok/build_fail)一致,消费端只记一套事件名。
//! variant 字段本就是 snake_case Rust 命名,无需逐 variant 附加属性。

use serde::{Deserialize, Serialize};

/// build 进度事件(file-server 经 SSE 发送,rcoder 接收)。`tag = "event"` 内部标签枚举。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BuildProgressEvent {
    /// 进入新阶段
    Stage { stage: String },
    /// 开始编译某服务（`service` 为 service_id 稳定身份，非 `[project].name` 显示名）
    Building { service: String },
    /// 某服务编译成功（`service` 为 service_id）
    BuildOk { service: String },
    /// 某服务编译失败（`service` 为 service_id；`error` 含服务名前缀与输出尾部）
    BuildFail { service: String, error: String },
    /// 一行日志(实时 tail；`service` 为 service_id)
    Log { service: String, line: String },
    /// 任务完成(build 产 release_id + 包摘要)。`artifact_path` 为相对 workspace
    /// 根的产物路径（`builds/workspace-package-{release_id}.zip`）——信息字段，
    /// 取包按 app 直下 `/api/v1/userapp/static/{app_id}`（服务端选最新产物）。
    Completed {
        release_id: String,
        sha256: String,
        size_bytes: u64,
        file_name: String,
        artifact_path: String,
    },
    /// 任务失败
    Failed { error: String },
    /// 任务被取消
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 全 snake_case:tag 值(build_ok/build_fail)+ 字段(release_id/size_bytes/file_name/artifact_path)。
    /// tag 与 SSE `event:` 名一致——消费端只记一套事件名。
    #[test]
    fn completed_event_serializes_all_snake_case() {
        let ev = BuildProgressEvent::Completed {
            release_id: "r1".into(),
            sha256: "abc".into(),
            size_bytes: 1024,
            file_name: "r1.zip".into(),
            artifact_path: "builds/r1.zip".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains(r#""event":"completed""#),
            "tag snake_case: {json}"
        );
        assert!(
            json.contains(r#""release_id":"r1""#),
            "release_id snake_case: {json}"
        );
        assert!(
            json.contains(r#""size_bytes":1024"#),
            "size_bytes snake_case: {json}"
        );
        assert!(
            json.contains(r#""file_name":"r1.zip""#),
            "file_name snake_case: {json}"
        );
        assert!(
            json.contains(r#""sha256":"abc""#),
            "sha256 unchanged: {json}"
        );
        assert!(
            json.contains(r#""artifact_path":"builds/r1.zip""#),
            "artifact_path snake_case: {json}"
        );
        // round-trip
        let back: BuildProgressEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn build_fail_tag_is_snake_case() {
        let ev = BuildProgressEvent::BuildFail {
            service: "web".into(),
            error: "boom".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains(r#""event":"build_fail""#),
            "tag build_fail: {json}"
        );
        let back: BuildProgressEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }

    /// 兼容 file-server 旧 wire 的关键:事件名 + 终态字段都能被消费端正确解析。
    #[test]
    fn terminal_events_round_trip() {
        for ev in [
            BuildProgressEvent::Completed {
                release_id: "r".into(),
                sha256: "s".into(),
                size_bytes: 1,
                file_name: "f".into(),
                artifact_path: "builds/f".into(),
            },
            BuildProgressEvent::Failed { error: "e".into() },
            BuildProgressEvent::Cancelled,
        ] {
            let json = serde_json::to_string(&ev).unwrap();
            let back: BuildProgressEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(back, ev, "round-trip failed for {json}");
        }
    }
}
