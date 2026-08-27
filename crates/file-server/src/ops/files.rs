//! 文件写类共享实现：files-update / upload-file(s) / generate-file / import-project。
//!
//! 壳与 handler 层测试在 handlers/computer/files/*。

use std::path::Path;

use crate::extract::AppJson as Json;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::models::FileOp;
use crate::path_safety;
use crate::service::code as code_service;
use crate::service::temp_file::TemporaryFile;

/// files-update 的 workspace 无关实现：返回写入的文件数（展示/回显归各域壳层）。
pub async fn files_update_impl(ws: &Path, mut files: Vec<FileOp>) -> Result<usize, AppError> {
    // 工作区不存在 → 创建 (对齐 nuwax computerFileUtils.updateFiles: !existsSync → mkdirSync recursive)。
    // 首次向全新 user/cId 工作区写入不应失败。
    tokio::fs::create_dir_all(ws).await?;
    // decodeURIComponent 文本内容 (对齐 nuwax safeDecodePath)
    for op in files.iter_mut() {
        if let Some(c) = op.contents.as_mut()
            && !c.is_empty()
        {
            *c = code_service::decode_uri_component(c);
        }
    }
    let count = files.len();
    // computer updateFiles: modify 用字节比较 (非 project 的行级 diff; 对齐 nuwax)
    code_service::apply_file_ops(ws, &files, code_service::ModifyStrategy::ByteCompare).await?;
    Ok(count)
}

/// upload-file 的 workspace 无关实现。
pub async fn upload_file_impl(
    ws: &Path,
    file_path: &str,
    data: TemporaryFile,
) -> Result<Json<Value>, AppError> {
    let target = path_safety::ensure_within(ws, file_path)?;
    // copy_file 内部已 create_dir_all(parent), 无需重复
    let file_size = data.size();
    crate::service::temp_file::copy_file(data.path(), &target).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully",
        "fileSize": file_size,
    })))
}

/// upload-files 的 workspace 无关实现 (单文件错误隔离: 单个失败不影响其余)。
pub async fn upload_files_impl(
    ws: &Path,
    file_paths: &[String],
    files_vec: &[(Option<String>, TemporaryFile)],
) -> Result<Json<Value>, AppError> {
    let total = file_paths.len();
    let mut success_count = 0usize;
    let mut results: Vec<Value> = Vec::new();
    for (fp, (original, data)) in file_paths.iter().zip(files_vec) {
        let target = match path_safety::ensure_within(ws, fp) {
            Ok(t) => t,
            Err(_) => {
                results.push(json!({
                    "success": false,
                    "filePath": fp,
                    "originalname": original,
                    "error": "Invalid file path",
                }));
                continue;
            }
        };
        let file_size = data.size();
        match write_file_create_parent(&target, data.path()).await {
            Ok(()) => {
                success_count += 1;
                results.push(json!({
                    "success": true,
                    "filePath": fp,
                    "originalname": original,
                    "message": "File uploaded successfully",
                    "fileSize": file_size,
                }));
            }
            Err(e) => {
                results.push(json!({
                    "success": false,
                    "filePath": fp,
                    "originalname": original,
                    "error": e.to_string(),
                }));
            }
        }
    }
    let fail_count = total - success_count;
    Ok(Json(json!({
        "success": true,
        "message": "Batch upload completed",
        "totalCount": total,
        "successCount": success_count,
        "failCount": fail_count,
        "results": results,
    })))
}

/// 写文件 (父目录自动创建); 用于 upload-files 单文件隔离错误。
async fn write_file_create_parent(target: &Path, source: &Path) -> Result<(), AppError> {
    crate::service::temp_file::copy_file(source, target)
        .await
        .map(|_| ())
}

/// generate-file 的 workspace 无关实现 (`file_name` 已 trim; 内容缺省空串)。
pub async fn generate_file_impl(
    ws: std::path::PathBuf,
    file_name: &str,
    content: String,
) -> Result<Json<Value>, AppError> {
    // 对齐 TS uploadFile.normalizeFilePath: 路径拼接时剥离前导 `/`
    // (允许 "src/foo.txt" 这类相对子路径;绝对路径会被 ensure_within 拒)。
    let target = path_safety::ensure_within(&ws, file_name.trim_start_matches('/'))?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = content.as_bytes();
    let file_size = bytes.len();
    tokio::fs::write(&target, bytes)
        .await
        .map_err(|e| AppError::system(format!("write generated file failed: {e}")))?;
    Ok(Json(json!({
        "success": true,
        "message": "File generated successfully",
        "fileName": file_name,
        "fileSize": file_size,
    })))
}

/// import-project 的 workspace 无关实现：解压合并并返回目标目录（展示/回显归各域壳层）。
pub async fn import_project_impl(
    target_dir: std::path::PathBuf,
    data: TemporaryFile,
) -> Result<String, AppError> {
    tokio::fs::create_dir_all(&target_dir).await?;
    let res = crate::service::computer_ws::import_project(&target_dir, data.path()).await?;
    Ok(res.target_dir)
}
