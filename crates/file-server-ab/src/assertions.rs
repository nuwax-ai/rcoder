//! 响应体断言家族（validate_*）与解析辅助。

use std::fs;
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::compare::*;
use crate::report::*;
use crate::scenarios::lifecycle::*;
use crate::types::*;
use crate::util::*;

pub(crate) fn validate_files_update_response(
    body: &[u8],
    expected_user: &str,
    expected_cid: &str,
    expected_count: u64,
) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("files-update response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true)
        || value.get("userId").and_then(Value::as_str) != Some(expected_user)
        || value.get("cId").and_then(Value::as_str) != Some(expected_cid)
        || value.get("filesCount").and_then(Value::as_u64) != Some(expected_count)
    {
        return Err(format!(
            "files-update response must confirm success, workspace identity, and {expected_count} operations; got {value}"
        ));
    }
    Ok(())
}

pub(crate) fn validate_upload_file_response(body: &[u8], expected_size: u64) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("upload-file response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true)
        || value.get("fileSize").and_then(Value::as_u64) != Some(expected_size)
    {
        return Err(format!(
            "upload-file response must confirm success and {expected_size} bytes; got {value}"
        ));
    }
    Ok(())
}

pub(crate) fn validate_upload_files_response(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("upload-files response is not JSON: {error}"))?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .filter(|results| results.len() == 2)
        .ok_or_else(|| "upload-files response must contain two results".to_string())?;
    let expected = [
        ("ab-transfer/batch/one.txt", "one.txt", 17),
        ("ab-transfer/batch/nested/two.bin", "two.bin", 5),
    ];
    if value.get("success").and_then(Value::as_bool) != Some(true)
        || value.get("totalCount").and_then(Value::as_u64) != Some(2)
        || value.get("successCount").and_then(Value::as_u64) != Some(2)
        || value.get("failCount").and_then(Value::as_u64) != Some(0)
    {
        return Err(format!(
            "upload-files response must report two successful files and zero failures; got {value}"
        ));
    }
    for (result, (path, name, size)) in results.iter().zip(expected) {
        if result.get("success").and_then(Value::as_bool) != Some(true)
            || result.get("filePath").and_then(Value::as_str) != Some(path)
            || result.get("originalname").and_then(Value::as_str) != Some(name)
            || result.get("fileSize").and_then(Value::as_u64) != Some(size)
        {
            return Err(format!(
                "upload-files result for {path} must preserve path, filename, and {size}-byte size; got {result}"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_body_equals(body: &[u8], expected: &[u8]) -> Result<(), String> {
    if body == expected {
        Ok(())
    } else {
        Err(format!(
            "file body differs: expected {} bytes sha256={}, got {} bytes sha256={}",
            expected.len(),
            sha256(expected),
            body.len(),
            sha256(body)
        ))
    }
}

pub(crate) fn validate_directory_empty_or_absent(path: &Path) -> Result<(), String> {
    let mut entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read temporary upload directory: {error}")),
    };
    match entries.next() {
        None => Ok(()),
        Some(Ok(entry)) => Err(format!(
            "temporary upload directory still contains {}",
            entry.file_name().to_string_lossy()
        )),
        Some(Err(error)) => Err(format!("read temporary upload directory entry: {error}")),
    }
}

pub(crate) fn validate_success_json(body: &[u8]) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!("response must contain success=true; got {value}"))
    }
}

pub(crate) fn validate_boolean_field(
    body: &[u8],
    field: &str,
    expected: bool,
) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("response is not JSON: {error}"))?;
    if value.get(field).and_then(Value::as_bool) == Some(expected) {
        Ok(())
    } else {
        Err(format!(
            "response field {field} must be {expected}; got {value}"
        ))
    }
}

pub(crate) fn validate_json_string_field(
    body: &[u8],
    field: &str,
    expected: &str,
) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get(field).and_then(Value::as_str) == Some(expected)
    {
        Ok(())
    } else {
        Err(format!(
            "response must contain success=true and {field}={expected:?}; got {value}"
        ))
    }
}

