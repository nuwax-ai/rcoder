use std::collections::HashMap;
use std::path::Path;

use crate::error::{AppOperationError, AppResult};

const RUNTIME_IDENTITY_KEYS: [&str; 3] = [
    "RCODER_PINGAP_VERSION",
    "RCODER_PINGAP_COMMIT",
    "RCODER_RUNTIME_IMAGE_DIGEST",
];

/// 从 live 集群读回的 env 中剥离系统保留变量（update 部分更新回退路径专用）。
///
/// 集群里的 env 携带 create 时注入的身份变量，原样进入 [`inject_release_identity`]
/// 会被误判为"用户提交了保留变量"而 400；剥离后由 inject 从当前发布锁重新注入权威值
/// （用户显式提交保留变量仍被拒绝，防伪造语义不变）。
pub(crate) fn strip_release_identity(env: &mut HashMap<String, String>) {
    for key in RUNTIME_IDENTITY_KEYS {
        env.remove(key);
    }
}

/// Load the immutable build identity from the active release lock and inject it
/// into the UserApp container. Callers may not provide these reserved values.
pub(crate) async fn inject_release_identity(
    app_dir: &Path,
    mut env: HashMap<String, String>,
) -> AppResult<HashMap<String, String>> {
    for key in RUNTIME_IDENTITY_KEYS {
        if env.contains_key(key) {
            return Err(AppOperationError::Validation(format!(
                "environment variable {key} is reserved and comes from release.lock.toml"
            )));
        }
    }

    let lock_path = app_dir.join("code").join("release.lock.toml");
    let content = tokio::fs::read_to_string(&lock_path)
        .await
        .map_err(|error| {
            AppOperationError::InvalidState(format!(
                "active release lock is required before creating the app container ({}): {error}",
                lock_path.display()
            ))
        })?;
    // 经 workspace_manifest::load_release_lock 读取：版本感知 + 迁移 dispatch（与 app-cli
    // 同一入口），顺带补上此前 app_manager 裸 toml::from_str 完全不校验 schema_version 的缺口。
    let lock = workspace_manifest::load_release_lock(&content).map_err(|error| {
        AppOperationError::Validation(format!(
            "invalid active release lock {}: {error}",
            lock_path.display()
        ))
    })?;

    let values = [
        ("RCODER_PINGAP_VERSION", lock.pingap.version),
        ("RCODER_PINGAP_COMMIT", lock.pingap.commit),
        ("RCODER_RUNTIME_IMAGE_DIGEST", lock.runtime_image_digest),
    ];
    for (key, value) in values {
        if value.trim().is_empty() {
            return Err(AppOperationError::Validation(format!(
                "active release lock has empty runtime identity: {key}"
            )));
        }
        env.insert(key.to_owned(), value);
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_lock() -> &'static str {
        r#"
schema_version = 1
release_id = "release-1"
workspace_name = "smoke"
minimum_app_cli_version = "0.1.0"
runtime_image_digest = "registry.example/app-runtime:0.1.140"

[pingap]
mode = "managed"
version = "0.13.7"
commit = "abc123"

[[services]]
service_id = "backend"
name = "Backend"
dir = "backend"
type = "go"
kind = "web"
enabled = true
port = 4100
logs = []

[services.run]
command = ["./server"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]

[services.proxy]
path = "/"
strip_prefix = false
plugins = []
upstream_includes = []

[services.env]
"#
    }

    #[tokio::test]
    async fn injects_identity_from_release_lock() {
        let root = tempfile::tempdir().expect("tempdir");
        let code = root.path().join("code");
        tokio::fs::create_dir_all(&code).await.expect("create code");
        tokio::fs::write(code.join("release.lock.toml"), release_lock())
            .await
            .expect("write lock");

        let env = inject_release_identity(root.path(), HashMap::new())
            .await
            .expect("inject identity");
        assert_eq!(
            env.get("RCODER_RUNTIME_IMAGE_DIGEST").map(String::as_str),
            Some("registry.example/app-runtime:0.1.140")
        );
        assert_eq!(
            env.get("RCODER_PINGAP_COMMIT").map(String::as_str),
            Some("abc123")
        );
    }

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

    #[tokio::test]
    async fn rejects_reserved_environment_override() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut env = HashMap::new();
        env.insert("RCODER_PINGAP_VERSION".to_owned(), "user-value".to_owned());
        let error = inject_release_identity(root.path(), env)
            .await
            .expect_err("reserved value must fail");
        assert!(error.to_string().contains("reserved"));
    }
}
