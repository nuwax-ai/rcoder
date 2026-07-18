use std::fmt;
use std::str::FromStr;

use gix::objs::tree::EntryMode;

use crate::error::{AppError, AppResult};

/// diff 数据来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSource {
    Worktree,
    Staged,
    Commit,
}

impl DiffSource {
    pub fn parse(s: &str) -> AppResult<Self> {
        match s {
            "" | "worktree" => Ok(Self::Worktree),
            "staged" => Ok(Self::Staged),
            "commit" => Ok(Self::Commit),
            other => Err(AppError::validation(format!(
                "source must be worktree|staged|commit, got {other}"
            ))),
        }
    }
}

impl fmt::Display for DiffSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Worktree => "worktree",
            Self::Staged => "staged",
            Self::Commit => "commit",
        })
    }
}

impl FromStr for DiffSource {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// diff 请求参数。
pub struct DiffParams {
    pub source: DiffSource,
    pub from: Option<String>,
    pub to: Option<String>,
    pub paths: Vec<String>,
    pub max_file_size_bytes: u64,
    pub max_total_bytes: u64,
    pub max_output_bytes: u64,
}

/// 单文件统计。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileSummary {
    pub file: String,
    pub changes: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub binary: bool,
}

/// diff 结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiffResult {
    pub diff: String,
    pub files: Vec<FileSummary>,
    pub insertions: usize,
    pub deletions: usize,
}

/// 一侧内容: 有 blob id 时优先用 id (避免重复 write_blob), 否则用裸 bytes。
pub(super) struct Side {
    pub(super) bytes: Option<Vec<u8>>,
    pub(super) mode: Option<EntryMode>,
}

impl Side {
    pub(super) fn missing() -> Self {
        Self {
            bytes: None,
            mode: None,
        }
    }

    pub(super) fn present(bytes: Vec<u8>, mode: EntryMode) -> Self {
        Self {
            bytes: Some(bytes),
            mode: Some(mode),
        }
    }

    pub(super) fn is_present(&self) -> bool {
        self.mode.is_some()
    }
}

/// 单文件两侧变更。
pub(super) struct FileChange {
    pub(super) path: String,
    pub(super) old: Side,
    pub(super) new: Side,
}
