//! wire 契约类型集中地（对齐 app_manager/models 范式，与 file-server 同构）。
//!
//! 规则：凡派生 `utoipa::ToSchema` 或 `utoipa::IntoParams` 的结构体/枚举
//! 一律定义在 `models/` 下，handlers 与 service 都从这里引用。类型名、
//! serde 属性、schema 名与搬迁前逐字节一致（wire 零变更，routes.rs 守卫
//! 测试锁定）。
//!
//! 分组：[`task`]（构建任务域共享：BuildTaskKind/Status/Snapshot）/
//! [`request`]（请求体 + Query + multipart 占位）/ [`response`]（HttpResult
//! .data 载荷）。跨 crate 共享类型（KilledPid/BinaryFile/FileOp）从
//! `file_server::models` 引用（洋葱模型单向依赖，单一事实源在 file-server）。

pub mod request;
pub mod response;
pub mod task;

pub use request::{
    AppFilesClearBody, AppFilesDeleteBody, AppFilesListParams, AppFilesUploadForm,
    AppFilesUploadFromUrlBody, BuildUserAppBody, DevLogsQuery, DevOpBody, ImportProjectBody,
    StaticQuery, StreamQuery, TaskLogsQuery, UserappDownloadQuery, UserappEnsureWorkspaceBody,
    UserappExecCommandBody, UserappFileListQuery, UserappFilesUpdateBody, UserappGenerateFileBody,
    UserappGetLogsQuery, UserappImportProjectForm, UserappInitTemplateForm, UserappInstallBody,
    UserappPushSkillsForm, UserappResolveFileQuery, UserappSearchFilesQuery, UserappUploadFileForm,
    UserappUploadFilesForm, UserappZipBody,
};
pub use response::{
    BuildCreatedData, CancelData, ConfirmData, DetectData, DetectionResult, UserappDevList,
    UserappDevProcess, UserappDevStopped, UserappDevTaskCreated, UserappEnsureWorkspaceData,
};
pub use task::{BuildTaskId, BuildTaskKind, BuildTaskSnapshot, BuildTaskStatus};

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// DTO 集中纪律守卫：`ToSchema` / `IntoParams` 派生只允许出现在 `models/`
    /// 目录（新增 DTO 散落回 handlers/service 会在此报红）。
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
        assert!(visited > 8, "sanity: 至少扫描 8 个源文件, 实际 {visited}");
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
