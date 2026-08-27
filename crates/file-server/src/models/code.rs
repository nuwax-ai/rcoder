//! code 文件操作契约（原 service/code/types.rs）：跨 project / computer /
//! userapp(file-server-userapp) 三域共享的文件写操作请求组件。
//!
//! `FileOperation` 的解析/展示逻辑随类型迁移——它是 wire 枚举（schema 断言
//! 锁定 `FileOp→FileOperation` 的 $ref 结构），service 侧继续引用。

use std::fmt;
use std::str::FromStr;

/// 增量文件操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileOperation {
    Create,
    Delete,
    Rename,
    Modify,
}

impl fmt::Display for FileOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Rename => "rename",
            Self::Modify => "modify",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFileOperationError {
    operation: String,
}

impl fmt::Display for ParseFileOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unsupported file operation: {}", self.operation)
    }
}

impl std::error::Error for ParseFileOperationError {}

impl FromStr for FileOperation {
    type Err = ParseFileOperationError;

    fn from_str(operation: &str) -> Result<Self, Self::Err> {
        match operation.to_ascii_lowercase().as_str() {
            "create" => Ok(Self::Create),
            "delete" => Ok(Self::Delete),
            "rename" => Ok(Self::Rename),
            "modify" => Ok(Self::Modify),
            _ => Err(ParseFileOperationError {
                operation: operation.to_string(),
            }),
        }
    }
}

/// 增量文件操作项 (specified-files-update / computer files-update)。
#[derive(Debug, Clone, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileOp {
    #[schema(value_type = FileOperation)]
    pub operation: String,
    pub name: String,
    #[serde(default)]
    pub is_dir: Option<bool>,
    #[serde(default)]
    pub contents: Option<String>,
    #[serde(default)]
    pub rename_from: Option<String>,
}

/// 全量文件项 (all-files-update)。
#[derive(Debug, Clone, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    #[serde(default)]
    pub contents: Option<String>,
    #[serde(default)]
    pub binary: Option<bool>,
    #[serde(default)]
    pub size_exceeded: Option<bool>,
    #[serde(default)]
    pub is_dir: Option<bool>,
    #[serde(default)]
    pub rename_from: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_operation_parses_case_insensitively_and_displays_protocol_value() {
        assert_eq!("CREATE".parse::<FileOperation>(), Ok(FileOperation::Create));
        assert_eq!(FileOperation::Rename.to_string(), "rename");
        assert!("copy".parse::<FileOperation>().is_err());
    }
}
