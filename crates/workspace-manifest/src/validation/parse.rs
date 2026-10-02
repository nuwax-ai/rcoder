//! TOML 解析入口与 fast-fail 兼容校验。

use super::project::{collect_workspace_issues, validate_project_at};
use crate::{ManifestError, ProjectManifest, ValidationIssue, WorkspaceManifest};

/// 仅反序列化 project manifest（不做校验）——供收集式校验入口把"语法错误"
/// 与"语义问题"分开呈现，避免对已四段式渲染的校验文本二次包装。
pub fn parse_project_toml(content: &str) -> Result<ProjectManifest, ManifestError> {
    toml::from_str(content).map_err(|error| ManifestError::Parse(error.to_string()))
}

/// 仅反序列化 workspace manifest，供收集式检查独立报告语义问题。
pub fn parse_workspace_toml(content: &str) -> Result<WorkspaceManifest, ManifestError> {
    toml::from_str(content).map_err(|error| ManifestError::Parse(error.to_string()))
}

/// 只读检查的解析入口：保留错误行列，但不输出原始配置或字段值。
pub fn parse_workspace_for_inspection(
    content: &str,
    file: &str,
) -> Result<WorkspaceManifest, ValidationIssue> {
    parse_for_inspection(content, file)
}

pub(crate) fn parse_project_for_inspection(
    content: &str,
    file: &str,
) -> Result<ProjectManifest, ValidationIssue> {
    parse_for_inspection(content, file)
}

fn parse_for_inspection<T: serde::de::DeserializeOwned>(
    content: &str,
    file: &str,
) -> Result<T, ValidationIssue> {
    toml::from_str(content).map_err(|error| {
        let mut message = "invalid TOML syntax, manifest field, or value".to_owned();
        if let Some(span) = error.span() {
            let (mut line, mut column) = (1, 1);
            for (_, character) in content.char_indices().take_while(|(index, _)| *index < span.start) {
                if character == '\n' {
                    line += 1;
                    column = 1;
                } else {
                    column += 1;
                }
            }
            message.push_str(&format!(" at line {line}, column {column}"));
        }
        ValidationIssue::new(message).at_file(file).with_hint(
            "check the TOML syntax and manifest field names/types at the reported position, then re-run",
        )
    })
}

pub fn parse_workspace(content: &str) -> Result<WorkspaceManifest, ManifestError> {
    let manifest = parse_workspace_toml(content)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_inspection_keeps_parse_and_semantic_validation_separate() {
        let content = "schema_version = 2\n[workspace]\nname = ''\n";
        let manifest = parse_workspace_toml(content).unwrap();
        assert_eq!(
            collect_workspace_issues(&manifest, "workspace.manifest.toml").len(),
            2
        );
        assert!(parse_workspace(content).is_err());
        assert!(parse_workspace_for_inspection(content, "workspace.manifest.toml").is_ok());
    }

    #[test]
    fn workspace_parse_diagnostics_do_not_disclose_unknown_fields_or_secret_values() {
        for content in [
            "schema_version = 1\n[workspace]\nname = 'safe'\nSECRET_FIELD = 'DO_NOT_DISCLOSE_SECRET'\n",
            "schema_version = 'DO_NOT_DISCLOSE_SECRET'\n[workspace]\nname = 'safe'\n",
            "schema_version = 1\n[workspace]\nname = 'DO_NOT_DISCLOSE_SECRET\n",
        ] {
            let issue =
                parse_workspace_for_inspection(content, "workspace.manifest.toml").unwrap_err();
            assert_eq!(issue.file.as_deref(), Some("workspace.manifest.toml"));
            let rendered = issue.to_string();
            assert!(rendered.contains("line "));
            assert!(rendered.contains("column "));
            assert!(!rendered.contains("DO_NOT_DISCLOSE_SECRET"));
            assert!(!rendered.contains("SECRET_FIELD"));
            assert!(parse_workspace_toml(content).is_err());
        }
    }
}
