//! 镜像管理：镜像拉取/选择、服务配置获取（从 DockerManager 拆出）

use bollard::query_parameters::CreateImageOptions;
use futures_util::StreamExt;
use tracing::{debug, info};

use crate::{DockerError, DockerManager, DockerResult};

impl DockerManager {
    /// 确保镜像存在，如果不存在则拉取
    pub(crate) async fn ensure_image_exists(&self, image: &str) -> DockerResult<()> {
        let inspect_started = std::time::Instant::now();
        debug!("Checking if image exists: {}", image);

        // 检查镜像是否存在
        match self.docker.inspect_image(image).await {
            Ok(_) => {
                debug!(
                    "Image {} already exists (inspect {:?})",
                    image,
                    inspect_started.elapsed()
                );
                Ok(())
            }
            Err(_) => {
                info!(
                    "Image {} not found, pulling... (inspect {:?})",
                    image,
                    inspect_started.elapsed()
                );

                let pull_started = std::time::Instant::now();
                let pull_options = CreateImageOptions {
                    from_image: Some(image.to_string()),
                    ..Default::default()
                };

                let mut pull_stream = self.docker.create_image(Some(pull_options), None, None);

                while let Some(result) = pull_stream.next().await {
                    match result {
                        Ok(progress) => {
                            if let Some(status) = progress.status {
                                debug!("Image pull progress: {}", status);
                            }
                        }
                        Err(e) => {
                            return Err(DockerError::ImagePullError(format!(
                                "Failed to pull image after {:?}: {}",
                                pull_started.elapsed(),
                                e
                            )));
                        }
                    }
                }

                info!(
                    "Image {} pull completed in {:?}",
                    image,
                    pull_started.elapsed()
                );
                Ok(())
            }
        }
    }

    /// 获取配置的默认镜像
    pub fn get_default_image(&self) -> String {
        self.config.default_image.clone()
    }

    /// 根据服务类型选择镜像
    pub async fn select_image(
        &self,
        service_type: &shared_types::ServiceType,
        project_overrides: Option<&shared_types::ProjectImageOverrides>,
    ) -> DockerResult<String> {
        // 使用多镜像配置选择镜像
        use crate::image_selector::ImageSelector;
        let selector = ImageSelector::new(self.config.multi_image_config.clone());

        debug!("ImageSelector: {:?}", service_type);
        selector.select_image(service_type, project_overrides).await
    }

    /// 获取服务配置
    pub async fn get_service_config(
        &self,
        service_type: &shared_types::ServiceType,
    ) -> DockerResult<shared_types::ServiceImageConfig> {
        use crate::image_selector::ImageSelector;
        let selector = ImageSelector::new(self.config.multi_image_config.clone());

        debug!("Getting config: {:?}", service_type);
        selector.get_service_config(service_type).await
    }
}
