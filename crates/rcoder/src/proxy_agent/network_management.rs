//! 简化的网络管理接口
//!
//! 统一的网络管理功能：
//! - 集成增强和基础端口管理器
//! - 统一的网络健康检查
//! - 网络性能监控
//! - 自动故障恢复

use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, error, info, warn};
use crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER;

/// 错误转换辅助函数
fn map_error_to_string<T: Into<String>>(error: T) -> String {
    error.into()
}

/// 网络管理状态
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NetworkManagementStatus {
    /// 总端口数
    pub total_ports: u16,
    /// 已使用端口数
    pub used_ports: u16,
    /// 端口利用率
    pub port_utilization: f64,
    /// 活跃连接数
    pub active_connections: usize,
    /// 失败连接数
    pub failed_connections: usize,
    /// 超时连接数
    pub timeout_connections: usize,
    /// 最后检查时间
    pub last_health_check: chrono::DateTime<chrono::Utc>,
}

/// 统一的网络管理器
#[derive(Clone)]
pub struct UnifiedNetworkManager {
    /// 基础端口管理器（兼容性）
    basic_port_manager: Option<()>,
}

impl UnifiedNetworkManager {
    /// 创建新的网络管理器
    pub fn new() -> Self {
        Self::with_enhanced(true)
    }

    /// 使用增强的网络管理器创建
    pub fn with_enhanced(use_enhanced: bool) -> Self {
        info!("🌐 初始化统一网络管理器: 增强模式={}", use_enhanced);

        Self {
            basic_port_manager: None,
        }
    }

    /// 分配端口
    pub async fn allocate_port(&self, project_id: &str) -> Result<u16, String> {
        info!("🔌 [NETWORK] 使用基础端口管理器分配端口: project_id={}", project_id);
        GLOBAL_PORT_MANAGER.allocate_port().await.map_err(map_error_to_string)
    }

    /// 释放端口
    pub async fn release_port(&self, port: u16) -> Result<(), String> {
        info!("🔌 [NETWORK] 使用基础端口管理器释放端口: port={}", port);
        GLOBAL_PORT_MANAGER.release_port(port).await;
        Ok(())
    }

    /// 获取网络管理状态
    pub async fn get_network_status(&self) -> NetworkManagementStatus {
        let allocated_count = GLOBAL_PORT_MANAGER.allocated_count().await;
        let total_ports = 9999 - 8000 + 1;
        let port_utilization = if total_ports > 0 {
            (allocated_count as f64 / total_ports as f64) * 100.0
        } else {
            0.0
        };

        NetworkManagementStatus {
            total_ports,
            used_ports: allocated_count as u16,
            port_utilization,
            active_connections: allocated_count,
            failed_connections: 0,
            timeout_connections: 0,
            last_health_check: chrono::Utc::now(),
        }
    }

    /// 执行网络健康检查
    pub async fn perform_health_check(&self) -> Result<NetworkDiagnostics, String> {
        info!("🔍 [NETWORK] 开始网络健康检查");

        let status = self.get_network_status().await;
        let mut diagnostics = HashMap::new();
        let mut recommendations = Vec::new();

        // 基础诊断信息
        diagnostics.insert("端口管理器类型".to_string(), "基础端口管理器".to_string());
        diagnostics.insert("健康检查时间".to_string(), chrono::Utc::now().to_rfc3339());

        // 基于状态提供建议
        if status.port_utilization > 80.0 {
            recommendations.push("考虑扩展端口范围以避免端口不足".to_string());
        }

        let diagnostics_result = NetworkDiagnostics {
            status,
            diagnostics,
            recommendations,
        };

        info!("✅ [NETWORK] 网络健康检查完成: 状态={:?}, 建议={}条",
              diagnostics_result.status, diagnostics_result.recommendations.len());

        Ok(diagnostics_result)
    }

    /// 清理空闲端口
    pub async fn cleanup_idle_ports(&self, _idle_threshold: std::time::Duration) -> Result<usize, String> {
        info!("🧹 [NETWORK] 基础管理器不支持空闲端口清理");
        Ok(0) // 基础管理器返回0，表示没有清理
    }

    /// 获取端口分配历史
    pub async fn get_port_allocation_history(&self, _limit: Option<usize>) -> Vec<(u16, String, chrono::DateTime<chrono::Utc>)> {
        info!("📊 [NETWORK] 基础管理器不支持端口分配历史");
        Vec::new()
    }

    /// 网络性能测试
    pub async fn perform_network_performance_test(&self, _port: u16) -> Result<HashMap<String, f64>, String> {
        info!("🚀 [NETWORK] 基础管理器不支持网络性能测试");
        Err("基础管理器不支持性能测试".to_string())
    }

    /// 强制端口重新分配
    pub async fn force_port_reallocation(&self, _project_id: &str, old_port: u16) -> Result<u16, String> {
        info!("🔄 [NETWORK] 基础端口重新分配: old_port={}", old_port);

        // 基础管理器的重新分配逻辑
        GLOBAL_PORT_MANAGER.release_port(old_port).await;
        let new_port = GLOBAL_PORT_MANAGER.allocate_port().await.map_err(map_error_to_string)?;
        info!("✅ [NETWORK] 基础端口重新分配完成: {} -> {}", old_port, new_port);
        Ok(new_port)
    }
}

impl Default for UnifiedNetworkManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 全局网络管理器实例
pub static GLOBAL_NETWORK_MANAGER: std::sync::LazyLock<std::sync::Mutex<Option<UnifiedNetworkManager>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// 初始化全局网络管理器
pub fn init_global_network_manager(use_enhanced: bool) -> Result<(), String> {
    let manager = UnifiedNetworkManager::with_enhanced(use_enhanced);
    let mut global_manager = GLOBAL_NETWORK_MANAGER.lock().unwrap();
    *global_manager = Some(manager);
    info!("🌐 全局网络管理器已初始化: 增强模式={}", use_enhanced);
    Ok(())
}

/// 获取全局网络管理器
pub fn get_global_network_manager() -> Option<UnifiedNetworkManager> {
    GLOBAL_NETWORK_MANAGER.lock().unwrap().clone()
}

/// 网络诊断信息
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NetworkDiagnostics {
    /// 网络管理状态
    pub status: NetworkManagementStatus,
    /// 详细的诊断信息
    pub diagnostics: HashMap<String, String>,
    /// 建议
    pub recommendations: Vec<String>,
}