//! 容器自检测器
//!
//! 用于在容器内部通过 Docker API 检测自己的挂载信息，
//! 获取容器内路径对应的宿主机绝对路径

use anyhow::{anyhow, bail, Context, Result};
use bollard::{Docker, API_DEFAULT_VERSION};
use std::path::Path;
use tokio::fs;
use tracing::{debug, info, warn};

/// 容器自检测器
///
/// 用于检测当前容器的挂载信息，获取容器内路径对应的宿主机路径
pub struct ContainerSelfInspector {
    /// Docker 客户端
    docker_client: Docker,
    /// 当前容器ID
    container_id: String,
}

impl ContainerSelfInspector {
    /// 创建新的容器自检测器
    ///
    /// # Arguments
    /// * `docker_socket_path` - Docker socket 路径
    ///
    /// # Returns
    /// * `Result<Self>` - 检测器实例或错误
    ///
    /// # Examples
    /// ```
    /// let inspector = ContainerSelfInspector::new("/var/run/docker.sock").await?;
    /// ```
    pub async fn new(docker_socket_path: &str) -> Result<Self> {
        info!("初始化容器自检测器，Docker socket: {}", docker_socket_path);

        // 创建 Docker 客户端
        let docker_client = Docker::connect_with_socket(docker_socket_path, 120, API_DEFAULT_VERSION)
            .context("连接 Docker socket 失败")?;

        // 测试 Docker 连接
        docker_client.ping().await
            .context("测试 Docker 连接失败，请检查 socket 路径和权限")?;

        info!("✅ Docker 连接成功");

        // 获取当前容器ID
        let container_id = Self::get_current_container_id().await
            .context("获取当前容器ID失败")?;

        info!("✅ 检测到当前容器ID: {}", container_id);

        Ok(Self {
            docker_client,
            container_id,
        })
    }

    /// 检测容器内路径对应的宿主机路径
    ///
    /// # Arguments
    /// * `container_path` - 容器内路径（如 "/app/project_workspace"）
    ///
    /// # Returns
    /// * `Result<String>` - 宿主机绝对路径或错误
    ///
    /// # Examples
    /// ```
    /// let host_path = inspector.detect_host_path_for_container_dir("/app/project_workspace").await?;
    /// ```
    pub async fn detect_host_path_for_container_dir(&self, container_path: &str) -> Result<String> {
        info!("检测路径 {} 对应的宿主机路径", container_path);

        // 获取容器详细信息
        let inspect_result = self.docker_client
            .inspect_container(&self.container_id, None::<bollard::query_parameters::InspectContainerOptions>)
            .await
            .context("调用 Docker inspect API 失败")?;

        debug!("容器 inspect 结果: {:?}", serde_json::to_string_pretty(&inspect_result)?);

        // 解析挂载信息
        if let Some(mounts) = inspect_result.mounts {
            debug!("容器共有 {} 个挂载点", mounts.len());

            for (index, mount) in mounts.iter().enumerate() {
                let mount_destination = mount.destination.as_ref()
                    .ok_or_else(|| anyhow!("挂载点 {} 没有 destination 字段", index))?
                    .clone();

                debug!("挂载点 {}: {} -> {}", index, mount_destination,
                      mount.source.as_ref().unwrap_or(&String::new()).clone());

                // 检查是否是我们要找的路径
                if mount_destination == container_path {
                    let host_path = mount.source.as_ref()
                        .ok_or_else(|| anyhow!("挂载点 {} 没有 source 字段", index))?
                        .clone();

                    info!("✅ 找到匹配的挂载点: {} -> {}", container_path, host_path);
                    return Ok(host_path);
                }
            }

            // 如果没找到，列出所有挂载点供调试
            warn!("未找到路径 {} 的挂载信息，可用的挂载点:", container_path);
            for (index, mount) in mounts.iter().enumerate() {
                if let (Some(dest), Some(source)) = (&mount.destination, &mount.source) {
                    warn!("  {}: {} -> {}", index, dest, source);
                }
            }

            bail!("未找到路径 {} 的挂载信息", container_path);
        } else {
            bail!("容器没有挂载信息（mounts 字段为空）");
        }
    }

