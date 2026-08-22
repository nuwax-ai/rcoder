//! KubernetesRuntime 固有辅助方法群（从 kubernetes_runtime.rs 拆出）。
//!
//! API 访问器（pods/pvcs/pvs/httproute）、镜像选择的四级兜底、K8s 命名 sanitize、
//! 资源需求构建、容器基础信息组装——与 k8s_app_*.rs 业务文件群共享的基础设施层。

#![cfg(feature = "kubernetes")]

use container_runtime_api::{ContainerRuntimeResult, RuntimeContainerInfo};
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim, Pod};
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind};
use tracing::{debug, info, warn};

use shared_types::{ContainerBasicInfo, ServiceResourceLimits, ServiceType};

use super::kubernetes_runtime::KubernetesRuntime;

impl KubernetesRuntime {
    /// Get the Pod API
    pub(crate) fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Get the PVC API
    pub(crate) fn pvcs(&self) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Get the HTTPRoute API（gateway.networking.k8s.io/v1 动态资源；apply / delete by name / label 扫 / 条件删共用）
    pub(crate) fn httproute_api(&self) -> Api<DynamicObject> {
        let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
        let ar = ApiResource::from_gvk(&gvk);
        Api::namespaced_with(self.client.clone(), &self.namespace, &ar)
    }

    /// Get the PV API (cluster-scoped)
    ///
    /// 阶段2: 读 PV `csi.volumeAttributes.subvolumePath` (rcoder 挂根聚合)。
    pub(crate) fn pvs(&self) -> Api<PersistentVolume> {
        Api::<PersistentVolume>::all(self.client.clone())
    }

    pub(crate) fn service_container_prefix(
        &self,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        // 完全分家:pod/PVC 命名前缀优先读 kubernetes_config(自包含 image_tag_prefix),
        // 回退 multi_image_config(过渡期),再回退 service_type.container_prefix() 默认。
        // 避免命名漂移:k8s 配置改了前缀,pod 与 PVC 必须同步用新前缀。
        if let Some(k8s_cfg) = self
            .config
            .kubernetes_config
            .get_service_config(service_type)
        {
            return Ok(k8s_cfg.container_prefix().to_string());
        }
        let service_key = service_type.to_string();
        if let Some(config) = self
            .config
            .docker_manager_config
            .multi_image_config
            .services
            .get(&service_key)
        {
            return Ok(config.container_prefix().to_string());
        }
        // 最后兜底:service_type 默认前缀(避免命名查询因配置缺失而硬失败)
        Ok(service_type.container_prefix().to_string())
    }

    pub(crate) fn sanitize_k8s_name_part(input: &str) -> String {
        input
            .to_ascii_lowercase()
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string()
    }

    /// Select image based on service type.
    ///
    /// 优先级:env(RCODER_DOCKER_IMAGE* / RCODER_DOCKER_IMAGE_COMPUTER)> `kubernetes_config`
    /// (完全分家后的主数据源)> `multi_image_config`(docker_config,过渡期安全兜底,
    /// 避免旧 chart 未带 kubernetes_config 时选不到镜像)> 硬编码默认值。
    pub(crate) fn select_image(&self, service_type: &ServiceType) -> String {
        // 1. 优先使用环境变量（允许运行时覆盖;deployment.yaml 注入）
        // 注意：ComputerAgentRunner 必须优先检查 RCODER_DOCKER_IMAGE_COMPUTER
        match service_type {
            ServiceType::ComputerAgentRunner => {
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE_COMPUTER")
                    && !env_image.is_empty()
                {
                    info!(
                        "[K8S] Using image from RCODER_DOCKER_IMAGE_COMPUTER env: {}",
                        env_image
                    );
                    return env_image;
                }
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE")
                    && !env_image.is_empty()
                {
                    info!(
                        "[K8S] Using image from RCODER_DOCKER_IMAGE env: {}",
                        env_image
                    );
                    return env_image;
                }
            }
            // UserAppBuilder 复用 agent-runner 镜像(含 file-server embed + build 工具链),与
            // ComputerAgentRunner 同源。只读 RCODER_DOCKER_IMAGE_COMPUTER(= agent-runner 镜像),
            // 绝不能读 RCODER_DOCKER_IMAGE(= rcoder 主镜像)——后者默认 CMD 是 node REPL,不是 agent_runner,
            // 会导致 builder pod 落入 node 交互式 shell 而非跑 agent_runner + 内嵌 file-server。
            ServiceType::UserAppBuilder => {
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE_COMPUTER")
                    && !env_image.is_empty()
                {
                    info!(
                        "[K8S] UserAppBuilder using agent-runner image from RCODER_DOCKER_IMAGE_COMPUTER env: {}",
                        env_image
                    );
                    return env_image;
                }
                // COMPUTER env 未设 → 落到 step 2 读 kubernetes_config.user-app-builder.image
            }
            _ => {
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE")
                    && !env_image.is_empty()
                {
                    info!(
                        "[K8S] Using image from RCODER_DOCKER_IMAGE env: {}",
                        env_image
                    );
                    return env_image;
                }
            }
        }

        // 2. 从 kubernetes_config(完全分家后的主数据源)按平台选镜像
        if let Some(svc) = self
            .config
            .kubernetes_config
            .get_service_config(service_type)
        {
            let arch = std::env::consts::ARCH;
            let platform = if arch == "aarch64" || arch == "arm64" {
                "linux/arm64"
            } else {
                "linux/amd64"
            };
            if let Some(image) = svc.get_image_for_platform(platform) {
                info!("[K8S] Using image from kubernetes_config: {}", image);
                return image;
            }
        }

        // 3. 过渡期安全兜底:回退到 docker_config.multi_image_config
        // (旧 chart / 旧 config.yml 未带 kubernetes_config 段时,避免选不到镜像)
        warn!(
            "[K8S] kubernetes_config has no image for {}, falling back to multi_image_config (legacy)",
            service_type
        );
        let multi_config = &self.config.docker_manager_config.multi_image_config;
        if let Some(service_config) = multi_config.get_service_config(service_type) {
            // 优先使用 image 字段
            if let Some(ref image) = service_config.image {
                info!(
                    "[K8S] Using image from multi_image_config (fallback): {}",
                    image
                );
                return image.clone();
            }
            // 使用架构特定镜像
            let arch = std::env::consts::ARCH;
            let image = if arch == "aarch64" || arch == "arm64" {
                service_config.arm64_image.clone()
            } else {
                service_config.amd64_image.clone()
            };
            if let Some(img) = image {
                info!(
                    "[K8S] Using architecture-specific image (fallback): {}",
                    img
                );
                return img.to_string();
            }
            // 使用默认镜像
            if let Some(ref img) = service_config.default_image {
                info!("[K8S] Using default image (fallback): {}", img);
                return img.clone();
            }
        }

        // 4. 硬编码兜底(env 与 config 都没给)
        warn!("[K8S] No image config found, using hardcoded fallback");
        match service_type {
            // UserApp 实际走 create_deployment（image_override），不走 create_container/select_image
            // 此处兜底与 WebAgentRunner 共用，仅为 match 穷尽
            ServiceType::WebAgentRunner | ServiceType::UserApp => "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder:latest".to_string(),
            // UserAppBuilder 复用 dev-rcoder-agent-runner 镜像(与 ComputerAgentRunner 同镜像)
            ServiceType::ComputerAgentRunner | ServiceType::UserAppBuilder => {
                "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest".to_string()
            }
        }
    }

