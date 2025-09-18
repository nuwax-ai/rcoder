use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// V0 文件数据结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V0FileData {
    #[serde(rename = "blockId")]
    pub block_id: String,
    pub source: String,
}

/// V0 文件条目结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V0FileEntry {
    pub file_type: String,
    pub file_path: PathBuf,
    pub is_merged: bool,
    pub is_edit: bool,
    pub is_quick_edit: bool,
    pub url: Option<String>,
    pub content: String,
    pub hash: String,
}

/// V0 解析结果结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V0ParseResult {
    pub files: Vec<V0FileEntry>,
    pub block_id: String,
}

/// 项目文件结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFile {
    pub path: PathBuf,
    pub content: String,
    pub hash: String,
    pub size: u64,
}

/// V0 解析错误类型
#[derive(Debug, thiserror::Error)]
pub enum V0ParseError {
    #[error("Invalid V0 file format: {0}")]
    InvalidFormat(String),
    #[error("Missing required attribute: {0}")]
    MissingAttribute(String),
    #[error("File IO error: {0}")]
    IoError(String),
    #[error("Network error: {0}")]
    NetworkError(String),
    #[error("Hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
}

/// 同步结果结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncResult {
    pub written_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub success: bool,
}