    /// 获取当前容器ID
    ///
    /// 通过读取 `/proc/self/cgroup` 文件解析容器ID
    ///
    /// # Returns
    /// * `Result<String>` - 容器ID或错误
    async fn get_current_container_id() -> Result<String> {
        debug!("开始获取当前容器ID");

        let cgroup_content = fs::read_to_string("/proc/self/cgroup").await
            .with_context(|| "读取 /proc/self/cgroup 文件失败")?;

        debug!("cgroup 文件内容: {}", cgroup_content);

        // 解析 cgroup 文件获取容器ID
        // 格式示例: 12:perf_event:/docker/abc123def456...
        for line in cgroup_content.lines() {
            debug!("解析 cgroup 行: {}", line);

            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 3 {
                let cgroup_path = parts[2];

                // 检查是否是 Docker 容器
                if cgroup_path.contains("/docker/") || cgroup_path.contains(".scope") {
                    debug!("找到 Docker 相关的 cgroup: {}", cgroup_path);

                    // 提取容器ID
                    let container_id = if cgroup_path.contains("/docker/") {
                        // 格式: /docker/abc123def456...
                        let id_parts: Vec<&str> = cgroup_path.split('/').collect();
                        if id_parts.len() >= 3 {
                            id_parts[2].to_string()
                        } else {
                            continue;
                        }
                    } else if cgroup_path.contains(".scope") {
                        // 格式: /system.slice/docker-abc123def456...scope
                        let scope_name = cgroup_path.split('/').last().unwrap_or("");
                        if scope_name.starts_with("docker-") && scope_name.ends_with(".scope") {
                            // 移除 "docker-" 前缀和 ".scope" 后缀
                            let id = &scope_name[7..scope_name.len() - 6];
                            id.to_string()
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    };

                    // 验证容器ID格式（应该是64个字符的十六进制字符串）
                    if container_id.len() == 64 && container_id.chars().all(|c| c.is_ascii_hexdigit()) {
                        info!("✅ 成功解析容器ID: {}", container_id);
                        return Ok(container_id);
                    } else {
                        debug!("跳过无效的容器ID: {}", container_id);
                    }
                }
            }
        }

        // 如果 cgroup 方法失败，尝试其他方法
        warn!("通过 cgroup 无法获取容器ID，尝试其他方法");

        // 方法2：尝试读取 /proc/1/cgroup（主进程）
        if let Ok(cgroup_content) = fs::read_to_string("/proc/1/cgroup").await {
            debug!("尝试读取 /proc/1/cgroup");
            for line in cgroup_content.lines() {
                if line.contains("/docker/") || line.contains(".scope") {
                    debug!("在 /proc/1/cgroup 中找到: {}", line);
                    // 类似的解析逻辑...
                }
            }
        }

        // 方法3：尝试读取主机名（某些环境容器ID会作为主机名）
        if let Ok(hostname) = std::env::var("HOSTNAME") {
            debug!("检查 HOSTNAME 环境变量: {}", hostname);
            if hostname.len() == 12 && hostname.chars().all(|c| c.is_ascii_hexdigit()) {
                // 可能是短格式的容器ID（前12位）
                info!("✅ 从 HOSTNAME 获取到容器ID前缀: {}", hostname);
                return Ok(hostname);
            }
        }

        bail!("无法获取当前容器ID，请确保容器有足够的权限访问 /proc/self/cgroup");
    }

    /// 验证 Docker socket 连接
    ///
    /// # Returns
    /// * `Result<()>` - 连接成功或错误
    pub async fn verify_docker_connection(&self) -> Result<()> {
        self.docker_client.ping().await
            .context("Docker socket 连接测试失败")?;
        info!("✅ Docker socket 连接验证成功");
        Ok(())
    }

    /// 获取容器所有挂载点信息（用于调试）
    ///
    /// # Returns
    /// * `Result<Vec<(String, String)>>` - 挂载点列表（容器路径 -> 宿主机路径）
    pub async fn get_all_mounts(&self) -> Result<Vec<(String, String)>> {
        let inspect_result = self.docker_client
            .inspect_container(&self.container_id, None::<bollard::query_parameters::InspectContainerOptions>)
            .await
            .context("调用 Docker inspect API 失败")?;

        let mut mounts = Vec::new();

        if let Some(mount_infos) = inspect_result.mounts {
            for mount in mount_infos {
                if let (Some(dest), Some(source)) = (&mount.destination, &mount.source) {
                    mounts.push((
                        dest.clone(),
                        source.clone(),
                    ));
                }
            }
        }

        Ok(mounts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_container_id_parsing() {
        // 这里应该模拟 cgroup 文件内容进行测试
        // 由于测试环境不在容器内，这个测试可能需要跳过
    }
}