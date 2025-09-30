use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use diffy::{PatchFormatter, create_patch};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};

use super::bridge;
use mcp_types::MCP_SCHEMA_VERSION;
use tracing::info;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

const DEFAULT_READ_LINE_LIMIT: u32 = 1000;
const MAX_READ_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineRange {
    start: u32,
    end: u32,
}

#[derive(Debug)]
struct ReadSnippet {
    text: String,
    lines_returned: u32,
    end_line: u32,
    truncated_by_line_limit: bool,
    truncated_by_bytes: bool,
    additional_lines_available: bool,
    bytes_returned: usize,
}

#[derive(Default)]
struct StagedEdits {
    entries: HashMap<String, StagedFile>,
}

struct StagedFile {
    content: String,
}

impl StagedEdits {
    fn stage(&mut self, path: String, content: String) {
        self.entries.insert(path, StagedFile { content });
    }

    fn get(&self, path: &str) -> Option<&StagedFile> {
        self.entries.get(path)
    }
}

pub async fn run() -> Result<()> {
    let bridge_addr = std::env::var("ACP_FS_BRIDGE_ADDR")
        .context("ACP_FS_BRIDGE_ADDR environment variable is required")?;
    let session_id = std::env::var("ACP_FS_SESSION_ID")
        .context("ACP_FS_SESSION_ID environment variable is required")?;
    serve_loop(&bridge_addr, session_id).await
}

async fn serve_loop(bridge_addr: &str, session_id: String) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let stdout = tokio::io::stdout();
    let mut writer = BufWriter::new(stdout);
    let mut staged_edits = StagedEdits::default();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let msg: serde_json::Value = serde_json::from_str(&line)?;
        let method = msg.get("method").and_then(|m| m.as_str());
        let id = msg.get("id").cloned();

        match method {
            Some("initialize") => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id.clone().unwrap_or(json!(null)),
                    "result": {
                        "capabilities": {
                            "tools": {
                                "listChanged": true
                            }
                        },
                        "serverInfo": {
                            "name": "codex-acp-fs",
                            "title": "Codex ACP Filesystem",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    "protocolVersion": MCP_SCHEMA_VERSION,
                    }
                });
                write_message(&mut writer, response).await?;

                let notification = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                    "params": null
                });
                write_message(&mut writer, notification).await?;
            }
            Some("ping") => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id.clone().unwrap_or(json!(null)),
                    "result": {}
                });
                write_message(&mut writer, response).await?;
            }
            Some("tools/list") => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id.clone().unwrap_or(json!(null)),
                    "result": {
                        "tools": [
                            read_tool_definition(),
                            write_tool_definition(),
                            edit_tool_definition(),
                            multi_edit_tool_definition(),
                        ]
                    }
                });
                write_message(&mut writer, response).await?;
            }
            Some("tools/call") => {
                let result =
                    handle_tool_call(bridge_addr, &session_id, &msg, &mut staged_edits).await;
                match result {
                    Ok(value) => {
                        let response = json!({
                            "jsonrpc": "2.0",
                            "id": id.clone().unwrap_or(json!(null)),
                            "result": value
                        });
                        write_message(&mut writer, response).await?;
                    }
                    Err(err) => {
                        let response = json!({
                            "jsonrpc": "2.0",
                            "id": id.clone().unwrap_or(json!(null)),
                            "error": {
                                "code": -32001,
                                "message": err.to_string(),
                            }
                        });
                        write_message(&mut writer, response).await?;
                    }
                }
            }
            _ => {
                if let Some(id_value) = id {
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id_value,
                        "error": {
                            "code": -32601,
                            "message": "method not found"
                        }
                    });
                    write_message(&mut writer, response).await?;
                }
            }
        }
    }

    writer.flush().await?;
    Ok(())
}

