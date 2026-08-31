//! Userapp 容器 env 的平台保留变量治理。
//!
//! RBD 卷形态下 rcoder 不再读卷上 release.lock.toml 注入运行身份变量
//! （`RCODER_PINGAP_VERSION/COMMIT/RUNTIME_IMAGE_DIGEST`——锁就在容器内
//! `/app/code`，app-cli 自己读；其兼容性 warn 检查随 env 缺失自然失效，本就
//! warn-only）。保留两件事：**防伪造**（用户显式提交保留键 → 400）与
//! **防误伤**（live 读回的历史注入值剥离后再校验）。

use std::collections::HashMap;

use crate::error::{AppOperationError, AppResult};

const RUNTIME_IDENTITY_KEYS: [&str; 3] = [
    "RCODER_PINGAP_VERSION",
    "RCODER_PINGAP_COMMIT",
    "RCODER_RUNTIME_IMAGE_DIGEST",
];

/// 从 live 集群读回的 env 中剥离系统保留变量（update 部分更新回退路径专用）。
///
/// 集群里的 env 可能携带历史版本注入的身份变量，原样进入校验会被误判为
/// "用户提交了保留变量"而 400；剥离后用户显式提交保留变量仍被拒绝，防伪造
/// 语义不变。
pub(crate) fn strip_release_identity(env: &mut HashMap<String, String>) {
    for key in RUNTIME_IDENTITY_KEYS {
        env.remove(key);
    }
}

/// 校验用户提交的 env 不含平台保留变量（调用方：create/update/部署 env 组装）。
pub(crate) fn ensure_no_reserved_env(env: &HashMap<String, String>) -> AppResult<()> {
    for key in RUNTIME_IDENTITY_KEYS {
        if env.contains_key(key) {
            return Err(AppOperationError::Validation(format!(
                "environment variable {key} is reserved by the platform"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_only_reserved_keys() {
        let mut env = HashMap::from([
            ("SPRING_PROFILES".to_owned(), "prod".to_owned()),
            ("RCODER_PINGAP_VERSION".to_owned(), "stale".to_owned()),
            ("RCODER_PINGAP_COMMIT".to_owned(), "stale".to_owned()),
            ("RCODER_RUNTIME_IMAGE_DIGEST".to_owned(), "stale".to_owned()),
        ]);
        strip_release_identity(&mut env);
        assert_eq!(env.len(), 1, "business env kept, reserved stripped");
        assert_eq!(env.get("SPRING_PROFILES").map(String::as_str), Some("prod"));
    }

    #[test]
    fn rejects_reserved_environment_override() {
        let env = HashMap::from([("RCODER_PINGAP_VERSION".to_owned(), "user-value".to_owned())]);
        let error = ensure_no_reserved_env(&env).expect_err("reserved value must fail");
        assert!(error.to_string().contains("reserved"), "{error}");
    }

    #[test]
    fn business_env_passes() {
        let env = HashMap::from([("SPRING_PROFILES".to_owned(), "prod".to_owned())]);
        assert!(ensure_no_reserved_env(&env).is_ok());
    }
}
