//! code 文件操作的请求结构 (对齐 nuwax codeService 入参)。

/// 增量文件操作项 (specified-files-update / computer files-update)。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOp {
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
#[derive(Debug, Clone, serde::Deserialize)]
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