async fn handle_tool_call(
    bridge_addr: &str,
    session_id: &str,
    message: &serde_json::Value,
    staged_edits: &mut StagedEdits,
) -> Result<serde_json::Value> {
    let params = message
        .get("params")
        .and_then(|p| p.get("arguments"))
        .cloned()
        .ok_or_else(|| anyhow!("missing tool call arguments"))?;

    let name = message
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_default();

    match name {
        "read_text_file" => {
            let ReadTextFileArgs { path, line, limit } = serde_json::from_value(params.clone())?;
            let start_line = line.unwrap_or(1).max(1);
            let requested_limit = limit
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_READ_LINE_LIMIT);
            let bridge_limit = requested_limit.saturating_add(1);
            let response = perform_bridge_request(
                bridge_addr,
                session_id,
                bridge::BridgeOp::Read,
                &path,
                line,
                Some(bridge_limit),
                None,
            )
            .await?;
            let mut snippet =
                prepare_read_snippet(&response, start_line, requested_limit, MAX_READ_BYTES);

            if let Some(hint) =
                build_file_read_hint(&snippet, start_line, requested_limit, MAX_READ_BYTES)
            {
                if !snippet.text.is_empty() {
                    snippet.text.push_str("\n\n");
                }
                snippet.text.push_str(&hint);
            }

            let ReadSnippet {
                text,
                lines_returned,
                end_line,
                truncated_by_line_limit,
                truncated_by_bytes,
                additional_lines_available,
                bytes_returned,
            } = snippet;

            let truncated =
                truncated_by_line_limit || truncated_by_bytes || additional_lines_available;
            let mut meta = json!({
                "path": path,
                "start_line": start_line,
                "end_line": end_line,
                "lines_returned": lines_returned,
                "line_limit": requested_limit,
                "bytes_returned": bytes_returned,
                "truncated": truncated,
                "truncated_by_line_limit": truncated_by_line_limit,
                "truncated_by_bytes": truncated_by_bytes,
                "additional_lines_available": additional_lines_available,
            });
            if truncated && let Some(obj) = meta.as_object_mut() {
                obj.insert("next_line".to_string(), json!(end_line.saturating_add(1)));
            }
            if truncated_by_bytes && let Some(obj) = meta.as_object_mut() {
                obj.insert("max_bytes".to_string(), json!(MAX_READ_BYTES));
            }

            Ok(json!({
                "content": [{
                    "type": "text",
                    "text": text,
                    "_meta": {
                        "codex_fs_read": meta
                    }
                }]
            }))
        }
        "write_text_file" => {
            let WriteTextFileArgs { path, content } = serde_json::from_value(params.clone())?;
            let mut final_content = content;
            let mut staged_applied = false;
            if let Some(entry) = staged_edits
                .get(&path)
                .filter(|entry| final_content.is_empty() || final_content == entry.content)
            {
                final_content = entry.content.clone();
                staged_applied = true;
            }

            let _ = perform_bridge_request(
                bridge_addr,
                session_id,
                bridge::BridgeOp::Write,
                &path,
                None,
                None,
                Some(final_content.clone()),
            )
            .await?;

            staged_edits.stage(path.clone(), final_content);

            let response_text = if staged_applied {
                "write completed (applied staged edits)"
            } else {
                "write completed"
            };

            Ok(json!({
                "content": [{
                    "type": "text",
                    "text": response_text
                }]
            }))
        }
        "edit_text_file" => {
            let args: EditTextFileArgs = serde_json::from_value(params.clone())?;
            let instructions = vec![EditInstruction {
                old_text: args.old_string,
                new_text: args.new_string,
                replace_all: false,
            }];
            stage_edits(
                bridge_addr,
                session_id,
                &args.path,
                instructions,
                staged_edits,
            )
            .await
        }
        "multi_edit_text_file" => {
            let args: MultiEditTextFileArgs = serde_json::from_value(params)?;
            if args.edits.is_empty() {
                return Err(anyhow!("edits array must not be empty"));
            }
            let instructions = args
                .edits
                .into_iter()
                .map(|edit| EditInstruction {
                    old_text: edit.old_string,
                    new_text: edit.new_string,
                    replace_all: edit.replace_all,
                })
                .collect::<Vec<_>>();
            stage_edits(
                bridge_addr,
                session_id,
                &args.path,
                instructions,
                staged_edits,
            )
            .await
        }
        other => Err(anyhow!("unknown tool {other}")),
    }
}