pub(crate) fn validate_deprecated_json(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("deprecated response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(false)
        && value.get("deprecated").and_then(Value::as_bool) == Some(true)
    {
        Ok(())
    } else {
        Err(format!(
            "response must explicitly mark the route deprecated; got {value}"
        ))
    }
}

pub(crate) fn validate_execute_command(body: &[u8]) -> Result<(), String> {
    validate_execute_command_output(body, "file-server-ab-command-ok\n")
}

pub(crate) fn validate_execute_command_output(
    body: &[u8],
    expected_stdout: &str,
) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("execute-command response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("exitCode").and_then(Value::as_i64) == Some(0)
        && value.get("stdout").and_then(Value::as_str) == Some(expected_stdout)
        && value.get("stderr").and_then(Value::as_str) == Some("")
    {
        Ok(())
    } else {
        Err(format!(
            "execute-command output did not match the expected command result; got {value}"
        ))
    }
}

pub(crate) fn validate_git_tree_response(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("git tree response is not JSON: {error}"))?;
    let tree = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let is_hex_tree =
        matches!(tree.len(), 40 | 64) && tree.bytes().all(|byte| byte.is_ascii_hexdigit());
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("exitCode").and_then(Value::as_i64) == Some(0)
        && value.get("stderr").and_then(Value::as_str) == Some("")
        && is_hex_tree
    {
        Ok(())
    } else {
        Err(format!(
            "init-project-template must create a Git commit with a tree hash; got {value}"
        ))
    }
}

pub(crate) fn validate_log_tail(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("get-logs response is not JSON: {error}"))?;
    let logs = value.get("logs").and_then(Value::as_array);
    let expected = [
        json!({"line": 3, "content": "third line"}),
        json!({"line": 4, "content": "fourth line"}),
    ];
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("totalLines").and_then(Value::as_u64) == Some(4)
        && value.get("startIndex").and_then(Value::as_u64) == Some(3)
        && value.get("logFileName").and_then(Value::as_str) == Some("ab.log")
        && logs.is_some_and(|actual| actual == &expected)
    {
        Ok(())
    } else {
        Err(format!(
            "get-logs did not return the expected two-line tail; got {value}"
        ))
    }
}

pub(crate) fn validate_build_artifact(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("build-agent-package response is not JSON: {error}"))?;
    let artifacts = value.get("artifacts").and_then(Value::as_array);
    let matches = artifacts.is_some_and(|items| {
        items.len() == 1
            && items[0].get("fileName").and_then(Value::as_str)
                == Some("agent-17-linux-x64-1.2.3.zip")
            && items[0].get("platform").and_then(Value::as_str) == Some("linux-x64")
            && items[0]
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("/agent-17-linux-x64-1.2.3.zip"))
    });
    if value.get("success").and_then(Value::as_bool) == Some(true) && matches {
        Ok(())
    } else {
        Err(format!(
            "build-agent-package artifact discovery differs from fixture; got {value}"
        ))
    }
}

/// Approved copy semantics (confirmed 2026-09-26): copies do NOT inherit the source
/// Git history. Rust's copy repo contains exactly the copy commit; TypeScript's
/// copy carries the source's init commit plus the copy commit (its known behavior
/// of copying `.git`, remotes included).
pub(crate) fn validate_copy_git_history(body: &[u8], side: &str) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("copy git log response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("copy git log must contain success=true".into());
    }
    let copy_message =
        "copy project: file-server-ab-project-lifecycle -> file-server-ab-project-copy";
    let commits = value
        .get("commits")
        .and_then(Value::as_array)
        .ok_or_else(|| "copy git log must contain a commits array".to_string())?;
    let message_of = |index: usize| {
        commits
            .get(index)
            .and_then(|commit| commit.get("message"))
            .and_then(Value::as_str)
            .map(str::trim_end)
            .unwrap_or_default()
            .to_string()
    };
    let total = value.get("total").and_then(Value::as_u64);
    if side == "rust" {
        if total != Some(1) || commits.len() != 1 || message_of(0) != copy_message {
            return Err(format!(
                "Rust copy history must contain only the copy commit; total={total:?}, commits={commits_len}",
                commits_len = commits.len()
            ));
        }
    } else if total != Some(2)
        || commits.len() != 2
        || message_of(0) != copy_message
        || !message_of(1).starts_with("init project: file-server-ab-project-lifecycle")
    {
        return Err(format!(
            "TypeScript copy history must contain the source init commit plus the copy commit; total={total:?}, commits={commits_len}",
            commits_len = commits.len()
        ));
    }
    Ok(())
}

