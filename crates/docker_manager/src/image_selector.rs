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
        debug!("created message, message : {}", platform);

        Self { config, platform }
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
                "服务类型 '{}' 未启用或配置不存在",
                service_type
            )));
        }

        // 直接计算镜像名称，无需缓存
        let image_name = self
            .select_service_image(service_type, project_overrides)
            .await?;

        info!(
            "选择镜像: {} (服务: {}, 平台: {})",
            image_name, service_type, self.platform
        );

        Ok(image_name)
    }

    /// 获取服务配置
    pub async fn get_service_config(
        &self,
        service_type: &ServiceType,
    ) -> DockerResult<shared_types::ServiceImageConfig> {
        // 强制验证：service_type 必须明确指定并启用
        if !self.is_service_enabled(service_type) {
            return Err(DockerError::ConfigurationError(format!(
                "服务类型 '{}' 未启用或配置不存在",
                service_type
            )));
        }

        // 从配置中获取服务配置
        let service_key = service_type.to_string();
        match self.config.services.get(&service_key) {
            Some(service_config) => {
                info!("get message configsucceeded: {}", service_key);
                Ok(service_config.clone())
            }
            None => Err(DockerError::ConfigurationError(format!(
                "服务类型 '{}' 的配置不存在",
                service_type
            ))),
        }
    }

    /// 检查服务是否已启用和配置
    pub fn is_service_enabled(&self, service_type: &ServiceType) -> bool {
        let service_key = service_type.to_string();
        info!(
            "🔍 [IMAGE_SELECTOR] 检查服务是否启用: service_type={:?}, service_key={}",
            service_type, service_key
        );

        if let Some(service_config) = self.config.services.get(&service_key) {
            info!(
                "✅ [IMAGE_SELECTOR] 服务已找到: enabled={}, arm64_image={:?}",
                service_config.enabled, service_config.arm64_image
            );
            service_config.enabled
        } else {
            warn!(
                "❌ [IMAGE_SELECTOR] 服务类型 '{}' 未在配置中找到，可用服务: {:?}",
                service_key,
                self.config.services.keys().collect::<Vec<_>>()
            );
            false
        }
    }

    /// 从服务特定配置选择镜像
    /// 简化版本：针对2种镜像的静态映射
    async fn select_service_image(
        &self,
        service_type: &ServiceType,
        _project_overrides: Option<&ProjectImageOverrides>,
    ) -> DockerResult<String> {
        let service_key = service_type.to_string();

        // 1. 优先使用服务特定配置
        if let Some(service_config) = self.config.services.get(&service_key) {
            // 服务级通用镜像（最高优先级）
            if let Some(image) = &service_config.image {
                debug!(" message : {}", image);
                return Ok(image.clone());
            }

            // 平台特定镜像
            if self.platform == "linux/arm64" {
                if let Some(arm64_image) = &service_config.arm64_image {
                    debug!(" message ARM64 message : {}", arm64_image);
                    return Ok(arm64_image.clone());
                }
            } else if let Some(amd64_image) = &service_config.amd64_image {
                debug!(" message AMD64 message : {}", amd64_image);
                return Ok(amd64_image.clone());
            }
        }

        // 2. 使用全局默认配置
        if let Some(default_image) = &self.config.global_defaults.default_image {
            debug!(" message default message : {}", default_image);
            return Ok(default_image.clone());
        }

        // 3. 配置错误：不应该发生，因为默认配置已经设置了镜像
        Err(DockerError::ConfigurationError(format!(
            "服务类型 '{}' 没有可用的镜像配置，请检查配置文件",
            service_key
        )))
    }
}