#[derive(Deserialize)]
struct ReadTextFileArgs {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct WriteTextFileArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditTextFileArgs {
    path: String,
    old_string: String,
    new_string: String,
}

#[derive(Deserialize)]
struct MultiEditTextFileArgs {
    path: String,
    edits: Vec<EditEntry>,
}

#[derive(Deserialize)]
struct EditEntry {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

struct EditInstruction {
    old_text: String,
    new_text: String,
    replace_all: bool,
}

async fn stage_edits(
    bridge_addr: &str,
    session_id: &str,
    path: &str,
    instructions: Vec<EditInstruction>,
    staged_edits: &mut StagedEdits,
) -> Result<serde_json::Value> {
    let base_content = if let Some(entry) = staged_edits.get(path) {
        entry.content.clone()
    } else {
        match perform_bridge_request(
            bridge_addr,
            session_id,
            bridge::BridgeOp::Read,
            path,
            None,
            None,
            None,
        )
        .await
        {
            Ok(content) => content,
            Err(err) => {
                let message = err.to_string();
                if is_missing_file_error(&message) {
                    String::new()
                } else {
                    return Err(err.context("failed to read current file content"));
                }
            }
        }
    };

    let new_content = apply_edits(&base_content, &instructions)?;

    if new_content == base_content {
        return Ok(json!({
            "content": [{
                "type": "text",
                "text": format!("No changes detected for {path}.")
            }]
        }));
    }

    let diff_text = format_diff_for_path(path, &base_content, &new_content);

    let write_content = new_content.clone();
    let staged_bytes = write_content.len();
    let _ = perform_bridge_request(
        bridge_addr,
        session_id,
        bridge::BridgeOp::Write,
        path,
        None,
        None,
        Some(write_content.clone()),
    )
    .await?;

    staged_edits.stage(path.to_string(), write_content);
    info!(file = %path, bytes = staged_bytes, "Staged edits committed");

    let (new_ranges, old_ranges) = parse_diff_line_ranges(&diff_text);
    let diff_meta = json!({
        "path": path,
        "new_ranges": line_ranges_to_json(&new_ranges),
        "old_ranges": line_ranges_to_json(&old_ranges),
    });

    Ok(json!({
        "content": [
            {
                "type": "text",
                "text": diff_text,
                "_meta": {
                    "codex_fs_diff": diff_meta
                }
            },
            {
                "type": "text",
                "text": format!("Write completed for {path}.")
            }
        ]
    }))
}

fn apply_edits(base: &str, edits: &[EditInstruction]) -> Result<String> {
    let mut content = base.to_string();
    for edit in edits {
        if edit.old_text.is_empty() {
            return Err(anyhow!(
                "the provided `old_string` is empty. No edits were applied."
            ));
        }

        if edit.replace_all {
            let replaced = content.replace(&edit.old_text, &edit.new_text);
            if replaced == content {
                return Err(anyhow!(
                    "The provided `old_string` does not appear in the file. No edits were applied."
                ));
            }
            content = replaced;
        } else {
            let Some(index) = content.find(&edit.old_text) else {
                return Err(anyhow!(
                    "The provided `old_string` does not appear in the file. No edits were applied."
                ));
            };
            let end = index + edit.old_text.len();
            content.replace_range(index..end, &edit.new_text);
        }
    }
    Ok(content)
}

fn format_diff_for_path(path: &str, before: &str, after: &str) -> String {
    let patch = create_patch(before, after);
    let formatter = PatchFormatter::new();
    let diff_body = formatter.fmt_patch(&patch).to_string();
    if diff_body.trim().is_empty() {
        format!("No textual differences for {path}.")
    } else {
        format!("--- {path}\n+++ {path}\n{diff_body}")
    }
}

fn is_missing_file_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no such file") || lower.contains("not found")
}

fn prepare_read_snippet(
    raw: &str,
    start_line: u32,
    requested_limit: u32,
    max_bytes: usize,
) -> ReadSnippet {
    if raw.is_empty() || requested_limit == 0 {
        return ReadSnippet {
            text: String::new(),
            lines_returned: 0,
            end_line: start_line.saturating_sub(1),
            truncated_by_line_limit: false,
            truncated_by_bytes: false,
            additional_lines_available: false,
            bytes_returned: 0,
        };
    }

    let mut text = String::new();
    let mut lines_taken: u32 = 0;
    let mut truncated_by_line_limit = false;
    let mut truncated_by_bytes = false;
    let mut bytes_used: usize = 0;

    for segment in raw.split_inclusive('\n') {
        if lines_taken >= requested_limit {
            truncated_by_line_limit = true;
            break;
        }

        let segment_bytes = segment.len();
        if bytes_used + segment_bytes > max_bytes {
            let remaining = max_bytes.saturating_sub(bytes_used);
            if remaining > 0 {
                let cut = truncate_to_char_boundary(segment, remaining);
                text.push_str(&segment[..cut]);
                bytes_used += cut;
            }
            truncated_by_bytes = true;
            lines_taken += 1;
            break;
        }

        text.push_str(segment);
        bytes_used += segment_bytes;
        lines_taken += 1;
    }

    let raw_line_count = if raw.is_empty() {
        0
    } else {
        raw.lines().count() as u32
    };

    let additional_lines_available =
        truncated_by_bytes || raw_line_count > lines_taken || bytes_used < raw.len();

    let end_line = if lines_taken == 0 {
        start_line.saturating_sub(1)
    } else {
        start_line.saturating_add(lines_taken.saturating_sub(1))
    };

    ReadSnippet {
        text,
        lines_returned: lines_taken,
        end_line,
        truncated_by_line_limit,
        truncated_by_bytes,
        additional_lines_available,
        bytes_returned: bytes_used,
    }
}

fn truncate_to_char_boundary(segment: &str, max_bytes: usize) -> usize {
    if max_bytes >= segment.len() {
        return segment.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !segment.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn build_file_read_hint(
    snippet: &ReadSnippet,
    start_line: u32,
    requested_limit: u32,
    max_bytes: usize,
) -> Option<String> {
    if !(snippet.truncated_by_line_limit
        || snippet.truncated_by_bytes
        || snippet.additional_lines_available)
    {
        return None;
    }

    let effective_end = if snippet.lines_returned == 0 {
        start_line
    } else {
        snippet.end_line
    };
    let mut description = format!("Read lines {}-{}", start_line, effective_end);

    if snippet.truncated_by_bytes {
        description.push_str(&format!(" (hit {} byte cap)", max_bytes));
    } else if snippet.truncated_by_line_limit || snippet.additional_lines_available {
        description.push_str(&format!(" (showing up to {} lines)", requested_limit));
    }

    let mut hint = format!("<file-read-info>{}", description);
    let next_line = snippet.end_line.saturating_add(1).max(start_line);
    hint.push_str(&format!(
        " Continue with line={} limit={}.",
        next_line, requested_limit
    ));
    hint.push_str("</file-read-info>");

    Some(hint)
}

fn parse_diff_line_ranges(diff_text: &str) -> (Vec<LineRange>, Vec<LineRange>) {
    let mut new_ranges = Vec::new();
    let mut old_ranges = Vec::new();

    for line in diff_text.lines() {
        let Some(stripped) = line.strip_prefix("@@") else {
            continue;
        };
        let Some((body, _)) = stripped.split_once("@@") else {
            continue;
        };
        for token in body.split_whitespace() {
            if let Some(range) = token.strip_prefix('+').and_then(parse_range_token) {
                new_ranges.push(range);
            } else if let Some(range) = token.strip_prefix('-').and_then(parse_range_token) {
                old_ranges.push(range);
            }
        }
    }

    (new_ranges, old_ranges)
}

fn parse_range_token(token: &str) -> Option<LineRange> {
    if token.is_empty() {
        return None;
    }

    let mut parts = token.split(',');
    let start = parts.next()?.parse::<i64>().ok()?;
    let count = parts
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1);

    if count <= 0 {
        return None;
    }

    let start = start.max(1) as u32;
    let end = start.saturating_add((count as u32).saturating_sub(1));
    Some(LineRange { start, end })
}

fn line_ranges_to_json(ranges: &[LineRange]) -> Vec<serde_json::Value> {
    ranges
        .iter()
        .map(|range| {
            json!({
                "start": range.start,
                "end": range.end,
            })
        })
        .collect()
}

async fn perform_bridge_request(
    bridge_addr: &str,
    session_id: &str,
    op: bridge::BridgeOp,
    path: &str,
    line: Option<u32>,
    limit: Option<u32>,
    content: Option<String>,
) -> Result<String> {
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut stream = TcpStream::connect(bridge_addr)
        .await
        .with_context(|| format!("failed to connect to bridge at {bridge_addr}"))?;
    let (reader_half, mut writer_half) = stream.split();
    let mut reader = BufReader::new(reader_half).lines();

    let payload = serde_json::to_string(&json!({
        "id": request_id,
        "session_id": session_id,
        "op": match op {
            bridge::BridgeOp::Read => "read",
            bridge::BridgeOp::Write => "write",
        },
        "path": path,
        "line": line,
        "limit": limit,
        "content": content,
    }))?;

    writer_half.write_all(payload.as_bytes()).await?;
    writer_half.write_all(b"\n").await?;
    writer_half.flush().await?;

    let line = timeout(Duration::from_secs(5), reader.next_line())
        .await
        .map_err(|_| anyhow!("bridge request timed out"))??
        .ok_or_else(|| anyhow!("bridge closed connection"))?;

    let response: serde_json::Value = serde_json::from_str(&line)?;
    let success = response
        .get("success")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if success {
        Ok(response
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string())
    } else {
        let message = response
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("bridge error");
        Err(anyhow!(message.to_string()))
    }
}

async fn write_message(
    writer: &mut BufWriter<tokio::io::Stdout>,
    value: serde_json::Value,
) -> Result<()> {
    let payload = serde_json::to_string(&value)?;
    writer.write_all(payload.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn read_tool_definition() -> serde_json::Value {
    json!({
        "name": "read_text_file",
        "description": "Read workspace files via ACP bridge (paged to ~1000 lines/50KB; use line/limit to continue).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to read." },
                "line": { "type": "integer", "minimum": 1, "description": "Optional start line (1-indexed)." },
                "limit": { "type": "integer", "minimum": 1, "description": "Optional number of lines to read." }
            },
            "required": ["path"],
            "additionalProperties": false
        }
    })
}

fn write_tool_definition() -> serde_json::Value {
    json!({
        "name": "write_text_file",
        "description": "Write workspace files via ACP bridge.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to write." },
                "content": { "type": "string", "description": "Full file contents." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }
    })
}

fn edit_tool_definition() -> serde_json::Value {
    json!({
        "name": "edit_text_file",
        "description": "Apply a focused replacement in a file and persist the result.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to modify." },
                "old_string": { "type": "string", "description": "Existing text to replace (must match exactly)." },
                "new_string": { "type": "string", "description": "Replacement text." }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        }
    })
}

fn multi_edit_tool_definition() -> serde_json::Value {
    json!({
        "name": "multi_edit_text_file",
        "description": "Apply multiple sequential replacements in a file and persist the result.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to modify." },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string", "description": "The text to replace." },
                            "new_string": { "type": "string", "description": "Replacement text." },
                            "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    },
                    "minItems": 1
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        }
    })
}
