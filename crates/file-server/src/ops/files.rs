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

/// upload-file 业务核心（类型化返回；各域响应拼装在本文件 `upload_file_impl`
/// 与 file-server-userapp 壳层——键风格分歧点在拼装层，编排逻辑单点在此）。
pub struct UploadedFile {
    pub file_size: u64,
}

/// upload-file 的 workspace 无关核心。
pub async fn upload_file_core(
    ws: &Path,
    file_path: &str,
    data: TemporaryFile,
) -> Result<UploadedFile, AppError> {
    let target = path_safety::ensure_within(ws, file_path)?;
    // copy_file 内部已 create_dir_all(parent), 无需重复
    let file_size = data.size();
    crate::service::temp_file::copy_file(data.path(), &target).await?;
    Ok(UploadedFile { file_size })
}

/// upload-file 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn upload_file_impl(
    ws: &Path,
    file_path: &str,
    data: TemporaryFile,
) -> Result<Json<Value>, AppError> {
    let r = upload_file_core(ws, file_path, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully",
        "fileSize": r.file_size,
    })))
}

/// upload-files 单文件结果（成功/失败两态；失败含 error 文案）。
pub enum BatchUploadItem {
    Ok {
        file_path: String,
        original: Option<String>,
        file_size: u64,
    },
    Err {
        file_path: String,
        original: Option<String>,
        error: String,
    },
}

/// upload-files 批量结果。
pub struct BatchUploadOutcome {
    pub total: usize,
    pub success_count: usize,
    pub results: Vec<BatchUploadItem>,
}

/// upload-files 的 workspace 无关核心 (单文件错误隔离: 单个失败不影响其余)。
pub async fn upload_files_core(
    ws: &Path,
    file_paths: &[String],
    files_vec: &[(Option<String>, TemporaryFile)],
) -> Result<BatchUploadOutcome, AppError> {
    let total = file_paths.len();
    let mut success_count = 0usize;
    let mut results: Vec<BatchUploadItem> = Vec::new();
    for (fp, (original, data)) in file_paths.iter().zip(files_vec) {
        let target = match path_safety::ensure_within(ws, fp) {
            Ok(t) => t,
            Err(_) => {
                results.push(BatchUploadItem::Err {
                    file_path: fp.clone(),
                    original: original.clone(),
                    error: "Invalid file path".to_string(),
                });
                continue;
            }
        };
        let file_size = data.size();
        match write_file_create_parent(&target, data.path()).await {
            Ok(()) => {
                success_count += 1;
                results.push(BatchUploadItem::Ok {
                    file_path: fp.clone(),
                    original: original.clone(),
                    file_size,
                });
            }
            Err(e) => {
                results.push(BatchUploadItem::Err {
                    file_path: fp.clone(),
                    original: original.clone(),
                    error: e.to_string(),
                });
            }
        }
    }
    Ok(BatchUploadOutcome {
        total,
        success_count,
        results,
    })
}

/// upload-files 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn upload_files_impl(
    ws: &Path,
    file_paths: &[String],
    files_vec: &[(Option<String>, TemporaryFile)],
) -> Result<Json<Value>, AppError> {
    let r = upload_files_core(ws, file_paths, files_vec).await?;
    let results: Vec<Value> = r
        .results
        .into_iter()
        .map(|item| match item {
            BatchUploadItem::Ok {
                file_path,
                original,
                file_size,
            } => json!({
                "success": true,
                "filePath": file_path,
                "originalname": original,
                "message": "File uploaded successfully",
                "fileSize": file_size,
            }),
            BatchUploadItem::Err {
                file_path,
                original,
                error,
            } => json!({
                "success": false,
                "filePath": file_path,
                "originalname": original,
                "error": error,
            }),
        })
        .collect();
    Ok(Json(json!({
        "success": true,
        "message": "Batch upload completed",
        "totalCount": r.total,
        "successCount": r.success_count,
        "failCount": r.total - r.success_count,
        "results": results,
    })))
}

/// 写文件 (父目录自动创建); 用于 upload-files 单文件隔离错误。
async fn write_file_create_parent(target: &Path, source: &Path) -> Result<(), AppError> {
    crate::service::temp_file::copy_file(source, target)
        .await
        .map(|_| ())
}

/// generate-file 业务核心的返回（file_name 为入参原样回显）。
pub struct GeneratedFile {
    pub file_name: String,
    pub file_size: usize,
}

/// generate-file 的 workspace 无关核心 (`file_name` 已 trim; 内容缺省空串)。
pub async fn generate_file_core(
    ws: std::path::PathBuf,
    file_name: &str,
    content: String,
) -> Result<GeneratedFile, AppError> {
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
    Ok(GeneratedFile {
        file_name: file_name.to_string(),
        file_size,
    })
}

/// generate-file 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn generate_file_impl(
    ws: std::path::PathBuf,
    file_name: &str,
    content: String,
) -> Result<Json<Value>, AppError> {
    let r = generate_file_core(ws, file_name, content).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File generated successfully",
        "fileName": r.file_name,
        "fileSize": r.file_size,
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