pub(crate) fn validate_project_copy(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("copy-project response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("sourceProjectId").and_then(Value::as_str)
            == Some("file-server-ab-project-lifecycle")
        && value.get("targetProjectId").and_then(Value::as_str)
            == Some("file-server-ab-project-copy")
    {
        Ok(())
    } else {
        Err(format!(
            "copy-project response does not identify its source and target; got {value}"
        ))
    }
}

pub(crate) fn validate_attachment_response(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("attachment response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("fileName").and_then(Value::as_str) == Some("ab-attachment.txt")
        && value.get("relativePath").and_then(Value::as_str)
            == Some(".attachments/ab-attachment.txt")
    {
        Ok(())
    } else {
        Err(format!(
            "attachment response does not match its requested name; got {value}"
        ))
    }
}

pub(crate) fn validate_project_delete(body: &[u8], project_id: &str) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("delete-project response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("projectId").and_then(Value::as_str) == Some(project_id)
        && value
            .get("failedDirectories")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        Ok(())
    } else {
        Err(format!(
            "delete-project did not confirm a clean deletion; got {value}"
        ))
    }
}

pub(crate) fn validate_download_archive(
    body: &[u8],
    user_id: &str,
    cid: &str,
) -> Result<(), String> {
    let entries = zip_semantic_entries(body).map_err(|error| error.to_string())?;
    let entries = entries
        .as_object()
        .ok_or_else(|| "ZIP semantic entries are not an object".to_string())?;
    let prefix = format!("{user_id}_{cid}/");
    if !entries.keys().any(|path| path.starts_with(&prefix))
        || entries.keys().any(|path| path.contains("/."))
        || !entries.contains_key(&format!("{prefix}README.md"))
    {
        return Err(format!(
            "download-all-files ZIP must have the expected prefix, README and no dot-segment entries; paths={:?}",
            entries.keys().collect::<Vec<_>>()
        ));
    }
    Ok(())
}

pub(crate) fn validate_workspace_archive(body: &[u8]) -> Result<(), String> {
    let entries = zip_semantic_entries(body).map_err(|error| error.to_string())?;
    let entries = entries
        .as_object()
        .ok_or_else(|| "ZIP semantic entries are not an object".to_string())?;
    if !entries.contains_key("README.md")
        || !entries.contains_key(".gitignore")
        || entries.keys().any(|path| path.starts_with("empty/"))
        || !entries.contains_key(".hidden.txt")
    {
        return Err(format!(
            "zip-workspace ZIP must be unprefixed, retain hidden and gitignore files, and exclude the requested directory; paths={:?}",
            entries.keys().collect::<Vec<_>>()
        ));
    }
    Ok(())
}

pub(crate) fn parse_dev_server(
    exchange: &Exchange,
    side: &str,
    project_id: &str,
) -> Result<DevServerIdentity, String> {
    if !exchange.status.is_some_and(|status| status.is_success()) {
        return Err(format!(
            "{side} start response was HTTP {:?}",
            exchange.status
        ));
    }
    let body: Value = serde_json::from_slice(&exchange.body)
        .map_err(|error| format!("{side} start response is not JSON: {error}"))?;
    if body.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!("{side} start response did not set success=true"));
    }
    if body.get("projectId").and_then(Value::as_str) != Some(project_id) {
        return Err(format!(
            "{side} start response projectId does not match {project_id}"
        ));
    }
    let pid = body
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| *pid > 0)
        .ok_or_else(|| format!("{side} start response pid must be a positive integer"))?;
    let port = body
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("{side} start response port must be a valid TCP port"))?;
    Ok(DevServerIdentity { pid, port })
}

