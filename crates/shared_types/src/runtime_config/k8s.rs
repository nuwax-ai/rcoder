//! K8s 运行时专用配置结构(与 docker_config 完全分家)
//!
//! 背景:rcoder 同时支持 docker 与 k8s 两种运行时,但二者配置需求已经分叉——
//! `docker_config.multi_image_config.services`(基于 `ServiceImageConfig`)面向 Docker 的
//! bind mount / host_path 语义;而 K8s 需要 PVC / emptyDir / ConfigMap 卷、sidecar 容器、
//! 以及独立的 workspace 解析路径。混用同一套结构会让 docker/k8s 逻辑互相污染。
//!
//! 因此本模块定义 **K8s 专用、自包含** 的配置族:
//! - `KubernetesConfig`:顶层,包含 `global_defaults` + `services` 哈希表
//! - `K8sServiceConfig`:单个服务(image 族 / env / command / 卷 / 挂载 / sidecar 全在这)
//! - `K8sVolumeSpec` / `K8sVolumeMountSpec` / `K8sSidecarSpec` / `K8sVolumeType`
//!
//! K8s 构建器(`docker_manager::runtime::kubernetes_runtime`)只读本模块,
//! 不再读 `docker_config`;docker 运行时只读 `docker_config`,不读本模块。
//! `AppConfig` 无 `deny_unknown_fields`,故 docker 部署下 `kubernetes_config` 键被忽略,
//! k8s 部署下 `docker_config` 键被忽略——天然隔离。
//!
//! **策略约束**:`K8sVolumeType::HostPath` 被禁止(K8s builder 翻译时跳过并告警),
//! 因为动态 agent pod 不可绑宿主机路径(多节点漂移、安全)。

use crate::{ServiceResourceLimits, ServiceType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============================================================================
// 卷类型
// ============================================================================

/// K8s 卷类型(对应 kube VolumeSource 的一个子集)
///
/// **HostPath 被策略禁用**:虽然枚举里保留以显式拒绝(翻译时跳过+告警),
/// 但 K8s builder 不会为它生成 `HostPathVolumeSource`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum K8sVolumeType {
    /// emptyDir(随 Pod 生命周期,适合容器间共享临时数据,如日志中转)
    #[default]
    EmptyDir,
    /// 已存在的 PVC(引用集群里预先创建的 PersistentVolumeClaim)
    Pvc,
    /// ConfigMap(把 ConfigMap 资源作为卷挂载)
    ConfigMap,
    /// HostPath —— **策略禁用**,仅用于显式拒绝+告警,不会真正生成卷
    HostPath,
}

impl std::fmt::Display for K8sVolumeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyDir => write!(f, "emptyDir"),
            Self::Pvc => write!(f, "pvc"),
            Self::ConfigMap => write!(f, "configMap"),
            Self::HostPath => write!(f, "hostPath"),
        }
    }
}

impl std::str::FromStr for K8sVolumeType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // 大小写不敏感 + 容忍 camelCase / snake_case / kebab-case
        let normalized = s.trim().to_lowercase();
        match normalized.as_str() {
            "emptydir" | "empty_dir" | "empty-dir" => Ok(Self::EmptyDir),
            "pvc" | "persistentvolumeclaim" | "persistent_volume_claim" => Ok(Self::Pvc),
            "configmap" | "config_map" => Ok(Self::ConfigMap),
            "hostpath" | "host_path" | "host-path" => Ok(Self::HostPath),
            other => Err(format!("unknown k8s volume_type '{}'", other)),
        }
    }
}

// 自定义 Deserialize:走 FromStr,容忍大小写/下划线/中划线差异
impl<'de> Deserialize<'de> for K8sVolumeType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<Self>().map_err(serde::de::Error::custom)
    }
}

// ============================================================================
// 卷 / 挂载 / sidecar 规格
// ============================================================================

/// K8s 卷规格(builder 翻译为 kube `Volume`)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct K8sVolumeSpec {
    /// 卷名(必须,且**不能**是 "workspace"——该名由 builder 硬编码占用,冲突会被跳过+告警)
    pub name: String,
    /// 卷类型(默认 emptyDir)。HostPath 被禁用
    #[serde(default)]
    pub volume_type: K8sVolumeType,
    /// emptyDir 的 sizeLimit(如 "1Gi");仅 EmptyDir 生效
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_limit: Option<String>,
    /// 引用的 PVC 名;仅 Pvc 生效
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_name: Option<String>,
    /// 引用的 ConfigMap 名;仅 ConfigMap 生效
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_name: Option<String>,
    /// 是否只读(Pvc / ConfigMap 生效;emptyDir 忽略)
    #[serde(default)]
    pub read_only: bool,
}

