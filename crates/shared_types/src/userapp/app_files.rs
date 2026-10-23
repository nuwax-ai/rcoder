//! Internal file-server `app-files` family contract (upload / upload-from-url /
//! list). The producer (file-server-userapp handlers) and the forwarding
//! consumer (app_manager) share these types instead of json! literals paired
//! with private DTO copies.
//!
//! Wire shape is frozen to the current contract: snake_case keys, flat
//! `success`, `extracted_count: null` for plain single-file uploads. The listing reader keeps the legacy omitted-`files` default; DTO
//! extraction must not tighten the established forwarding contract. Other
//! required fields, including the existing success confirmation, stay required.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Upload / upload-from-url success result (flat success, no HttpResult envelope).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserappAppFilesUploadResult {
    /// Always true on HTTP 200; error paths use the business error envelope.
    pub success: bool,
    /// App-root-relative landing path of the upload target.
    pub file_path: String,
    /// Upload size in bytes (archive byte size before extraction).
    pub file_size: u64,
    /// RFC 3339 upload completion timestamp.
    pub uploaded_at: String,
    /// Extracted entry count for archives; `null` for plain single-file writes.
    pub extracted_count: Option<usize>,
}

/// One directory entry in the listing projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserappAppFilesEntry {
    /// Path relative to the app root.
    pub path: String,
    /// Entry size in bytes; 0 for directories.
    pub size: u64,
    /// True when the entry is a directory.
    pub is_dir: bool,
    /// RFC 3339 mtime; empty string when the platform cannot report it.
    pub modified_at: String,
}

/// Directory listing result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserappAppFilesListResult {
    /// Always true on HTTP 200; error paths use the business error envelope.
    pub success: bool,
    /// Entries directly inside the requested directory (non-recursive).
    /// Older successful peers may omit an empty listing.
    #[serde(default)]
    pub files: Vec<UserappAppFilesEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire 快照：与生产者当前 json! 输出逐字节一致（键序/平铺/null 保留）。
    #[test]
    fn upload_wire_snapshot_matches_current_producer_shape() {
        let value = UserappAppFilesUploadResult {
            success: true,
            file_path: "dist/app.tar.gz".into(),
            file_size: 1024,
            uploaded_at: "2026-10-23T08:00:00+00:00".into(),
            extracted_count: Some(3),
        };
        assert_eq!(
            serde_json::to_value(&value).unwrap(),
            serde_json::json!({
                "success": true,
                "file_path": "dist/app.tar.gz",
                "file_size": 1024,
                "uploaded_at": "2026-10-23T08:00:00+00:00",
                "extracted_count": 3,
            })
        );
        // 单文件直写: extracted_count 序列化为 null, 不缺省键
        let plain = UserappAppFilesUploadResult {
            extracted_count: None,
            ..value.clone()
        };
        assert_eq!(
            serde_json::to_value(&plain).unwrap()["extracted_count"],
            serde_json::Value::Null
        );
        // 消费者方向：生产者 JSON 必须可还原
        let parsed: UserappAppFilesUploadResult =
            serde_json::from_value(serde_json::to_value(&plain).unwrap()).unwrap();
        assert_eq!(parsed, plain);
    }

    #[test]
    fn list_wire_snapshot_preserves_omitted_empty_listing() {
        let value = UserappAppFilesListResult {
            success: true,
            files: vec![UserappAppFilesEntry {
                path: "index.html".into(),
                size: 128,
                is_dir: false,
                modified_at: "2026-10-23T08:00:00+00:00".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&value).unwrap(),
            serde_json::json!({
                "success": true,
                "files": [
                    {"path": "index.html", "size": 128, "is_dir": false,
                     "modified_at": "2026-10-23T08:00:00+00:00"}
                ],
            })
        );
        // Keep the legacy reader default while always serializing `files`.
        let empty: UserappAppFilesListResult =
            serde_json::from_value(serde_json::json!({"success": true})).unwrap();
        assert!(empty.files.is_empty());
        assert_eq!(
            serde_json::to_value(empty).unwrap(),
            serde_json::json!({"success": true, "files": []})
        );
    }
    /// The forwarding reader accepted a successful legacy listing without
    /// `files`; introducing a shared DTO must not tighten that contract.
    #[test]
    fn legacy_listing_without_files_still_deserializes() {
        let parsed: UserappAppFilesListResult = serde_json::from_value(
            serde_json::json!({"success": true, "legacy_extension": "ignored"}),
        )
        .expect("legacy omitted files is an empty listing");
        assert!(parsed.success);
        assert!(parsed.files.is_empty());
        assert!(
            serde_json::from_value::<UserappAppFilesListResult>(serde_json::json!({"files": []}))
                .is_err(),
            "success remains mandatory as in the forwarding envelope check"
        );
    }
}