pub(crate) fn json_bool(body: &[u8], field: &str) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get(field).and_then(Value::as_bool))
        == Some(true)
}

pub(crate) fn validate_git_commit_response(body: &[u8]) -> Result<String, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("commit response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("commit response must contain success=true".into());
    }
    if value.get("message").and_then(Value::as_str) != Some("Commit successful") {
        return Err("initial non-empty commit must return 'Commit successful'".into());
    }
    let hash = value
        .get("commit")
        .and_then(Value::as_str)
        .ok_or_else(|| "commit response must contain a commit hash".to_string())?;
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "commit must return a full 40-digit SHA-1 hash, got {hash:?}"
        ));
    }
    Ok(hash.to_string())
}

pub(crate) fn validate_git_conflicted_status(
    body: &[u8],
    expected_path: &str,
) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("Git status response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("Git status response must contain success=true".into());
    }
    let conflicted = value
        .get("conflicted")
        .and_then(Value::as_array)
        .ok_or_else(|| "Git status response must contain a conflicted array".to_string())?;
    if !conflicted
        .iter()
        .any(|path| path.as_str() == Some(expected_path))
    {
        return Err(format!(
            "Git status conflicted list must contain {expected_path:?}, got {conflicted:?}"
        ));
    }
    Ok(())
}

pub(crate) fn validate_git_log_response(body: &[u8]) -> Result<String, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("git log response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("git log response must contain success=true".into());
    }
    if value.get("total").and_then(Value::as_u64) != Some(2) {
        return Err("git log should return the two fixture commits".into());
    }
    let commits = value
        .get("commits")
        .and_then(Value::as_array)
        .filter(|commits| commits.len() == 2)
        .ok_or_else(|| "git log should contain two commit entries".to_string())?;
    let commit = &commits[0];
    let hash = commit
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| "git log entry must contain hash".to_string())?;
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("git log entry must contain a full 40-digit SHA-1 hash".into());
    }
    let date = commit
        .get("date")
        .and_then(Value::as_str)
        .ok_or_else(|| "git log entry must contain date".to_string())?;
    chrono::DateTime::parse_from_rfc3339(date)
        .map_err(|error| format!("git log date must be RFC 3339: {error}"))?;
    for (index, expected_message) in ["A/B second commit", "A/B initial commit"]
        .into_iter()
        .enumerate()
    {
        let entry = &commits[index];
        let entry_hash = entry
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("git log entry {index} must contain hash"))?;
        if entry_hash.len() != 40 || !entry_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "git log entry {index} must contain a full 40-digit SHA-1 hash"
            ));
        }
        let date = entry
            .get("date")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("git log entry {index} must contain date"))?;
        chrono::DateTime::parse_from_rfc3339(date)
            .map_err(|error| format!("git log entry {index} date must be RFC 3339: {error}"))?;
        if entry.get("author_name").and_then(Value::as_str) != Some("File Server A-B")
            || entry.get("author_email").and_then(Value::as_str) != Some("ab@example.invalid")
            || entry
                .get("message")
                .and_then(Value::as_str)
                .is_none_or(|message| message.trim_end_matches('\n') != expected_message)
        {
            return Err(format!(
                "git log entry {index} must contain the fixture author and message {expected_message:?}"
            ));
        }
    }
    Ok(hash.to_string())
}