/// K8s 卷挂载规格(builder 翻译为 kube `VolumeMount`,挂到某个容器上)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct K8sVolumeMountSpec {
    /// 引用的卷名(须与某个 K8sVolumeSpec.name 对应)
    pub name: String,
    /// 容器内挂载路径
    pub mount_path: String,
    /// subPath(可选,挂卷的某个子目录)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
    /// 是否只读
    #[serde(default)]
    pub read_only: bool,
}

/// K8s sidecar 容器规格(builder 翻译为 kube `Container`,与主 agent 容器同 Pod)
///
/// 典型用途:log-collector sidecar —— tail 容器内日志文件到 stdout,
/// 让 fluent-bit(只收 stdout)能把容器内文件日志带进 Loki。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct K8sSidecarSpec {
    /// sidecar 容器名
    pub name: String,
    /// 镜像(完整引用,含 registry/tag)
    pub image: String,
    /// 镜像拉取策略(默认 IfNotPresent)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,
    /// 启动命令(为空则走镜像 ENTRYPOINT/CMD)
    #[serde(default)]
    pub command: Vec<String>,
    /// 该 sidecar 的卷挂载(翻译为该容器的 VolumeMount 列表)
    #[serde(default)]
    pub volume_mounts: Vec<K8sVolumeMountSpec>,
    /// 资源限制(复用 ServiceResourceLimits,memory/cpu 等)
    #[serde(default)]
    pub resources: ServiceResourceLimits,
}

// ============================================================================
// 服务配置(自包含,K8s 专用,不复用 ServiceImageConfig)
// ============================================================================

/// K8s 服务配置(自包含:镜像族 / env / command / workspace 路径 / 资源 / 卷 / 挂载 / sidecar)
///
/// 与 `ServiceImageConfig`(docker 用)平行但**不复用**——字段集合针对 K8s 裁剪
/// (无 host bind mount、无 network_mode、无 container_path_template,
/// 增加 volumes / volume_mounts / sidecars)。
///
/// `service_type` 字段不实现 serde default(`ServiceType` 无 Default),
/// 故 YAML 中必须显式给出 `service_type`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sServiceConfig {
    /// 服务类型(YAML 必填)
    pub service_type: ServiceType,
    /// 通用镜像(优先级最高,指定则忽略架构特定镜像)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// ARM64 架构专用镜像
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm64_image: Option<String>,
    /// AMD64 架构专用镜像
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amd64_image: Option<String>,
    /// 默认回退镜像
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_image: Option<String>,
    /// 镜像标签前缀(用于容器名前缀;缺省回退到 service_type.container_prefix())
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_tag_prefix: Option<String>,
    /// 是否启用该服务类型
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 服务特定环境变量
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// 容器启动命令(为空则走镜像 ENTRYPOINT/CMD)
    #[serde(default)]
    pub command: Vec<String>,
    /// workspace 解析路径(rcoder 容器内基准路径,web→/app/project_workspace,
    /// computer→/app/computer-project-workspace);未配置时按 service_type 默认推导
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_resolution_path: Option<String>,
    /// 资源限制
    #[serde(default)]
    pub resource_limits: ServiceResourceLimits,
    /// 额外卷(追加到 builder 硬编码的 workspace PVC 之后)
    #[serde(default)]
    pub volumes: Vec<K8sVolumeSpec>,
    /// 主 agent 容器的额外卷挂载(追加到 workspace 挂载之后)
    #[serde(default)]
    pub volume_mounts: Vec<K8sVolumeMountSpec>,
    /// sidecar 容器(与主 agent 同 Pod)
    #[serde(default)]
    pub sidecars: Vec<K8sSidecarSpec>,
}

fn default_enabled() -> bool {
    true
}

impl K8sServiceConfig {
    /// 根据 service_type 产生默认的 workspace_resolution_path
    fn default_workspace_resolution_path(service_type: &ServiceType) -> String {
        match service_type {
            ServiceType::WebAgentRunner => crate::paths::WORKSPACE_ROOT.to_string(),
            ServiceType::ComputerAgentRunner => crate::paths::COMPUTER_WORKSPACE_ROOT.to_string(),
            ServiceType::Userapp => "/app/app-workspace".to_string(),
            // UserappBuilder: per-app PVC 挂载点
            ServiceType::UserappBuilder => "/app/userapp-workspace".to_string(),
        }
    }

