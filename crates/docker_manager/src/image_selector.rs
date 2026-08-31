//! 镜像选择器
//!
//! 根据服务类型选择合适的 Docker 镜像。
//! 简化版本：针对只有2种镜像的静态映射场景进行了优化。

use crate::utils::DockerUtils;
use crate::{DockerError, DockerResult};
use shared_types::{MultiImageConfig, ProjectImageOverrides, ServiceType};
use tracing::{debug, info, warn};

/// 简化的镜像选择器
///
/// 根据服务类型选择合适的 Docker 镜像。
/// 针对2种镜像的静态映射场景进行了优化，移除了不必要的缓存。
/// 强制要求明确指定服务类型，不支持默认值。
///
/// # 配置兼容性
///
/// 支持老配置文件的兼容性：
/// - 老配置中 services 的 key 可能是 `"rcoder"`，而不是 `"web-agent-runner"`
/// - 本选择器会自动尝试两种 key 进行查找
pub struct ImageSelector {
    /// 多镜像配置
    config: MultiImageConfig,
    /// 当前平台
    platform: String,
}

impl ImageSelector {
    /// 创建新的镜像选择器
    pub fn new(config: MultiImageConfig) -> Self {
        let platform = DockerUtils::get_optimal_platform();
        debug!("created selector for: {}", platform);

        Self { config, platform }
    }

    /// 查找服务配置（支持老配置兼容）
    ///
    /// 支持两种 key 格式：
    /// 1. 新格式：`"web-agent-runner"`, `"computer-agent-runner"`
    /// 2. 老格式：`"rcoder"` (兼容 WebAgentRunner)
    fn find_service_config(
        &self,
        service_type: &ServiceType,
    ) -> Option<&shared_types::ServiceImageConfig> {
        // 1. 先尝试新的服务名称
        let service_key = service_type.to_string();
        if let Some(config) = self.config.services.get(&service_key) {
            return Some(config);
        }

        // 2. 如果找不到，尝试老的服务名称
        match service_type {
            ServiceType::WebAgentRunner => {
                // 兼容老配置中的 "rcoder" key
                self.config.services.get("rcoder")
            }
            ServiceType::ComputerAgentRunner => {
                // ComputerAgentRunner 没有老名称
                None
            }
            ServiceType::Userapp | ServiceType::UserappBuilder => {
                // Userapp / UserappBuilder 镜像由调用方/chart 提供,不走多镜像选择。
                // (UserappBuilder 在 select_image 特判为 dev-rcoder-agent-runner,见路B)
                None
            }
        }
    }

    /// 根据服务类型和项目配置选择镜像
    ///
    /// 注意：service_type 不能为空，必须明确指定。
    /// 会自动验证服务是否已启用。
    /// 简化版本：直接计算镜像名称，无缓存
    pub async fn select_image(
        &self,
        service_type: &ServiceType,
        project_overrides: Option<&ProjectImageOverrides>,
    ) -> DockerResult<String> {
        // 强制验证：service_type 必须明确指定并启用
        if !self.is_service_enabled(service_type) {
            return Err(DockerError::ConfigurationError(format!(
                "service type '{}' is not enabled or configuration does not exist",
                service_type
            )));
        }

        // 直接计算镜像名称，无需缓存
        let image_name = self
            .select_service_image(service_type, project_overrides)
            .await?;

        info!(
            "Selected image: {} (service: {}, platform: {})",
            image_name, service_type, self.platform
        );

        Ok(image_name)
    }

    /// 获取服务配置
    ///
    /// 支持老配置兼容性，会自动尝试新旧两种服务名称
    pub async fn get_service_config(
        &self,
        service_type: &ServiceType,
    ) -> DockerResult<shared_types::ServiceImageConfig> {
        // 强制验证：service_type 必须明确指定并启用
        if !self.is_service_enabled(service_type) {
            return Err(DockerError::ConfigurationError(format!(
                "service type '{}' is not enabled or configuration does not exist",
                service_type
            )));
        }

        // 从配置中获取服务配置（支持老配置兼容）
        match self.find_service_config(service_type) {
            Some(service_config) => {
                info!("Get config succeeded: {}", service_type);
                Ok(service_config.clone())
            }
            None => Err(DockerError::ConfigurationError(format!(
                "configuration for service type '{}' does not exist",
                service_type
            ))),
        }
    }

    /// 检查服务是否已启用和配置
    ///
    /// 支持老配置兼容性，会自动尝试新旧两种服务名称
    pub fn is_service_enabled(&self, service_type: &ServiceType) -> bool {
        let service_key = service_type.to_string();
        info!(
            "[IMAGE_SELECTOR] Checking if service is enabled: service_type={:?}, service_key={}",
            service_type, service_key
        );

        // 使用 find_service_config 支持老配置兼容
        if let Some(service_config) = self.find_service_config(service_type) {
            info!(
                "[IMAGE_SELECTOR] Service found: enabled={}, arm64_image={:?}",
                service_config.enabled, service_config.arm64_image
            );
            service_config.enabled
        } else {
            warn!(
                "[IMAGE_SELECTOR] Service type '{}' not found in config, available services: {:?}",
                service_key,
                self.config.services.keys().collect::<Vec<_>>()
            );
            false
        }
    }

    /// 从服务特定配置选择镜像
    /// 简化版本：针对2种镜像的静态映射
    ///
    /// 支持老配置兼容性，会自动尝试新旧两种服务名称
    async fn select_service_image(
        &self,
        service_type: &ServiceType,
        _project_overrides: Option<&ProjectImageOverrides>,
    ) -> DockerResult<String> {
        // 1. 优先使用服务特定配置（支持老配置兼容）
        if let Some(service_config) = self.find_service_config(service_type) {
            // 服务级通用镜像（最高优先级）
            if let Some(image) = &service_config.image {
                debug!(" using image: {}", image);
                return Ok(image.clone());
            }

            // 平台特定镜像
            if self.platform == "linux/arm64" {
                if let Some(arm64_image) = &service_config.arm64_image {
                    debug!(" using ARM64 image: {}", arm64_image);
                    return Ok(arm64_image.clone());
                }
            } else if let Some(amd64_image) = &service_config.amd64_image {
                debug!(" using AMD64 image: {}", amd64_image);
                return Ok(amd64_image.clone());
            }
        }

        // 2. 使用全局默认配置
        if let Some(default_image) = &self.config.global_defaults.default_image {
            debug!(" using default image: {}", default_image);
            return Ok(default_image.clone());
        }

        // 3. 配置错误：不应该发生，因为默认配置已经设置了镜像
        Err(DockerError::ConfigurationError(format!(
            "Service type '{}' has no available image config, please check the configuration file",
            service_type
        )))
    }
}