/// Assert the value looks like a semantic version; implementations declare their
/// own contract line and must not echo each other's number.
pub(crate) fn validate_semver_field(body: &[u8], field: &str) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("response is not JSON: {error}"))?;
    let version = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("field {field} must be a string"))?;
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || !parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!(
            "field {field} must be a semantic version, got {version:?}"
        ));
    }
    Ok(())
}

pub(crate) fn validate_posix_backslash_list_link(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("file list response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("file list response must contain success=true".into());
    }
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| "file list response must contain files".to_string())?;
    let link = files
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("literal\\link.txt"))
        .ok_or_else(|| {
            "live POSIX link literal\\link.txt must be listed with its original name".to_string()
        })?;
    if link.get("isDir").and_then(Value::as_bool) != Some(false)
        || link.get("isLink").and_then(Value::as_bool) != Some(true)
        || link.get("fileProxyUrl").and_then(Value::as_str) != Some("/proxy/literal%5Clink.txt")
    {
        return Err(format!(
            "POSIX link must remain a link with a percent-encoded literal backslash URL: {link}"
        ));
    }
    Ok(())
}

pub(crate) fn validate_meta_response(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("file metadata response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("file metadata response must contain success=true".into());
    }
    let metas = value
        .get("metas")
        .and_then(Value::as_array)
        .filter(|metas| metas.len() == 2)
        .ok_or_else(|| "file metadata response must contain two requested entries".to_string())?;
    for (entry, path) in metas.iter().zip(["README.md", "sub/nested/hello.txt"]) {
        if entry.get("path").and_then(Value::as_str) != Some(path)
            || entry.get("isDir").and_then(Value::as_bool) != Some(false)
            || entry.get("isLink").and_then(Value::as_bool) != Some(false)
            || !entry
                .get("size")
                .and_then(Value::as_u64)
                .is_some_and(|size| size > 0)
            || !entry
                .get("mtimeMs")
                .and_then(Value::as_f64)
                .is_some_and(|mtime| mtime > 0.0)
        {
            return Err(format!(
                "file metadata entry for {path} is incomplete or invalid"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_boundary_meta_response(body: &[u8], side: &str) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("boundary file metadata response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("boundary file metadata response must contain success=true".into());
    }
    let metas = value
        .get("metas")
        .and_then(Value::as_array)
        .filter(|metas| metas.len() == 3)
        .ok_or_else(|| {
            "boundary metadata response must contain three requested entries".to_string()
        })?;
    for (index, path) in [(0, ".hidden.txt"), (2, "binary.bin")] {
        let entry = &metas[index];
        if entry.get("path").and_then(Value::as_str) != Some(path)
            || entry.get("isDir").and_then(Value::as_bool) != Some(false)
            || entry.get("isLink").and_then(Value::as_bool) != Some(false)
            || !entry
                .get("mtimeMs")
                .and_then(Value::as_f64)
                .is_some_and(|mtime| mtime > 0.0)
        {
            return Err(format!(
                "boundary metadata entry for {path} is incomplete or invalid"
            ));
        }
    }
    // metas[1] 是带首尾空格的文件名。TS v1.5.6 起路径原样使用不再 trim,
    // 两侧契约统一为: 精确寻址返回完整元数据 (回显原名、error 缺席、字节 24)。
    // （旧断言曾接受 TS trim+ENOENT 的"已知缺陷"形态, 已随 TS 修复移除。）
    let spaced = &metas[1];
    let full = spaced
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| path == "  中文文件 .txt  ");
    let complete = spaced.get("error").is_none()
        && spaced.get("isDir").and_then(Value::as_bool) == Some(false)
        && spaced.get("isLink").and_then(Value::as_bool) == Some(false)
        && spaced.get("size").and_then(Value::as_u64) == Some(24)
        && spaced
            .get("mtimeMs")
            .and_then(Value::as_f64)
            .is_some_and(|mtime| mtime > 0.0);
    if !full || !complete {
        return Err(format!(
            "{side} must resolve the exact spaced file name with full metadata"
        ));
    }
    Ok(())
}

/// Per-side dev-log page contract: success, echoed startIndex, non-empty first page,
/// string content and consecutive line numbers on every entry, totalLines covering
/// the last returned line, and the temp-log file name shape. Log TEXT may differ
/// between implementations; this contract must hold on each side independently.
pub(crate) fn validate_log_page(body: &[u8], requested_start: u64) -> Result<(u64, u64), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("log page is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("log page must contain success=true".into());
    }
    if value.get("startIndex").and_then(Value::as_u64) != Some(requested_start) {
        return Err(format!(
            "startIndex must echo the requested start {requested_start}"
        ));
    }
    let log_file = value
        .get("logFileName")
        .and_then(Value::as_str)
        .ok_or_else(|| "logFileName must be a string".to_string())?;
    let stamp = log_file
        .strip_prefix("dev-temp-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .filter(|stamp| !stamp.is_empty() && stamp.bytes().all(|byte| byte.is_ascii_digit()));
    if stamp.is_none() {
        return Err(format!(
            "temp log file name must look like dev-temp-<epoch-millis>.log, got {log_file:?}"
        ));
    }
    let logs = value
        .get("logs")
        .and_then(Value::as_array)
        .ok_or_else(|| "logs must be an array".to_string())?;
    if requested_start == 1 && logs.is_empty() {
        return Err("the first log page must not be empty".into());
    }
    let mut last_line = requested_start.saturating_sub(1);
    for entry in logs {
        let line = entry
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| "each log entry must carry a numeric line".to_string())?;
        if entry.get("content").and_then(Value::as_str).is_none() {
            return Err("each log entry must carry a string content".into());
        }
        if line != last_line + 1 {
            return Err(format!(
                "log line numbers must be consecutive from {requested_start}; got {line} after {last_line}"
            ));
        }
        last_line = line;
    }
    let total = value
        .get("totalLines")
        .and_then(Value::as_u64)
        .ok_or_else(|| "totalLines must be a number".to_string())?;
    if total < last_line {
        return Err(format!(
            "totalLines {total} is smaller than the last returned line {last_line}"
        ));
    }
    Ok((last_line, total))
}

pub(crate) fn response_has_project(body: &[u8], project_id: &str) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("list").and_then(Value::as_array).cloned())
        .is_some_and(|list| {
            list.iter()
                .any(|entry| entry.get("projectId").and_then(Value::as_str) == Some(project_id))
        })
}

