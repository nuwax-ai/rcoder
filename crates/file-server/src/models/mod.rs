//! wire 契约类型集中地（对齐 app_manager/models 范式）。
//!
//! 规则：凡派生 `utoipa::ToSchema` 或 `utoipa::IntoParams` 的结构体/枚举
//! 一律定义在 `models/` 下，handlers 与 service 都从这里引用——models 是
//! 最底层共享层（无 crate 内依赖），消除 DTO 散落 handlers/service 两层的
//! 问题。类型名、serde 属性、schema 名与搬迁前逐字节一致（wire 零变更，
//! openapi.rs 守卫测试锁定）。
//!
//! 分组：[`commons`]（全 crate 公共信封）/ [`code`]（跨 project/computer/
//! userapp 共享的文件操作契约）/ [`forms`]（OpenAPI-only multipart 占位）/
//! [`request`]（JSON 请求体 + Query 参数）/ [`response`]（响应载荷）。
//! 非 wire 契约的领域内部类型（TaskState、DevServerManager 等）不在此处。

pub mod code;
pub mod commons;
pub mod forms;
pub mod request;
pub mod response;

pub use code::{FileEntry, FileOp, FileOperation};
pub use commons::{BinaryFile, ErrorDetail, ErrorResponse, SuccessResponse};
pub use forms::{
    CreateWorkspaceForm, CreateWorkspaceV2Form, ImportProjectForm, InitProjectTemplateForm,
    PushProjectSkillsForm, PushSkillsForm, UploadAttachmentForm, UploadBatchFilesForm,
    UploadFileForm, UploadFilesForm, UploadProjectForm, UploadSingleFileForm,
};
pub use request::{
    AllFilesBody, BackupVersionBody, BranchCreateBody, BranchNameBody, BuildAgentBody, BuildQuery,
    CleanupBuildArtifactsBody, CommitBody, CopyProjectBody, CreateProjectBody, DeleteParams,
    DeleteWorkspaceBody, DevLogQuery, DiffBody, ExecCommandBody, ExportBody, FileContentBody,
    FileListQuery, FilesBody, FilesUpdateBody, GenerateFileBody, GetByVersionParams,
    GetContentParams, GetLogsQuery, GitLogQuery, GitQuery, GitWriteBody, InstallBody,
    KeepAliveQuery, ParseErrorBody, ResetBody, ResolveFileQuery, RevertBody, RollbackBody,
    SearchFilesQuery, SpecifiedBody, TagCreateBody, TagNameBody, TargetBody, UserCidQuery, ZipBody,
};
pub use response::{
    CreateWorkspaceResponse, HealthResponse, KilledPid, LogLine, MemoryUsage, ReadDevLogResult,
    SkillFailure, VersionResponse,
};

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// DTO 集中纪律守卫：`ToSchema` / `IntoParams` 派生只允许出现在 `models/`
    /// 目录（新增 DTO 散落回 handlers/service 会在此报红）。扫描按源码文本
    /// 匹配 token——models 内部的引用与导入不受影响。
    #[test]
    fn wire_contract_derives_live_only_in_models() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut visited = 0usize;
        visit(
            &src,
            &mut |file, content| {
                let relative = file.strip_prefix(&src).unwrap_or(file);
                if relative.components().any(|c| c.as_os_str() == "models") {
                    return;
                }
                for line in content.lines() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                        continue;
                    }
                    if line.contains("ToSchema") || line.contains("IntoParams") {
                        offenders.push(format!("{}: {}", relative.display(), line.trim()));
                    }
                }
            },
            &mut visited,
        );
        assert!(
            offenders.is_empty(),
            "wire 契约派生（ToSchema/IntoParams）只允许定义在 src/models/ 下：\n{}",
            offenders.join("\n")
        );
        assert!(visited > 80, "sanity: 至少扫描 80 个源文件, 实际 {visited}");
    }

    fn visit(dir: &Path, f: &mut dyn FnMut(&Path, &str), visited: &mut usize) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, f, visited);
                continue;
            }
            if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                *visited += 1;
                f(&path, &content);
            }
        }
    }
}
