//! computer 工作区装配 handlers: create-workspace / push-skills-to-workspace /
//! init-project-template。
//!
//! 拆分: [`create`] (create-workspace v1/v2) / [`push_skills`] (push-skills v1/v2) /
//! [`init_template`] (init-project-template)。本 mod.rs 仅做模块声明 + 共享校验辅助。

pub(crate) mod create;
pub(crate) mod init_template;
pub(crate) mod push_skills;

use garde::Validate;

use crate::error::AppError;

/// multipart 必填文本字段 (userId/cId; 提取后构造 + garde 校验)。
#[derive(garde::Validate)]
struct WorkspaceFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    user_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    cid: Option<String>,
}

/// 校验 userId/cId 并取数 (对齐 TS "userId/cId is required" 语义)。
pub(super) fn require_workspace_fields(
    user_id: Option<String>,
    cid: Option<String>,
) -> Result<(String, String), AppError> {
    let fields = WorkspaceFields { user_id, cid };
    fields.validate().map_err(crate::error::from_garde)?;
    // 校验已保证必填; 取数 (失败逻辑不可达, 防御性处理)
    Ok((
        fields
            .user_id
            .ok_or_else(|| AppError::system("user_id missing after garde validation"))?,
        fields
            .cid
            .ok_or_else(|| AppError::system("c_id missing after garde validation"))?,
    ))
}
