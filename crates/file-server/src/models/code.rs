//! code 文件操作契约：跨 project / computer /
//! userapp(file-server-userapp) 三域共享的文件写操作请求组件。
//!
//! `FileOperation` 带 FromStr/Display 解析逻辑——它是 wire 枚举（schema 断言
//! 锁定 `FileOp→FileOperation` 的 $ref 结构），service 侧直接引用。

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
    /// 操作类型：create / delete / rename / modify
    #[schema(value_type = FileOperation)]
    pub operation: String,
    /// 目标文件相对路径（rename 时为新路径）
    pub name: String,
    /// 是否目录（缺省 false）
    #[serde(default)]
    pub is_dir: Option<bool>,
    /// 文本内容（create/modify 时写入；服务端做 URL 解码）
    #[serde(default)]
    pub contents: Option<String>,
    /// rename 的源路径（操作为 rename 时必填）
    #[serde(default)]
    pub rename_from: Option<String>,
}

/// 全量文件项 (all-files-update)。
#[derive(Debug, Clone, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// 文件相对路径
    pub name: String,
    /// 文本内容（全量覆盖写入；服务端做 URL 解码）
    #[serde(default)]
    pub contents: Option<String>,
    /// 二进制内容标记（对齐 nuwax 字段位）
    #[serde(default)]
    pub binary: Option<bool>,
    /// 内容超过单文件上限时置 true（此时内容被丢弃）
    #[serde(default)]
    pub size_exceeded: Option<bool>,
    /// 是否目录
    #[serde(default)]
    pub is_dir: Option<bool>,
    /// rename 的源路径（同名字段重命名场景）
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
