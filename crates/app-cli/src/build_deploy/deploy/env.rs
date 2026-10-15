use super::*;

/// 环境变量入口：`APP_DEPLOY_URL` 未设置/为空 → 跳过（走卷上已有 code 的兼容形态）。
pub async fn run_from_env(workspace: &Path) -> Result<()> {
    let Some(url) = env_non_empty("APP_DEPLOY_URL") else {
        return Ok(());
    };
    let Some(release_id) = env_non_empty("APP_RELEASE_ID") else {
        bail!("APP_DEPLOY_URL is set but APP_RELEASE_ID is missing — deploy env contract violated");
    };
    let sha256 = env_non_empty("APP_DEPLOY_SHA256").map(|s| s.to_ascii_lowercase());
    if let Some(sha) = &sha256
        && (sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("APP_DEPLOY_SHA256 must be 64 hex characters, got '{sha}'");
    }
    deploy(workspace, &url, &release_id, sha256.as_deref()).await
}

/// 是否请求了部署（main 用于决定是否先占位 liveness 端口）。
pub fn deploy_requested() -> bool {
    env_non_empty("APP_DEPLOY_URL").is_some()
}

/// 从 env 三元组解析部署请求（server 启动时的首次部署判定）。
/// 契约与 [`run_from_env`] 一致：URL 在而 RELEASE_ID 缺 = 契约违背（Err）。
pub fn request_from_env() -> Result<crate::server::DeployRequest> {
    let Some(url) = env_non_empty("APP_DEPLOY_URL") else {
        bail!("APP_DEPLOY_URL not set");
    };
    let Some(release_id) = env_non_empty("APP_RELEASE_ID") else {
        bail!("APP_DEPLOY_URL is set but APP_RELEASE_ID is missing — deploy env contract violated");
    };
    let sha256 = env_non_empty("APP_DEPLOY_SHA256").map(|s| s.to_ascii_lowercase());
    if let Some(sha) = &sha256
        && (sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("APP_DEPLOY_SHA256 must be 64 hex characters, got '{sha}'");
    }
    Ok(crate::server::DeployRequest {
        runtime_operation_id: None,
        url,
        release_id,
        sha256,
        local_path: None,
        execution_target: None,

        run_pg: None,
    })
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
