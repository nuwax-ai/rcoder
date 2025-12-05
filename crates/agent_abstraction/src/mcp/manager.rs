//! MCP server manager implementation
//!
//! 管理多个 MCP 服务实例的生命周期。

use std::sync::Arc;

use agent_config::McpServerConfig;
use dashmap::DashMap;
use rmcp::model::Tool;

use super::error::{McpError, McpResult};
use super::instance::McpServerInstance;
use super::types::{McpServerInfo, ToolCallRequest, ToolCallResponse};

/// MCP server manager
///
/// 管理多个 MCP 服务实例，提供统一的启动/停止/查询接口。
/// 使用 DashMap 实现高效的并发访问。
pub struct McpServerManager {
    /// Active server instances
    servers: DashMap<String, Arc<McpServerInstance>>,
}

impl McpServerManager {
    /// Create a new MCP server manager
    pub fn new() -> Self {
        Self {
            servers: DashMap::new(),
        }
    }

    /// Register a server (不启动)
    pub fn register_server(&self, name: &str, config: McpServerConfig) -> McpResult<()> {
        if self.servers.contains_key(name) {
            return Err(McpError::AlreadyRunning(name.to_string()));
        }

        let instance = Arc::new(McpServerInstance::new(name.to_string(), config));
        self.servers.insert(name.to_string(), instance);

        tracing::debug!(name = %name, "MCP server registered");
        Ok(())
    }

    /// Start a registered server
    pub async fn start_server(&self, name: &str) -> McpResult<()> {
        let instance = self
            .servers
            .get(name)
            .map(|r| Arc::clone(r.value()))
            .ok_or_else(|| McpError::NotRunning(format!("Server {} not registered", name)))?;

        instance.start().await
    }

    /// Register and start a server
    pub async fn start_new_server(
        &self,
        name: &str,
        config: McpServerConfig,
    ) -> McpResult<Arc<McpServerInstance>> {
        // 检查是否已存在
        if self.servers.contains_key(name) {
            return Err(McpError::AlreadyRunning(name.to_string()));
        }

        // 创建并启动
        let instance = Arc::new(McpServerInstance::new(name.to_string(), config));
        instance.start().await?;

        // 存储
        self.servers.insert(name.to_string(), Arc::clone(&instance));

        Ok(instance)
    }

    /// Stop a server
    pub async fn stop_server(&self, name: &str) -> McpResult<()> {
        let instance = self
            .servers
            .get(name)
            .map(|r| Arc::clone(r.value()))
            .ok_or_else(|| McpError::NotRunning(name.to_string()))?;

        instance.stop().await
    }

    /// Stop and remove a server
    pub async fn remove_server(&self, name: &str) -> McpResult<()> {
        if let Some((_, instance)) = self.servers.remove(name) {
            instance.stop().await?;
        }
        Ok(())
    }

    /// Get server instance
    pub fn get_server(&self, name: &str) -> Option<Arc<McpServerInstance>> {
        self.servers.get(name).map(|r| Arc::clone(r.value()))
    }

    /// Get server info
    pub async fn get_server_info(&self, name: &str) -> Option<McpServerInfo> {
        if let Some(instance) = self.get_server(name) {
            Some(instance.info().await)
        } else {
            None
        }
    }

    /// List all server names
    pub fn list_servers(&self) -> Vec<String> {
        self.servers.iter().map(|e| e.key().clone()).collect()
    }

    /// List all server infos
    pub async fn list_server_infos(&self) -> Vec<McpServerInfo> {
        let mut infos = Vec::new();
        for entry in self.servers.iter() {
            infos.push(entry.value().info().await);
        }
        infos
    }

    /// Get the number of servers
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    /// Get running server count
    pub async fn running_server_count(&self) -> usize {
        let mut count = 0;
        for entry in self.servers.iter() {
            if entry.value().is_running().await {
                count += 1;
            }
        }
        count
    }

    /// Check if a server exists
    pub fn has_server(&self, name: &str) -> bool {
        self.servers.contains_key(name)
    }

    /// Check if a server is running
    pub async fn is_server_running(&self, name: &str) -> bool {
        if let Some(instance) = self.get_server(name) {
            instance.is_running().await
        } else {
            false
        }
    }

    /// Stop all servers
    pub async fn stop_all(&self) -> McpResult<()> {
        let names: Vec<String> = self.list_servers();
        let mut errors = Vec::new();

        for name in names {
            if let Err(e) = self.stop_server(&name).await {
                tracing::warn!(name = %name, error = %e, "Failed to stop MCP server");
                errors.push(format!("{}: {}", name, e));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(McpError::StopFailed(errors.join("; ")))
        }
    }

    /// List tools from a specific server
    pub async fn list_tools(&self, server_name: &str) -> McpResult<Vec<Tool>> {
        let instance = self
            .get_server(server_name)
            .ok_or_else(|| McpError::NotRunning(server_name.to_string()))?;

        instance.list_tools().await
    }

    /// List tools from all running servers
    pub async fn list_all_tools(&self) -> Vec<(String, Vec<Tool>)> {
        let mut result = Vec::new();

        for entry in self.servers.iter() {
            let name = entry.key().clone();
            if let Ok(tools) = entry.value().list_tools().await {
                result.push((name, tools));
            }
        }

        result
    }

    /// Call a tool on a specific server
    pub async fn call_tool(
        &self,
        server_name: &str,
        request: ToolCallRequest,
    ) -> McpResult<ToolCallResponse> {
        let instance = self
            .get_server(server_name)
            .ok_or_else(|| McpError::NotRunning(server_name.to_string()))?;

        instance.call_tool(request).await
    }

    /// Batch start servers
    pub async fn start_servers(&self, configs: &[(String, McpServerConfig)]) -> Vec<McpResult<()>> {
        let mut results = Vec::new();

        for (name, config) in configs {
            let result = self
                .start_new_server(name, config.clone())
                .await
                .map(|_| ());
            results.push(result);
        }

        results
    }
}

impl Default for McpServerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for McpServerManager {
    fn drop(&mut self) {
        tracing::debug!("McpServerManager dropping, servers will be cleaned up");
    }
}

impl std::fmt::Debug for McpServerManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerManager")
            .field("server_count", &self.servers.len())
            .finish()
    }
}
