use anyhow::{Result, anyhow};
use shared_types::ModelProviderConfig;

/// API key placeholder used when a proxy injects the real credential.
pub const API_KEY_PLACEHOLDER: &str = "PROXY_MANAGED_KEY";

/// Resolved model values used while rendering an agent subprocess environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelEnv {
    pub api_key: String,
    pub base_url: String,
    pub default_model: String,
    pub provider_name: String,
    /// When true, existing sensitive env vars from config overrides must be
    /// replaced with the resolved proxy-safe values.
    pub override_existing_sensitive_env: bool,
}

/// Runtime strategy for model credentials and endpoint values.
pub trait ModelRuntimeEnvResolver: Send + Sync {
    fn resolve(
        &self,
        provider: &ModelProviderConfig,
        service_uuid: Option<&str>,
    ) -> Result<ResolvedModelEnv>;
}

/// Direct mode: pass through the provider values unchanged.
#[derive(Debug, Default)]
pub struct DirectModelRuntimeEnvResolver;

impl ModelRuntimeEnvResolver for DirectModelRuntimeEnvResolver {
    fn resolve(
        &self,
        provider: &ModelProviderConfig,
        _service_uuid: Option<&str>,
    ) -> Result<ResolvedModelEnv> {
        Ok(ResolvedModelEnv {
            api_key: provider.api_key.clone(),
            base_url: provider.base_url.clone(),
            default_model: provider.default_model.clone(),
            provider_name: provider.name.clone(),
            override_existing_sensitive_env: false,
        })
    }
}

/// Proxy mode: hide real credentials from the child process and route traffic
/// through the configured local proxy service.
#[derive(Debug, Clone)]
pub struct ProxyModelRuntimeEnvResolver {
    proxy_base_url_template: String,
}

impl ProxyModelRuntimeEnvResolver {
    pub fn new(proxy_base_url_template: impl Into<String>) -> Self {
        Self {
            proxy_base_url_template: proxy_base_url_template.into(),
        }
    }
}

impl ModelRuntimeEnvResolver for ProxyModelRuntimeEnvResolver {
    fn resolve(
        &self,
        provider: &ModelProviderConfig,
        service_uuid: Option<&str>,
    ) -> Result<ResolvedModelEnv> {
        let service_uuid = service_uuid
            .filter(|uuid| !uuid.is_empty())
            .ok_or_else(|| anyhow!("proxy model runtime requires service_uuid"))?;

        Ok(ResolvedModelEnv {
            api_key: API_KEY_PLACEHOLDER.to_string(),
            base_url: self
                .proxy_base_url_template
                .replace("{SERVICE_UUID}", service_uuid),
            default_model: provider.default_model.clone(),
            provider_name: provider.name.clone(),
            override_existing_sensitive_env: true,
        })
    }
}

pub fn direct_model_runtime_env_resolver() -> std::sync::Arc<dyn ModelRuntimeEnvResolver> {
    std::sync::Arc::new(DirectModelRuntimeEnvResolver)
}
