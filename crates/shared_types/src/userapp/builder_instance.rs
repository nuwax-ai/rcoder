//! UserappBuilder dev 容器实例复合键：`(user_id, app_id)` → 单值 identifier。
//!
//! 协作模型（共享 space 成员各自为同一 app 持有独立 dev 容器）下，builder
//! 容器族不再以 app_id 单值定位，改为复合键。复合串装入
//! `ContainerCreateParams.project_id` 槽位（docker_manager 命名链以 identifier
//! 无差别派生 STS/PVC/svc/label/锁名），K8s 卷内 subPath 同用复合串；容器内
//! 挂载路径保持 `/home/user/{app_id}`（subPath 与 mountPath 不必同名），容器
//! 内契约（file-server workspace 解析、chat work_dir、PGDATA env）零变化。
//!
//! 生产 Userapp（`ServiceType::Userapp`）不复合——发布物恒 per-app 唯一。

/// 复合 identifier 总长上限。
///
/// 推导链同 [`crate::USERAPP_APP_ID_MAX_LEN`]：STS pod 的
/// controller-revision-hash label 值 = `{sts 名}-{10 位 hash}` ≤ 63 字节 →
/// STS 名 ≤ 52 → `rcoder-app-builder-`(19) + identifier ≤ 52 → identifier
/// ≤ 33。分段预算按 user_id 现网 10 位推导：app_id ≤ 22（即
/// [`crate::USERAPP_APP_ID_MAX_LEN`]）；user_id 更长时由
/// [`validate_builder_instance_id`] 的复合总长校验兜底 Fail Fast。
pub const USERAPP_BUILDER_INSTANCE_ID_MAX_LEN: usize = 33;

/// 组装 builder 实例复合 identifier：`{user_id}-{app_id}`。
///
/// **app_id 段不得含 `-`**（见 [`parse_builder_instance_id`] 的解析规则）；
/// user_id 段允许含 `-`（解析从右侧切分）。长度约束由调用方经
/// [`validate_builder_instance_id`] 显式校验（ensure 入口 Fail Fast），本函数
/// 只做结构合法性（非空 + app_id 无 `-`），保持纯组装职责。
pub fn builder_instance_id(user_id: &str, app_id: &str) -> Result<String, String> {
    if user_id.trim().is_empty() {
        return Err("builder instance user_id must not be empty".to_string());
    }
    if app_id.trim().is_empty() {
        return Err("builder instance app_id must not be empty".to_string());
    }
    if app_id.contains('-') {
        return Err(format!(
            "builder instance app_id must not contain '-': {app_id}"
        ));
    }
    Ok(format!("{user_id}-{app_id}"))
}

/// 解析 builder 实例复合 identifier → `(user_id, app_id)`。
///
/// 从**右侧**切分（`rsplit_once`）：user_id 段允许含 `-`（如 e2e 的
/// `u-idle`），app_id 段由 [`builder_instance_id`] 保证无 `-`——右切使
/// `u-idle-77` 正确解为 `("u-idle", "77")`。非复合形态（无 `-`、空段）
/// 返回 None，调用方据此区分存量纯 app_id identifier。
pub fn parse_builder_instance_id(instance_id: &str) -> Option<(&str, &str)> {
    let (user_id, app_id) = instance_id.rsplit_once('-')?;
    (!user_id.is_empty() && !app_id.is_empty()).then_some((user_id, app_id))
}

/// 校验复合 identifier 总长（K8s STS label 63 字节预算的入口 Fail Fast）。
///
/// 注册 miss（新建路径）时校验——注册命中说明历史上已建成，不受限（同
/// `USERAPP_APP_ID_MAX_LEN` 的既有纪律）。超限在 K8s 下必然
/// `FailedCreate: invalid metadata.labels`，表象含糊（ensure 500/连接超时），
/// 入口拒绝把它变成明确报错。
pub fn validate_builder_instance_id(user_id: &str, app_id: &str) -> Result<(), String> {
    let len = user_id.chars().count() + 1 + app_id.chars().count();
    if len > USERAPP_BUILDER_INSTANCE_ID_MAX_LEN {
        return Err(format!(
            "builder instance id length {len} exceeds {USERAPP_BUILDER_INSTANCE_ID_MAX_LEN} \
             (user_id + app_id; K8s StatefulSet label 63-byte limit, \
             see USERAPP_BUILDER_INSTANCE_ID_MAX_LEN)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_roundtrip() {
        for (uid, app) in [("1754545591", "77"), ("u-idle", "42"), ("6", "123")] {
            let composite = builder_instance_id(uid, app).expect("assemble");
            assert_eq!(composite, format!("{uid}-{app}"));
            assert_eq!(parse_builder_instance_id(&composite), Some((uid, app)));
        }
    }

    /// user_id 含 `-` 时右切仍正确（app_id 段无 `-` 是结构保证）。
    #[test]
    fn rsplit_disambiguates_dashed_user_id() {
        assert_eq!(
            parse_builder_instance_id("u-idle-77"),
            Some(("u-idle", "77"))
        );
    }

    #[test]
    fn rejects_app_id_with_dash() {
        assert!(builder_instance_id("u1", "e2e-app").is_err());
    }

    #[test]
    fn parse_rejects_non_composite_forms() {
        assert_eq!(parse_builder_instance_id("77"), None);
        assert_eq!(parse_builder_instance_id("-77"), None);
        assert_eq!(parse_builder_instance_id("u1-"), None);
    }

    #[test]
    fn length_budget_enforced_on_composite() {
        // uid 10 + '-' + 22 = 33 恰好上限
        let uid = "1234567890";
        let app_ok = "a".repeat(crate::USERAPP_APP_ID_MAX_LEN);
        assert!(validate_builder_instance_id(uid, &app_ok).is_ok());
        let app_over = "a".repeat(crate::USERAPP_APP_ID_MAX_LEN + 1);
        assert!(validate_builder_instance_id(uid, &app_over).is_err());
    }
}
