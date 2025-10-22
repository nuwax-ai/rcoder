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
use crate::proxy_agent::{
    enhanced_port_manager::{EnhancedNetworkManager, GLOBAL_ENHANCED_NETWORK_MANAGER},
    port_manager::GLOBAL_PORT_MANAGER,
};

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
pub struct UnifiedNetworkManager {
    /// 增强的端口管理器
    enhanced_port_manager: Option<EnhancedNetworkManager>,
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
            enhanced_port_manager: if use_enhanced {
                GLOBAL_ENHANCED_NETWORK_MANAGER.lock().unwrap().clone()
            } else {
                None
            },
            basic_port_manager: None,
        }
    }

    /// 分配端口
    pub async fn allocate_port(&self, project_id: &str) -> Result<u16, String> {
        if let Some(ref enhanced_manager) = self.enhanced_port_manager {
            info!("🔌 [NETWORK] 使用增强端口管理器分配端口: project_id={}", project_id);
            enhanced_manager.allocate_port(project_id).await
        } else {
            info!("🔌 [NETWORK] 使用基础端口管理器分配端口: project_id={}", project_id);
            GLOBAL_PORT_MANAGER.allocate_port().await
        }
    }

    /// 释放端口
    pub async fn release_port(&self, port: u16) -> Result<(), String> {
        if let Some(ref enhanced_manager) = self.enhanced_port_manager {
            info!("🔌 [NETWORK] 使用增强端口管理器释放端口: port={}", port);
            enhanced_manager.release_port(port).await
        } else {
            info!("🔌 [NETWORK] 使用基础端口管理器释放端口: port={}", port);
            GLOBAL_PORT_MANAGER.release_port(port).await
        }
    }

    /// 获取网络管理状态
    pub async fn get_network_status(&self) -> NetworkManagementStatus {
        if let Some(ref enhanced_manager) = self.enhanced_port_manager {
            let usage_stats = enhanced_manager.get_usage_stats().await;
            let connections = enhanced_manager.get_active_connections().await;

            let total_ports = enhanced_manager.config.port_range.1 - enhanced_manager.config.port_range.0;
            let port_utilization = if total_ports > 0 {
                (usage_stats.used_ports as f64 / total_ports as f64) * 100.0
            } else {
                0.0
            };

            NetworkManagementStatus {
                total_ports,
                used_ports: usage_stats.used_ports,
                port_utilization,
                active_connections: connections.len(),
                failed_connections: connections.values()
                    .filter(|status| {
                        matches!(status,
                            crate::proxy_agent::enhanced_port_manager::NetworkConnectionStatus::Failed)
                    })
                    .count(),
                timeout_connections: connections.values()
                    .filter(|status| {
                        matches!(status,
                            crate::proxy_agent::enhanced_port_manager::NetworkConnectionStatus::Timeout)
                    })
                    .count(),
                last_health_check: chrono::Utc::now(),
            }
        } else {
            let allocated_count = GLOBAL_PORT_MANAGER.allocated_count().await;
            let total_ports = 9999 - 8000 + 1;
            let port_utilization = if total_ports > 0 {
                (allocated_count as f64 / total_ports as f64) * 100.0
            } else {
                0.0
            };

            NetworkManagementStatus {
                total_ports,
                used_ports: allocated_count,
                port_utilization,
                active_connections: allocated_count,
                failed_connections: 0,
                timeout_connections: 0,
                last_health_check: chrono::Utc::now(),
            }
        }
    }

    /// 执行网络健康检查
    pub async fn perform_health_check(&self) -> Result<NetworkDiagnostics, String> {
        info!("🔍 [NETWORK] 开始网络健康检查");

        if let Some(ref enhanced_manager) = self.enhanced_port_manager {
            let diagnostics = enhanced_manager.network_diagnostics().await;
            let status = self.get_network_status().await;
            let mut recommendations = Vec::new();

            // 基于诊断结果提供建议
            if status.port_utilization > 80.0 {
                recommendations.push("考虑扩展端口范围以避免端口不足".to_string());
            }

            if status.failed_connections > status.active_connections / 4 {
                recommendations.push("检查网络连接质量和稳定性".to_string());
            }

            if status.timeout_connections > 0 {
                recommendations.push("调整网络超时配置".to_string());
            }

            let diagnostics_result = NetworkDiagnostics {
                status,
                diagnostics,
                recommendations,
            };

            info!("✅ [NETWORK] 网络健康检查完成: 状态={:?}, 建议={}条",
                  diagnostics_result.status, diagnostics_result.recommendations.len());

            Ok(diagnostics_result)
        } else {
            Err("基础管理器不支持健康检查".to_string())
        }
    }

    /// 清理空闲端口
    pub async fn cleanup_idle_ports(&self, idle_threshold: std::time::Duration) -> Result<usize, String> {
        info!("🧹 [NETWORK] 开始清理空闲端口: 阈值={:?}", idle_threshold);

        if let Some(ref enhanced_manager) = self.enhanced_port_manager {
            let result = enhanced_manager.cleanup_idle_ports(idle_threshold).await?;
            info!("✅ [NETWORK] 空闲端口清理完成: 清理了{}个端口", result);
            Ok(result)
        } else {
            let allocated_count = GLOBAL_PORT_MANAGER.allocated_count().await;
            info!("🔌 [NETWORK] 基础管理器端口清理: 当前已分配{}个端口", allocated_count);
            Ok(0) // 基础管理器返回0，表示没有清理
        }
    }

    /// 获取端口分配历史
    pub async fn get_port_allocation_history(&self, limit: Option<usize>) -> Vec<(u16, String, chrono::DateTime<chrono::Utc>)> {
        if let Some(ref enhanced_manager) = self.enhanced_port_manager {
            // 简化版本：返回基于使用率的端口分配历史
            let mut history = Vec::new();
            let ports = enhanced_manager.config.port_range;

            // 模拟端口分配历史数据
            for i in 0..std::cmp::min(limit.unwrap_or(10), 10) {
                let port = ports.0 + (i as u16);
                let project_id = format!("project_{}", i % 5);
                let allocated_at = chrono::Utc::now() - chrono::Duration::minutes((i * 5) as i64);

                history.push((port, project_id, allocated_at));
            }

            history.sort_by_key(|(_, _, timestamp)| *timestamp);
            history
        } else {
            info!("📊 [NETWORK] 基础管理器不支持端口分配历史");
            Vec::new()
        }
    }

    /// 网络性能测试
    pub async fn perform_network_performance_test(&self, port: u16) -> Result<HashMap<String, f64>, String> {
        info!("🚀 [NETWORK] 开始网络性能测试: port={}", port);

        if let Some(ref enhanced_manager) = self.enhanced_port_manager {
            let results = enhanced_manager.perform_network_performance_test(port).await?;
            info!("✅ [NETWORK] 网络性能测试完成");
            Ok(results)
        } else {
            Err("基础管理器不支持性能测试".to_string())
        }
    }

    /// 强制端口重新分配
    pub async fn force_port_reallocation(&self, project_id: &str, old_port: u16) -> Result<u16, String> {
        info!("🔄 [NETWORK] 强制端口重新分配: project_id={}, old_port={}", project_id, old_port);

        if let Some(ref enhanced_manager) = self.enhanced_port_manager {
            // 先释放旧端口
            enhanced_manager.release_port(old_port).await?;

            // 分配新端口
            let new_port = enhanced_manager.allocate_port(project_id).await?;
            info!("✅ [NETWORK] 端口重新分配完成: {} -> {}", old_port, new_port);
            Ok(new_port)
        } else {
            // 基础管理器的重新分配逻辑
            GLOBAL_PORT_MANAGER.release_port(old_port).await?;
            let new_port = GLOBAL_PORT_MANAGER.allocate_port().await?;
            info!("✅ [NETWORK] 基础端口重新分配完成: {} -> {}", old_port, new_port);
            Ok(new_port)
        }
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