//! 项目级镜像覆盖（impl ProjectImageOverrides）。

use super::multi_image_inner::{ConfigError, ProjectImageOverrides};
use crate::ServiceType;
use crate::runtime_config::service::ServiceImageConfig;

impl ProjectImageOverrides {
    /// 验证项目级配置
    pub fn validate(&self) -> Result<(), ConfigError> {
        // 验证镜像覆盖配置
        for (service_type, image_name) in &self.images {
            if service_type.trim().is_empty() {
                return Err(ConfigError::ValidationError(
                    "Service type name cannot be empty".to_string(),
                ));
            }

            if image_name.trim().is_empty() {
                return Err(ConfigError::ValidationError(format!(
                    "Image name for service type '{}' cannot be empty",
                    service_type
                )));
            }
        }

        // 验证启用的服务类型
        for service_type in &self.enabled_services {
            if service_type.trim().is_empty() {
                return Err(ConfigError::ValidationError(
                    "Enabled service type name cannot be empty".to_string(),
                ));
            }
        }

        Ok(())
    }

    /// 应用项目级配置到服务配置
    pub fn apply_to_service_config(
        &self,
        service_type: &ServiceType,
        config: &mut ServiceImageConfig,
    ) -> Result<(), ConfigError> {
        let service_key = service_type.to_string();

        // 应用镜像覆盖
        if let Some(override_image) = self.images.get(&service_key) {
            config.image = Some(override_image.clone());
            tracing::info!(
                "Applying project-level image override to service '{}': {}",
                service_key,
                override_image
            );
        }

        // 应用环境变量覆盖
        for (key, value) in &self.environment {
            config.environment.insert(key.clone(), value.clone());
        }

        Ok(())
    }

    /// 生成配置哈希键（用于缓存）
    ///
    /// 注意：使用 `DefaultHasher`，其输出**不保证跨 Rust 版本稳定**，故仅限
    /// 进程内缓存 key 使用，不可作为持久化 key 或跨进程比对依据。
    pub fn hash_key(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();

        // 哈希镜像配置
        for (key, value) in &self.images {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }

        // 哈希启用的服务
        for service in &self.enabled_services {
            service.hash(&mut hasher);
        }

        // 哈希环境变量
        for (key, value) in &self.environment {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }

        format!("{:x}", hasher.finish())
    }

    /// 检查服务是否在项目级配置中启用
    pub fn is_service_enabled(&self, service_type: &ServiceType) -> bool {
        let service_key = service_type.to_string();
        self.enabled_services.contains(&service_key)
    }

    /// 获取配置摘要
    pub fn get_summary(&self) -> String {
        format!(
            "Images: {}, Enabled Services: {}, Environment Variables: {}",
            self.images.len(),
            self.enabled_services.len(),
            self.environment.len()
        )
    }
}