    /// 获取 workspace 解析路径(显式配置优先,否则按 service_type 默认)
    pub fn effective_workspace_resolution_path(&self) -> String {
        self.workspace_resolution_path
            .clone()
            .unwrap_or_else(|| Self::default_workspace_resolution_path(&self.service_type))
    }

    /// 获取 workspace 在子容器内的挂载路径
    ///
    /// 优先读环境变量 `PROJECT_WORKSPACE_BASE`(config.yml 配置),
    /// 回退到 `effective_workspace_resolution_path()`。镜像 `ServiceImageConfig::workspace_container_path`。
    pub fn workspace_container_path(&self) -> String {
        self.environment
            .get("PROJECT_WORKSPACE_BASE")
            .cloned()
            .unwrap_or_else(|| self.effective_workspace_resolution_path())
    }

    /// 容器名前缀(image_tag_prefix 优先,否则 service_type.container_prefix())
    pub fn container_prefix(&self) -> &str {
        self.image_tag_prefix
            .as_deref()
            .unwrap_or_else(|| self.service_type.container_prefix())
    }

    /// 按平台选镜像(image 优先 → 架构特定 → default)
    pub fn get_image_for_platform(&self, platform: &str) -> Option<String> {
        if let Some(ref image) = self.image {
            return Some(image.clone());
        }
        match platform {
            "linux/arm64" => self
                .arm64_image
                .clone()
                .or_else(|| self.default_image.clone()),
            "linux/amd64" => self
                .amd64_image
                .clone()
                .or_else(|| self.default_image.clone()),
            _ => {
                tracing::warn!(
                    "[K8S_CONFIG] unknown platform '{}', using default image",
                    platform
                );
                self.default_image.clone()
            }
        }
    }
}

// ============================================================================
// 顶层结构
// ============================================================================

/// K8s 全局默认配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct K8sGlobalDefaults {
    /// 镜像仓库前缀(如 `nuwax-.../nuwax-k8s-test`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_prefix: Option<String>,
}

/// K8s 顶层配置(AppConfig.kubernetes_config)
///
/// 与 `MultiImageConfig`(docker 用)平行但独立。`get_service_config` 复用相同的
/// ServiceType key + 旧 "rcoder" 兼容逻辑。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KubernetesConfig {
    /// 全局默认(目前主要是 registry_prefix)
    #[serde(default)]
    pub global_defaults: K8sGlobalDefaults,
    /// 各服务配置,key 为 service_type 字符串(或旧名 "rcoder")
    #[serde(default)]
    pub services: HashMap<String, K8sServiceConfig>,
}

impl KubernetesConfig {
    /// 获取指定服务类型的配置
    ///
    /// 先按 `service_type.to_string()` 查;找不到再按旧名 "rcoder"(仅 WebAgentRunner)兼容。
    /// 镜像 `MultiImageConfig::get_service_config` 的查找逻辑。
    pub fn get_service_config(&self, service_type: &ServiceType) -> Option<&K8sServiceConfig> {
        // 1. 按规范名查
        let key = service_type.to_string();
        if let Some(cfg) = self.services.get(&key) {
            return Some(cfg);
        }
        // 2. 旧名兼容(WebAgentRunner ↔ "rcoder")
        match service_type {
            ServiceType::WebAgentRunner => self.services.get("rcoder"),
            ServiceType::ComputerAgentRunner => None,
            ServiceType::Userapp | ServiceType::UserappBuilder => None,
        }
    }

