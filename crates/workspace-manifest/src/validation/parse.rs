//! TOML 解析入口与 fast-fail 兼容校验。

use super::project::{collect_workspace_issues, validate_project_at};
use crate::{ManifestError, ProjectManifest, WorkspaceManifest};

/// 仅反序列化 project manifest（不做校验）——供收集式校验入口把"语法错误"
/// 与"语义问题"分开呈现，避免对已四段式渲染的校验文本二次包装。
pub fn parse_project_toml(content: &str) -> Result<ProjectManifest, ManifestError> {
    toml::from_str(content).map_err(|error| ManifestError::Parse(error.to_string()))
}

pub fn parse_workspace(content: &str) -> Result<WorkspaceManifest, ManifestError> {
    let manifest: WorkspaceManifest =
        toml::from_str(content).map_err(|error| ManifestError::Parse(error.to_string()))?;
    validate_workspace(&manifest)?;
    Ok(manifest)
}

pub fn parse_project(content: &str) -> Result<ProjectManifest, ManifestError> {
    let manifest: ProjectManifest =
        toml::from_str(content).map_err(|error| ManifestError::Parse(error.to_string()))?;
    validate_project(&manifest)?;
    Ok(manifest)
}

pub fn validate_workspace(manifest: &WorkspaceManifest) -> Result<(), ManifestError> {
    let issues = collect_workspace_issues(manifest, "workspace.manifest.toml");
    issues
        .into_iter()
        .next()
        .map(|issue| ManifestError::Validation(issue.to_string()))
        .map_or(Ok(()), Err)
}

pub fn validate_project(manifest: &ProjectManifest) -> Result<(), ManifestError> {
    validate_project_at(manifest, "")
        .into_iter()
        .next()
        .map(|issue| ManifestError::Validation(issue.to_string()))
        .map_or(Ok(()), Err)
}