    /// Build resource requirements for K8s container from ServiceResourceLimits。
    ///
    /// 委派给共享 `build_decoupled_resources`（与 UserApp 侧 `build_app_resource_requirements`
    /// 共用 requests/limits 解耦策略，值一致）。仅在此做入参转换：ServiceResourceLimits 的
    /// memory(bytes f64）/cpu（核数 f64）归一化为 K8s Quantity 字符串。
    pub(crate) fn build_resource_requirements(
        limits: &ServiceResourceLimits,
    ) -> Option<ResourceRequirements> {
        // memory 字节 → Mi；cpu 核数 → 十进制字符串（K8s Quantity 原生接受）
        let cpu = limits.cpu.map(|c| format!("{}", c));
        let memory = limits
            .memory
            .map(|bytes| format!("{}Mi", (bytes / (1024.0 * 1024.0)) as i64));
        // ephemeral-storage：优先 ephemeral_storage_limit，回退 storage_size（与 PVC storage_size
        // 是两个独立配额；未显式指定时回退，与 app 侧对称）
        let ephemeral = limits
            .ephemeral_storage_limit
            .clone()
            .or_else(|| limits.storage_size.clone());
        super::k8s_app_helpers::build_decoupled_resources(cpu, memory, ephemeral)
    }

    /// 获取 K8s 模式 agent 容器的访问地址(Service FQDN)。
    /// Docker 模式不经过此函数(走 docker_runtime,用容器 IP)。
    fn get_container_access_address(&self, identifier: &str) -> String {
        // identifier 是完整 Pod 名(pod_info.container_name = {prefix}-{业务id}),
        // 真实 agent Service 名 = "{pod_name}-svc"(create_agent_service 创建)。
        // 复用 shared_types::build_k8s_service_fqdn 统一 FQDN 格式(与 rcoder 侧 handler、
        // 实际 K8s Service 名对齐)。不要过 agent_service_name/pod_name —— 那会再叠一层
        // service_container_prefix,产生 {prefix}-{prefix}-{id}-svc 双前缀(生产 bug 根因:
        // service_url 多出 rcoder-k8s- 前缀 → permission/cancel/stop transport error)。
        let fqdn = shared_types::build_k8s_service_fqdn(
            identifier,
            &self.namespace,
            &self.config.cluster_domain,
        );
        debug!(
            "[K8S] agent access address: identifier={} -> {}",
            identifier, fqdn
        );
        fqdn
    }

    /// Build container basic info from runtime container info
    pub(crate) async fn build_container_basic_info(
        &self,
        project_id: &str,
        pod_info: &RuntimeContainerInfo,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // service_url = {sts_name}-svc：container_name 已在 get_container_info 源头剥成 sts_name
        // （agent-runner STS pod 名 {sts_name}-0 的寻址基名），直接拼 Service FQDN。
        let access_address = self.get_container_access_address(&pod_info.container_name);

        Ok(ContainerBasicInfo {
            container_id: pod_info.container_id.clone(),
            container_name: pod_info.container_name.clone(),
            container_ip: pod_info.container_ip.clone(),
            internal_port: shared_types::HTTP_DEFAULT_PORT,
            external_port: 0,
            project_id: project_id.to_string(),
            status: String::from(pod_info.status.clone()),
            created_at: pod_info.created_at,
            service_url: format!(
                "http://{}:{}",
                access_address,
                shared_types::HTTP_DEFAULT_PORT
            ),
        })
    }
}