pub(crate) fn assertion_difference(case: &str, path: &str, message: String) -> Difference {
    diff(case, path, "assertion_failed", Some(json!(message)), None)
}

#[cfg(test)]
mod posix_backslash_list_link_tests {
    use super::*;

    #[test]
    fn validates_original_symlink_name_and_encoded_proxy_url() {
        let expected = json!({
            "success": true,
            "files": [{
                "name": "literal\\link.txt",
                "isDir": false,
                "isLink": true,
                "fileProxyUrl": "/proxy/literal%5Clink.txt"
            }]
        });
        let validate =
            |value: &Value| validate_posix_backslash_list_link(&serde_json::to_vec(value).unwrap());
        assert!(validate(&expected).is_ok());

        let mut missing = expected.clone();
        missing["files"] = json!([]);
        assert!(validate(&missing).is_err(), "hidden live link must fail");

        let mut normalized_name = expected.clone();
        normalized_name["files"][0]["name"] = json!("literal/link.txt");
        assert!(
            validate(&normalized_name).is_err(),
            "rewritten name must fail"
        );

        let mut lost_link_flag = expected.clone();
        lost_link_flag["files"][0]["isLink"] = json!(false);
        assert!(
            validate(&lost_link_flag).is_err(),
            "lost link identity must fail"
        );

        let mut rewritten_url = expected;
        rewritten_url["files"][0]["fileProxyUrl"] = json!("/proxy/literal/link.txt");
        assert!(validate(&rewritten_url).is_err(), "rewritten URL must fail");
    }
}
