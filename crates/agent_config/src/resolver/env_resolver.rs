//! Environment variable resolver.

use std::collections::HashMap;

use super::{ProjectContext, ResolutionContext};
use crate::types::agent_config::AgentConfig;
use crate::types::prompt_config::{SystemPromptConfig, UserPromptConfig};

/// Environment variable resolver
pub struct EnvironmentVariableResolver {
    /// Standard mappings for common variables
    mappings: HashMap<String, String>,
}

impl EnvironmentVariableResolver {
    /// Create a new resolver with standard mappings
    pub fn new() -> Self {
        let mut mappings = HashMap::new();

        // ModelProvider related mappings
        mappings.insert(
            "MODEL_PROVIDER_ID".to_string(),
            "model_provider.id".to_string(),
        );
        mappings.insert(
            "MODEL_PROVIDER_NAME".to_string(),
            "model_provider.name".to_string(),
        );
        mappings.insert(
            "MODEL_PROVIDER_API_KEY".to_string(),
            "model_provider.api_key".to_string(),
        );
        mappings.insert(
            "MODEL_PROVIDER_DEFAULT_MODEL".to_string(),
            "model_provider.default_model".to_string(),
        );
        mappings.insert(
            "MODEL_PROVIDER_BASE_URL".to_string(),
            "model_provider.base_url".to_string(),
        );
        mappings.insert(
            "MODEL_PROVIDER_REQUIRES_OPENAI_AUTH".to_string(),
            "model_provider.requires_openai_auth".to_string(),
        );
        mappings.insert(
            "MODEL_PROVIDER_API_PROTOCOL".to_string(),
            "model_provider.api_protocol".to_string(),
        );

        // Project related mappings
        mappings.insert(
            "PROJECT_ID".to_string(),
            "project_context.project_id".to_string(),
        );
        mappings.insert(
            "PROJECT_NAME".to_string(),
            "project_context.project_name".to_string(),
        );
        mappings.insert(
            "PROJECT_PATH".to_string(),
            "project_context.project_path".to_string(),
        );

        Self { mappings }
    }

    /// Resolve environment variables in an agent config
    pub fn resolve_agent_config(&self, config: &mut AgentConfig, context: &ResolutionContext) {
        // Resolve environment variables
        for (_key, value) in config.env.iter_mut() {
            *value = self.resolve_value(value, context);
        }

        // Resolve command arguments
        for arg in config.args.iter_mut() {
            *arg = self.resolve_value(arg, context);
        }

        // Resolve system prompt template
        if let Some(sys_prompt) = config.system_prompt.as_mut() {
            sys_prompt.template = self.resolve_value(&sys_prompt.template, context);
        }

        // Resolve user prompt template
        if let Some(user_prompt) = config.user_prompt.as_mut() {
            user_prompt.template = self.resolve_value(&user_prompt.template, context);
        }
    }

    /// Resolve a single value by replacing placeholders
    pub fn resolve_value(&self, template: &str, context: &ResolutionContext) -> String {
        let mut result = template.to_string();

        // Replace ModelProvider variables
        result = result
            .replace("{MODEL_PROVIDER_ID}", &context.model_provider.id)
            .replace("{MODEL_PROVIDER_NAME}", &context.model_provider.name)
            .replace("{MODEL_PROVIDER_API_KEY}", &context.model_provider.api_key)
            .replace(
                "{MODEL_PROVIDER_DEFAULT_MODEL}",
                &context.model_provider.default_model,
            )
            .replace(
                "{MODEL_PROVIDER_BASE_URL}",
                &context.model_provider.base_url,
            )
            .replace(
                "{MODEL_PROVIDER_REQUIRES_OPENAI_AUTH}",
                &context.model_provider.requires_openai_auth.to_string(),
            )
            .replace(
                "{MODEL_PROVIDER_API_PROTOCOL}",
                context
                    .model_provider
                    .api_protocol
                    .as_ref()
                    .unwrap_or(&String::new()),
            );

        // Replace project variables
        result = result
            .replace("{PROJECT_ID}", &context.project_context.project_id)
            .replace("{PROJECT_NAME}", &context.project_context.project_name)
            .replace(
                "{PROJECT_PATH}",
                &context.project_context.project_path.display().to_string(),
            );

        // Replace custom variables
        for (key, value) in &context.custom_variables {
            result = result.replace(&format!("{{{}}}", key), value);
        }

        // Replace MCP variables
        for (key, value) in &context.mcp_variables {
            result = result.replace(&format!("{{{}}}", key), value);
        }

        result
    }

    /// Resolve system prompt
    pub fn resolve_system_prompt(
        &self,
        system_prompt: &Option<SystemPromptConfig>,
        context: &ResolutionContext,
    ) -> Option<String> {
        match system_prompt {
            Some(config) if config.enabled => {
                let resolved = self.resolve_value(&config.template, context);
                Some(resolved)
            }
            _ => None,
        }
    }

    /// Resolve user prompt
    pub fn resolve_user_prompt(
        &self,
        user_input: &str,
        user_prompt_config: &Option<UserPromptConfig>,
    ) -> String {
        match user_prompt_config {
            Some(config) if config.enabled => config.template.replace("{user_prompt}", user_input),
            _ => user_input.to_string(),
        }
    }

    /// Add a custom mapping
    pub fn add_mapping(&mut self, key: String, path: String) {
        self.mappings.insert(key, path);
    }
}

impl Default for EnvironmentVariableResolver {
    fn default() -> Self {
        Self::new()
    }
}
