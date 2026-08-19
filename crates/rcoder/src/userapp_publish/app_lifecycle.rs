//! file-server project_id → rcoder app_id 的标识归一。
//!
//! ensure_app/wait_app_ready 已下沉到 app_manager（`release_runtime.rs`，activate 单接口
//! 语义的组成部分）；本模块仅保留 id 归一（多处消费）。

/// file-server project_id → rcoder app_id(强制 `app-` 前缀,已带则原样)。
pub(super) fn rcoder_app_id(app_id: &str) -> String {
    if app_id.starts_with("app-") {
        app_id.to_string()
    } else {
        format!("app-{app_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::rcoder_app_id;

    #[test]
    fn rcoder_app_id_prepends_prefix_when_missing() {
        assert_eq!(rcoder_app_id("userapp-e2e"), "app-userapp-e2e");
    }

    #[test]
    fn rcoder_app_id_is_idempotent_when_prefixed() {
        assert_eq!(rcoder_app_id("app-userapp-e2e"), "app-userapp-e2e");
    }
}
