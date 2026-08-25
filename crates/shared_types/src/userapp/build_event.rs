//! UserApp build 进度事件 —— file-server(发送)与 rcoder(接收)共享的类型化 DTO。
//!
//! 历史:file-server 定义、rcoder 以字符串键(`event`/`release_id`/`error`)重复解析,
//! 字段重命名会静默断链。统一到此模块,两端共用同一类型,消除漂移。
//!
//! wire casing 统一 camelCase(tag 值 + 字段)。修掉原来 serde 意外的混合
//! (容器 rename_all 只作用于 tag,struct-variant 字段是 snake_case)。每个 struct variant
//! 显式 `#[serde(rename_all = "camelCase")]` 让字段也驼峰化。

use serde::{Deserialize, Serialize};

/// build 进度事件(file-server 经 SSE 发送,rcoder 接收)。`tag = "event"` 内部标签枚举。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum BuildProgressEvent {
    /// 进入新阶段
    Stage { stage: String },
    /// 开始编译某服务
    Building { service: String },
    /// 某服务编译成功
    BuildOk { service: String },
    /// 某服务编译失败
    #[serde(rename_all = "camelCase")]
    BuildFail { service: String, error: String },
    /// 一行日志(实时 tail)
    #[serde(rename_all = "camelCase")]
    Log { service: String, line: String },
    /// 任务完成(build 产 release_id + 包摘要)。`artifact_path` 为相对 workspace
    /// 根的产物路径（`builds/workspace-package-{releaseId}.zip`）——取包 URL
    /// `/api/userapp/static/{appId}/{artifactPath}` 的直接拼装段。
    #[serde(rename_all = "camelCase")]
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

    /// wire 全 camelCase:tag 值(buildOk)+ 字段(releaseId/sizeBytes/fileName/artifactPath)。
    #[test]
    fn completed_event_serializes_all_camel_case() {
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
            "tag camelCase: {json}"
        );
        assert!(
            json.contains(r#""releaseId":"r1""#),
            "releaseId camelCase: {json}"
        );
        assert!(
            json.contains(r#""sizeBytes":1024"#),
            "sizeBytes camelCase: {json}"
        );
        assert!(
            json.contains(r#""fileName":"r1.zip""#),
            "fileName camelCase: {json}"
        );
        assert!(
            json.contains(r#""sha256":"abc""#),
            "sha256 unchanged: {json}"
        );
        assert!(
            json.contains(r#""artifactPath":"builds/r1.zip""#),
            "artifactPath camelCase: {json}"
        );
        // round-trip
        let back: BuildProgressEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn build_fail_tag_is_camel_case() {
        let ev = BuildProgressEvent::BuildFail {
            service: "web".into(),
            error: "boom".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains(r#""event":"buildFail""#),
            "tag buildFail: {json}"
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