    /// 全局镜像仓库前缀(缺省空串)
    pub fn get_registry_prefix(&self) -> String {
        self.global_defaults
            .registry_prefix
            .clone()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_type_parse_tolerant() {
        assert_eq!(
            "emptyDir".parse::<K8sVolumeType>().unwrap(),
            K8sVolumeType::EmptyDir
        );
        assert_eq!(
            "empty_dir".parse::<K8sVolumeType>().unwrap(),
            K8sVolumeType::EmptyDir
        );
        assert_eq!(
            "EMPTYDIR".parse::<K8sVolumeType>().unwrap(),
            K8sVolumeType::EmptyDir
        );
        assert_eq!("pvc".parse::<K8sVolumeType>().unwrap(), K8sVolumeType::Pvc);
        assert_eq!(
            "configMap".parse::<K8sVolumeType>().unwrap(),
            K8sVolumeType::ConfigMap
        );
        assert_eq!(
            "hostPath".parse::<K8sVolumeType>().unwrap(),
            K8sVolumeType::HostPath
        );
        assert!("bogus".parse::<K8sVolumeType>().is_err());
    }

    #[test]
    fn test_volume_type_default() {
        assert_eq!(K8sVolumeType::default(), K8sVolumeType::EmptyDir);
    }

    #[test]
    fn test_k8s_service_config_roundtrip() {
        let yaml = r#"
service_type: web-agent-runner
image: registry.example.com/rcoder:latest
arm64_image: registry.example.com/rcoder:arm64
amd64_image: registry.example.com/rcoder:amd64
default_image: registry.example.com/rcoder:latest
image_tag_prefix: rcoder-k8s
enabled: true
environment:
  RUST_LOG: info
  PROJECT_WORKSPACE_BASE: /app/project_workspace
command: ["/app/bin/agent_runner", "--port", "8086"]
resource_limits:
  memory_limit: 2147483648
  cpu_limit: 2.0
volumes:
  - name: container-logs
    volume_type: emptyDir
volume_mounts:
  - name: container-logs
    mount_path: /app/container-logs
sidecars:
  - name: log-collector
    image: registry.example.com/alpine:3.22.4
    image_pull_policy: IfNotPresent
    command: ["/bin/sh", "-c", "tail -F /app/container-logs/**/*.log"]
    volume_mounts:
      - name: container-logs
        mount_path: /app/container-logs
        read_only: true
    resources:
      memory_limit: 134217728
      cpu_limit: 0.2
"#;
        let cfg: K8sServiceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.service_type, ServiceType::WebAgentRunner);
        assert_eq!(
            cfg.image.as_deref(),
            Some("registry.example.com/rcoder:latest")
        );
        assert!(cfg.enabled);
        assert_eq!(cfg.container_prefix(), "rcoder-k8s");
        // PROJECT_WORKSPACE_BASE 优先
        assert_eq!(cfg.workspace_container_path(), "/app/project_workspace");
        assert_eq!(cfg.volumes.len(), 1);
        assert_eq!(cfg.volumes[0].volume_type, K8sVolumeType::EmptyDir);
        assert_eq!(cfg.sidecars.len(), 1);
        assert_eq!(cfg.sidecars[0].name, "log-collector");
        // resource_limits 兼容旧键名 alias
        assert_eq!(cfg.resource_limits.memory, Some(2_147_483_648.0));
        assert_eq!(cfg.resource_limits.cpu, Some(2.0));
    }

    #[test]
    fn test_kubernetes_config_get_service_legacy_key() {
        // 旧名 "rcoder" 兼容 WebAgentRunner
        let mut kc = KubernetesConfig::default();
        kc.services.insert(
            "rcoder".to_string(),
            K8sServiceConfig {
                service_type: ServiceType::WebAgentRunner,
                image: Some("img".to_string()),
                enabled: true,
                ..parse_minimal_defaults()
            },
        );
        let found = kc.get_service_config(&ServiceType::WebAgentRunner);
        assert!(found.is_some());
        assert_eq!(found.unwrap().image.as_deref(), Some("img"));
        // 规范名也插一份,验证规范名优先
        kc.services.insert(
            "web-agent-runner".to_string(),
            K8sServiceConfig {
                service_type: ServiceType::WebAgentRunner,
                image: Some("canonical".to_string()),
                enabled: true,
                ..parse_minimal_defaults()
            },
        );
        assert_eq!(
            kc.get_service_config(&ServiceType::WebAgentRunner)
                .unwrap()
                .image
                .as_deref(),
            Some("canonical")
        );
    }

    #[test]
    fn test_workspace_container_path_fallback() {
        let cfg = K8sServiceConfig {
            service_type: ServiceType::ComputerAgentRunner,
            image: None,
            arm64_image: None,
            amd64_image: None,
            default_image: None,
            image_tag_prefix: None,
            enabled: true,
            environment: HashMap::new(), // 无 PROJECT_WORKSPACE_BASE
            command: vec![],
            workspace_resolution_path: None,
            resource_limits: ServiceResourceLimits::default(),
            volumes: vec![],
            volume_mounts: vec![],
            sidecars: vec![],
        };
        // 回退到 service_type 默认
        assert_eq!(
            cfg.workspace_container_path(),
            "/app/computer-project-workspace"
        );
        // 显式 workspace_resolution_path 优先于 service_type 默认
        let mut cfg2 = cfg.clone();
        cfg2.workspace_resolution_path = Some("/custom/ws".to_string());
        assert_eq!(cfg2.workspace_container_path(), "/custom/ws");
        // PROJECT_WORKSPACE_BASE 环境变量最高优先
        let mut cfg3 = cfg.clone();
        cfg3.environment.insert(
            "PROJECT_WORKSPACE_BASE".to_string(),
            "/from/env".to_string(),
        );
        assert_eq!(cfg3.workspace_container_path(), "/from/env");
    }

