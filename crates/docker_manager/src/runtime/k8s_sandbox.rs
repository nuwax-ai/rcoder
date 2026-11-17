//! E2B 兼容沙箱的 K8s 编排适配器（feature `kubernetes`）。
//!
//! 与 agent-runner 的 STS 路径正交：ephemeral 沙箱是**裸 Pod**（无 PVC/STS/svc），
//! 生命周期完全由 SandboxStore 状态机 + 超时回收器驱动。
//!
//! Guaranteed QoS 是硬约束：requests == limits（Burstable 下 envd 的 oom wrapper
//! 会被 kubelet floor 拒绝，实测结论，见 build_config/e2b-sandbox/ENDPOINTS.md §7）。
//! RuntimeClass（如 kata-clh-runtime-rs）由模板规格透传；模板不可用应在注册表
//! 层拒绝，而非静默回退 runc。

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, PodSpec,
    ResourceRequirements, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::{api::ObjectMeta, Api, Client};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
#[error("sandbox k8s: {0}")]
pub struct SandboxK8sError(pub String);

fn err(context: &str, source: impl std::fmt::Display) -> SandboxK8sError {
    SandboxK8sError(format!("{context}: {source}"))
}

/// 裸沙箱 Pod 的创建参数（由引擎服务从模板规格+请求组装）。
pub struct SandboxPodParams {
    pub name: String,
    pub image: String,
    /// `None` = runc。
    pub runtime_class: Option<String>,
    pub cpu_millicores: i64,
    pub memory_mib: i64,
    pub envd_port: i32,
    /// Pod 最长存活秒数（activeDeadlineSeconds，不可续期的硬顶）。
    pub max_lifetime_seconds: i64,
    /// 透传给 envd 进程的容器级环境（M1 通常为空；进程 env 走 envd /init）。
    pub env: Vec<(String, String)>,
    /// 卷挂载：(PVC 名称, 容器内挂载路径)。
    pub volume_mounts: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct SandboxPodStatus {
    pub phase: String,
    pub pod_ip: Option<String>,
    pub pod_uid: Option<String>,
}

pub struct SandboxK8s {
    client: Client,
    namespace: String,
}

impl SandboxK8s {
    pub fn new(client: Client, namespace: String) -> Self {
        Self { client, namespace }
    }

    /// 独立发现 kube 配置并自建 client（不依赖 KubernetesRuntime 实例；
    /// 组合根装配用，与主运行时共享同一集群凭据来源）。
    pub async fn discover(namespace: String) -> Result<Self, SandboxK8sError> {
        let config = kube::Config::infer().await.map_err(|error| err("kube config", error))?;
        let client = Client::try_from(config).map_err(|error| err("kube client", error))?;
        Ok(Self::new(client, namespace))
    }

    /// 复用既有 KubernetesRuntime 的 client/namespace（字段 crate 可见）。
    pub fn from_runtime(
        runtime: &super::kubernetes_runtime::KubernetesRuntime,
        namespace: &str,
    ) -> Self {
        Self::new(runtime.client.clone(), namespace.to_string())
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }


    /// 创建 CephFS PVC（RWX，多沙箱可并发挂载）。
    pub async fn create_pvc(
        &self,
        pvc_name: &str,
        storage_class: &str,
        size_gib: i32,
    ) -> Result<(), SandboxK8sError> {
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(pvc_name.to_string()),
                labels: Some(std::collections::BTreeMap::from([
                    ("app".to_string(), "e2b-sandbox".to_string()),
                    ("e2b-volume".to_string(), "true".to_string()),
                ])),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteMany".to_string()]),
                storage_class_name: Some(storage_class.to_string()),
                resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                    requests: Some(std::collections::BTreeMap::from([(
                        "storage".to_string(),
                        Quantity(format!("{}Gi", size_gib)),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), &self.namespace)
            .create(&kube::api::PostParams::default(), &pvc)
            .await
            .map(|_| ())
            .map_err(|error| err("create pvc", error))
    }

    /// 删除 PVC；404 视为已删除。
    pub async fn delete_pvc(&self, pvc_name: &str) -> Result<bool, SandboxK8sError> {
        match Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), &self.namespace)
            .delete(pvc_name, &kube::api::DeleteParams::default())
            .await
        {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(response)) if response.code == 404 => Ok(false),
            Err(error) => Err(err("delete pvc", error)),
        }
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// 创建裸沙箱 Pod（幂等由调用方以 sandbox_id 命名承担；同名冲突报错）。
    pub async fn create_pod(&self, params: &SandboxPodParams) -> Result<(), SandboxK8sError> {
        let cpu = format!("{}m", params.cpu_millicores);
        let memory = format!("{}Mi", params.memory_mib);
        // Guaranteed：requests 与 limits 严格同值。
        let resources = ResourceRequirements {
            requests: Some(std::collections::BTreeMap::from([
                ("cpu".to_string(), Quantity(cpu.clone())),
                ("memory".to_string(), Quantity(memory.clone())),
            ])),
            limits: Some(std::collections::BTreeMap::from([
                ("cpu".to_string(), Quantity(cpu)),
                ("memory".to_string(), Quantity(memory)),
            ])),
            ..Default::default()
        };
        let container = Container {
            name: "envd".to_string(),
            image: Some(params.image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            ports: Some(vec![ContainerPort {
                container_port: params.envd_port,
                protocol: Some("TCP".to_string()),
                name: Some("envd".to_string()),
                ..Default::default()
            }]),
            resources: Some(resources),
            volume_mounts: if params.volume_mounts.is_empty() {
                None
            } else {
                Some(
                    params
                        .volume_mounts
                        .iter()
                        .map(|(pvc_name, mount_path)| VolumeMount {
                            name: format!("vol-{}", pvc_name.trim_start_matches("e2b-vol-")),
                            mount_path: mount_path.clone(),
                            ..Default::default()
                        })
                        .collect(),
                )
            },
            env: Some(
                params
                    .env
                    .iter()
                    .map(|(key, value)| k8s_openapi::api::core::v1::EnvVar {
                        name: key.clone(),
                        value: Some(value.clone()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        let volumes: Vec<Volume> = params
            .volume_mounts
            .iter()
            .map(|(pvc_name, _)| Volume {
                name: format!("vol-{}", pvc_name.trim_start_matches("e2b-vol-")),
                persistent_volume_claim: Some(k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                    claim_name: pvc_name.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();
        let spec = PodSpec {
            volumes: if volumes.is_empty() { None } else { Some(volumes) },
            containers: vec![container],
            runtime_class_name: params.runtime_class.clone(),
            active_deadline_seconds: Some(params.max_lifetime_seconds),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        };
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(params.name.clone()),
                labels: Some(std::collections::BTreeMap::from([
                    ("app".to_string(), "e2b-sandbox".to_string()),
                    ("e2b-sandbox-id".to_string(), params.name.clone()),
                ])),
                ..Default::default()
            },
            spec: Some(spec),
            ..Default::default()
        };
        self.pods()
            .create(&kube::api::PostParams::default(), &pod)
            .await
            .map(|_| ())
            .map_err(|error| err("create pod", error))
    }

    /// 删除沙箱 Pod；返回是否存在（404 视为已删除，幂等成功）。
    pub async fn delete_pod(&self, name: &str) -> Result<bool, SandboxK8sError> {
        match self.pods().delete(name, &kube::api::DeleteParams::default()).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(response)) if response.code == 404 => Ok(false),
            Err(error) => Err(err("delete pod", error)),
        }
    }

    pub async fn pod_status(&self, name: &str) -> Result<Option<SandboxPodStatus>, SandboxK8sError> {
        match self.pods().get(name).await {
            Ok(pod) => Ok(Some(SandboxPodStatus {
                phase: pod
                    .status
                    .as_ref()
                    .and_then(|status| status.phase.clone())
                    .unwrap_or_else(|| "Unknown".to_string()),
                pod_ip: pod.status.as_ref().and_then(|status| status.pod_ip.clone()),
                pod_uid: pod.metadata.uid.clone(),
            })),
            Err(kube::Error::Api(response)) if response.code == 404 => Ok(None),
            Err(error) => Err(err("get pod", error)),
        }
    }

    /// 轮询至 Running 且分配 PodIP（M1 简单轮询；watch 化是 M3 优化项）。
    pub async fn wait_ready(
        &self,
        name: &str,
        deadline: Duration,
    ) -> Result<SandboxPodStatus, SandboxK8sError> {
        let started = std::time::Instant::now();
        loop {
            let status = self.pod_status(name).await?;
            match status {
                Some(status) if status.phase == "Running" && status.pod_ip.is_some() => {
                    return Ok(status)
                }
                Some(status) if matches!(status.phase.as_str(), "Failed" | "Succeeded") => {
                    return Err(SandboxK8sError(format!(
                        "sandbox pod {name} reached terminal phase {}",
                        status.phase
                    )))
                }
                _ => {}
            }
            if started.elapsed() >= deadline {
                return Err(SandboxK8sError(format!(
                    "sandbox pod {name} not ready within {:?}",
                    deadline
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
