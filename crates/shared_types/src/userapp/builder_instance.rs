//! UserappBuilder dev 容器 identifier：纯 app_id（移除用户绑定）。
//!
//! 协作模型变更（2026-09-15 移除用户绑定）：UserApp 只按 `(stage, app_id)`
//! 定位，同 app 全使用者共享 dev 容器/PVC/工作区。identifier 恒等于
//! `app_id`，不再有复合键 `{user_id}-{app_id}`，也不做 rsplit 猜用户。
//!
//! 生产 Userapp（`ServiceType::Userapp`）同样纯 app_id——发布物恒 per-app 唯一。
//!
//! 历史：复合键（`{user_id}-{app_id}`）曾短暂引入用于多用户协作独立容器，
//! 已按 spec/userapp-remove-user-id-binding 移除；旧复合名容器不迁移不接管。

/// 校验 builder identifier（= 纯 app_id）长度。
///
/// 预算推导链：STS pod controller-revision-hash label 值 =
/// `{sts 名}-{10 位 hash}` ≤ 63 字节 → STS 名 ≤ 52 →
/// `rcoder-app-builder-`(19) + app_id ≤ 52 → app_id ≤ 33。当前公共常量
/// [`crate::USERAPP_APP_ID_MAX_LEN`] = 22（更保守），沿用统一入口。
pub fn validate_builder_app_id(app_id: &str) -> Result<(), String> {
    crate::validate_identifier(app_id, "app_id")
        .map_err(|e| format!("builder app_id invalid: {e}"))?;
    let len = app_id.chars().count();
    if len > crate::USERAPP_APP_ID_MAX_LEN {
        return Err(format!(
            "builder app_id length {len} exceeds {} (K8s StatefulSet label 63-byte limit, \
             see USERAPP_APP_ID_MAX_LEN)",
            crate::USERAPP_APP_ID_MAX_LEN
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_app_id() {
        assert!(validate_builder_app_id("79").is_ok());
        assert!(validate_builder_app_id("app123").is_ok());
    }

    #[test]
    fn rejects_empty_and_oversize() {
        assert!(validate_builder_app_id("").is_err());
        assert!(validate_builder_app_id(" ").is_err());
        let over = "a".repeat(crate::USERAPP_APP_ID_MAX_LEN + 1);
        assert!(validate_builder_app_id(&over).is_err());
    }
}