    /// 验证 helm 渲染出的 log-collector sidecar 命令(shell 含 $f / $(find) / \" / >> / {})
    /// 经 serde_yaml 反序列化后,shell 元字符原样保留 —— 这是日志采集链路的关键。
    /// 命令串直接取自 templates/_helpers.tpl 的 nuwax.rcoderKubernetesConfig partial 渲染结果。
    #[test]
    fn test_helm_rendered_log_collector_command_survives() {
        let yaml = r#"
sidecars:
  - name: "log-collector"
    image: "registry.example.com/nuwax-k8s-test/alpine:3.22.4"
    image_pull_policy: "IfNotPresent"
    command:
      - "/bin/sh"
      - "-c"
      - "while true; do for f in $(find /app/container-logs -type f -name '*.log' 2>/dev/null); do grep -qxF \"$f\" /tmp/tailed 2>/dev/null || { echo \"$f\" >> /tmp/tailed; echo \"=== $f ===\"; tail -n+1 -f \"$f\" 2>/dev/null & }; done; sleep 10; done"
    volume_mounts:
      - name: "container-logs"
        mount_path: "/app/container-logs"
        read_only: true
    resources:
      memory_limit: 134217728
      cpu_limit: 0.2
"#;
        // K8sSidecarSpec 单独解析(命令是它最脆弱的字段)
        #[derive(Deserialize)]
        struct Wrap {
            sidecars: Vec<K8sSidecarSpec>,
        }
        let w: Wrap = serde_yaml::from_str(yaml).unwrap();
        let sc = &w.sidecars[0];
        assert_eq!(sc.name, "log-collector");
        assert_eq!(sc.image_pull_policy.as_deref(), Some("IfNotPresent"));
        // shell 元字符必须原样保留(YAML 双引号里 \" 还原成 ")
        let cmd = &sc.command[2];
        assert!(
            cmd.contains("$(find /app/container-logs -type f -name '*.log'"),
            "cmd: {cmd}"
        );
        assert!(cmd.contains("\"$f\""), "double-quote must survive: {cmd}");
        assert!(cmd.contains(">> /tmp/tailed"), "cmd: {cmd}");
        assert!(cmd.contains("=== $f ==="), "cmd: {cmd}");
        assert_eq!(sc.command[0], "/bin/sh");
        assert_eq!(sc.command[1], "-c");
        // 资源限制旧键名 alias 解析
        assert_eq!(sc.resources.memory, Some(134_217_728.0));
        assert_eq!(sc.resources.cpu, Some(0.2));
    }

    #[test]
    fn test_get_image_for_platform_priority() {
        let cfg = K8sServiceConfig {
            service_type: ServiceType::WebAgentRunner,
            image: Some("generic".to_string()),
            arm64_image: Some("arm".to_string()),
            amd64_image: Some("amd".to_string()),
            default_image: Some("def".to_string()),
            image_tag_prefix: None,
            enabled: true,
            environment: HashMap::new(),
            command: vec![],
            workspace_resolution_path: None,
            resource_limits: ServiceResourceLimits::default(),
            volumes: vec![],
            volume_mounts: vec![],
            sidecars: vec![],
        };
        // image 优先
        assert_eq!(
            cfg.get_image_for_platform("linux/arm64"),
            Some("generic".to_string())
        );
        // 无 image → 架构特定 → default
        let mut cfg2 = cfg.clone();
        cfg2.image = None;
        assert_eq!(
            cfg2.get_image_for_platform("linux/arm64"),
            Some("arm".to_string())
        );
        assert_eq!(
            cfg2.get_image_for_platform("linux/amd64"),
            Some("amd".to_string())
        );
        // 无架构特定 → default
        let mut cfg3 = cfg2.clone();
        cfg3.arm64_image = None;
        assert_eq!(
            cfg3.get_image_for_platform("linux/arm64"),
            Some("def".to_string())
        );
    }

    /// 测试辅助:构造一个全 Option 字段为 None、集合为空的 K8sServiceConfig
    fn parse_minimal_defaults() -> K8sServiceConfig {
        K8sServiceConfig {
            service_type: ServiceType::WebAgentRunner,
            image: None,
            arm64_image: None,
            amd64_image: None,
            default_image: None,
            image_tag_prefix: None,
            enabled: true,
            environment: HashMap::new(),
            command: vec![],
            workspace_resolution_path: None,
            resource_limits: ServiceResourceLimits::default(),
            volumes: vec![],
            volume_mounts: vec![],
            sidecars: vec![],
        }
    }
}
