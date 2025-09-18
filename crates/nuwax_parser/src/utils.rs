use anyhow::{Context, Result};
use serde_json;
use sha2::{Digest, Sha256};
use std::path::Path;
use uuid::Uuid;

use crate::types::{V0FileData, ProjectFile, V0ParseResult, SyncResult};
use crate::sync::V0FileSync;

/// 计算 SHA256 哈希值
pub fn calculate_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

/// 生成 V0 格式字符串
pub fn generate_v0_format(files: &[ProjectFile]) -> Result<String> {
    let mut source = String::new();

    for file in files {
        let file_type = determine_file_type(&file.path);
        let header = format!(
            "[V0_FILE]{}:file=\"{}\" isMerged=\"true\"",
            file_type,
            file.path.display()
        );
        source.push_str(&header);
        source.push('\n');
        source.push_str(&file.content);
        source.push('\n');
    }

    let v0_data = V0FileData {
        block_id: Uuid::new_v4().to_string(),
        source,
    };

    serde_json::to_string(&v0_data)
        .context("Failed to generate V0 format")
}

/// 确定文件类型
fn determine_file_type(path: &Path) -> String {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("tsx") => "typescriptreact".to_string(),
        Some("ts") => "typescript".to_string(),
        Some("jsx") => "javascriptreact".to_string(),
        Some("js") => "javascript".to_string(),
        Some("css") => "css".to_string(),
        Some("json") => "json".to_string(),
        Some("jpg") => "jpg".to_string(),
        Some("jpeg") => "jpg".to_string(),
        Some("png") => "png".to_string(),
        Some("gif") => "gif".to_string(),
        Some("md") => "markdown".to_string(),
        Some("txt") => "plaintext".to_string(),
        Some("rs") => "rust".to_string(),
        Some("toml") => "toml".to_string(),
        _ => "plaintext".to_string(),
    }
}

/// 便捷函数：从项目路径直接创建 V0ParseResult
///
/// # 参数
/// * `project_path` - 项目根目录路径
/// * `ignore_hidden` - 是否忽略隐藏目录（如 .git, .claude 等）
///
/// # 返回值
/// 返回包含所有项目文件的 V0ParseResult
///
/// # 示例
/// ```rust
/// use nuwax_parser::project_to_v0_result;
/// use std::path::PathBuf;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let project_path = PathBuf::from("./my-project");
///     let v0_result = project_to_v0_result(&project_path, true).await?;
///     println!("Found {} files", v0_result.files.len());
///     Ok(())
/// }
/// ```
pub async fn project_to_v0_result<P: AsRef<Path>>(
    project_path: P,
    ignore_hidden: bool,
) -> Result<V0ParseResult> {
    let sync = V0FileSync::new(project_path);
    let project_files = sync.read_project_files(ignore_hidden).await?;
    let v0_json = generate_v0_format(&project_files)?;

    let v0_data = V0FileData::from_json(&v0_json)?;
    v0_data.parse_source()
}

/// 便捷函数：从项目路径创建 V0 格式的 JSON 字符串
///
/// # 参数
/// * `project_path` - 项目根目录路径
/// * `ignore_hidden` - 是否忽略隐藏目录
///
/// # 返回值
/// 返回 V0 格式的 JSON 字符串，可以直接发送给前端
pub async fn project_to_v0_json<P: AsRef<Path>>(
    project_path: P,
    ignore_hidden: bool,
) -> Result<String> {
    let sync = V0FileSync::new(project_path);
    let project_files = sync.read_project_files(ignore_hidden).await?;
    generate_v0_format(&project_files)
}


/// 完全同步 V0ParseResult 到文件系统（包括删除多余文件）
///
/// 这个函数会确保后端文件系统与前端 V0ParseResult 完全一致：
/// 1. 更新或写入 V0ParseResult 中存在的文件
/// 2. 删除后端存在但 V0ParseResult 中不存在的文件
/// 3. 清理空目录
///
/// # 参数
/// * `project_path` - 项目根目录路径
/// * `v0_result` - 要同步的 V0ParseResult
///
/// # 返回值
/// 返回包含写入和删除文件列表的 SyncResult
///
/// # 示例
/// ```rust
/// use nuwax_parser::{project_to_v0_result, sync_v0_result_to_project};
/// use std::path::PathBuf;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let project_path = PathBuf::from("./my-project");
///     let frontend_result = get_frontend_v0_result(); // 从前端获取
///
///     // 完全同步到后端（包括删除多余的文件）
///     let sync_result = sync_v0_result_to_project(&project_path, &frontend_result).await?;
///
///     println!("写入 {} 个文件", sync_result.written_files.len());
///     println!("删除 {} 个文件", sync_result.deleted_files.len());
///     Ok(())
/// }
/// ```
pub async fn sync_v0_result_to_project<P: AsRef<Path>>(
    project_path: P,
    v0_result: &V0ParseResult,
) -> Result<SyncResult> {
    let sync = V0FileSync::new(project_path);
    sync.sync_v0_result(v0_result).await
}