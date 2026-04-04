//! Kubernetes runtime implementation
//!
//! This module provides `KubernetesRuntime` that creates pods in Kubernetes
//! instead of Docker containers, enabling rcoder to work in K8s environments.

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use chrono::Utc;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    ContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus,
    RuntimeContainerInfo,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{Container as K8sContainer, ContainerPort, EnvVar, Pod, PodSpec};
#[cfg(feature = "kubernetes")]
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta, PostParams};
#[cfg(feature = "kubernetes")]
use kube::client::Client;
#[cfg(feature = "kubernetes")]
use kube::Config;
#[cfg(feature = "kubernetes")]
use shared_types::{ContainerBasicInfo, ServiceResourceLimits, ServiceType};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use std::sync::Arc;
#[cfg(feature = "kubernetes")]
use tokio::sync::RwLock;
#[cfg(feature = "kubernetes")]
use tracing::info;

#[cfg(feature = "kubernetes")]
use crate::types::DockerManagerConfig;

/// Kubernetes runtime implementation using kube-rs
#[cfg(feature = "kubernetes")]
pub struct KubernetesRuntime {
    client: Client,
    namespace: String,
    config: KubernetesRuntimeConfig,
    /// Cache for pod information (using RwLock to avoid DashMap deadlocks)
    pod_cache: Arc<RwLock<std::collections::HashMap<String, RuntimeContainerInfo>>>,
}

#[cfg(feature = "kubernetes")]
#[derive(Debug, Clone)]
pub struct KubernetesRuntimeConfig {
    /// Namespace where pods are created
    pub namespace: String,
    /// Pod cleanup TTL in seconds
    pub pod_ttl_seconds: Option<u64>,
    /// Default image pull secret (if needed)
    pub image_pull_secret: Option<String>,
    /// Service account name for pods
    pub service_account_name: String,
}

#[cfg(feature = "kubernetes")]
impl KubernetesRuntime {
    /// Create a new Kubernetes runtime
    pub async fn new(config: DockerManagerConfig) -> ContainerRuntimeResult<Self> {
        // Load kube config from environment or in-cluster config
        let kube_config = Config::infer()
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Failed to load kube config: {}", e))
            })?;

        let client = Client::try_from(kube_config)
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Failed to create K8s client: {}", e))
            })?;

        let namespace =
            std::env::var("RCODER_K8S_NAMESPACE").unwrap_or_else(|_| "default".to_string());

        info!(
            "[K8S] Kubernetes runtime initialized, namespace: {}",
            namespace
        );

        Ok(Self {
            client,
            namespace: namespace.clone(),
            config: KubernetesRuntimeConfig {
                namespace: namespace.clone(),
                pod_ttl_seconds: config.container_ttl_seconds,
                image_pull_secret: None,
                service_account_name: "rcoder-pods-sa".to_string(),
            },
            pod_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Get the Pod API
    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Generate pod name from project_id
    fn pod_name(&self, project_id: &str, service_type: &ServiceType) -> String {
        format!("{}-{}", service_type.container_prefix(), project_id)
    }

    /// Extract pod status from Pod object
    fn extract_pod_status(pod: &Pod) -> ContainerRuntimeStatus {
        match &pod.status {
            Some(status) => match status.phase.as_deref() {
                Some("Running") => ContainerRuntimeStatus::Running,
                Some("Succeeded") => ContainerRuntimeStatus::Succeeded,
                Some("Failed") => ContainerRuntimeStatus::Failed,
                Some("Pending") => ContainerRuntimeStatus::Pending,
                Some(phase) => ContainerRuntimeStatus::Unknown(phase.to_string()),
                None => ContainerRuntimeStatus::Unknown("No phase".to_string()),
            },
            None => ContainerRuntimeStatus::Pending,
        }
    }

    /// Wait for pod to be ready
    async fn wait_for_pod_ready(&self, project_id: &str) -> ContainerRuntimeResult<()> {
        let timeout = std::time::Duration::from_secs(120);
        let start = std::time::Instant::now();

        while start.elapsed() < timeout {
            if let Some(pod) = self.find_container(project_id, &ServiceType::RCoder).await? {
                if pod.status == ContainerRuntimeStatus::Running {
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(ContainerRuntimeError::Timeout(
            "Pod did not become ready in time".to_string(),
        ))
    }

    /// Select image based on service type
    fn select_image(&self, service_type: ServiceType) -> String {
        match service_type {
            ServiceType::RCoder => std::env::var("RCODER_DOCKER_IMAGE")
                .unwrap_or_else(|_| "registry.yichamao.com/agent-runner:latest".to_string()),
            ServiceType::ComputerAgentRunner => std::env::var("RCODER_DOCKER_IMAGE_COMPUTER")
                .unwrap_or_else(|_| {
                    "registry.yichamao.com/computer-agent-runner:latest".to_string()
                }),
        }
    }

    /// Build container basic info from runtime container info
    async fn build_container_basic_info(
        &self,
        project_id: &str,
        pod_info: &RuntimeContainerInfo,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Ok(ContainerBasicInfo {
            container_id: pod_info.container_id.clone(),
            container_name: pod_info.container_name.clone(),
            container_ip: pod_info.container_ip.clone(),
            internal_port: shared_types::GRPC_DEFAULT_PORT,
            external_port: 0,
            project_id: project_id.to_string(),
            status: String::from(pod_info.status.clone()),
            created_at: pod_info.created_at,
            service_url: format!(
                "http://{}:{}",
                pod_info.container_ip, shared_types::GRPC_DEFAULT_PORT
            ),
        })
    }
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl ContainerRuntime for KubernetesRuntime {
    async fn create_container(
        &self,
        project_id: Option<&str>,
        _user_id: Option<&str>,
        _host_workspace_path: &str,
        service_type: ServiceType,
        _resource_limits: Option<ServiceResourceLimits>,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        let project_id =
            project_id.ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "project_id is required for K8s runtime".to_string(),
                )
            })?;

        let pod_name = self.pod_name(project_id, &service_type);

        // Check if pod already exists and is running
        if let Some(cached) = self.pod_cache.read().await.get(project_id) {
            if cached.status == ContainerRuntimeStatus::Running {
                info!("[K8S] Pod {} already exists and is running", pod_name);
                return self
                    .get_container_info(project_id)
                    .await?
                    .ok_or_else(|| {
                        ContainerRuntimeError::ContainerNotFound(project_id.to_string())
                    });
            }
        }

        let service_type_str = service_type.to_string();
        let image = self.select_image(service_type);

        // Build labels using BTreeMap (required by k8s-openapi)
        let labels: BTreeMap<String, String> = vec![
            ("app".to_string(), "rcoder".to_string()),
            ("project_id".to_string(), project_id.to_string()),
            ("service_type".to_string(), service_type_str.clone()),
        ]
        .into_iter()
        .collect();

        // Build Pod object using k8s-openapi types
        let pod: Pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![K8sContainer {
                    name: "agent".to_string(),
                    image: Some(image),
                    env: Some(vec![
                        EnvVar {
                            name: "PROJECT_ID".to_string(),
                            value: Some(project_id.to_string()),
                            ..Default::default()
                        },
                        EnvVar {
                            name: "SERVICE_TYPE".to_string(),
                            value: Some(service_type_str.clone()),
                            ..Default::default()
                        },
                    ]),
                    ports: Some(vec![ContainerPort {
                        container_port: shared_types::GRPC_DEFAULT_PORT as i32,
                        name: Some("grpc".to_string()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }],
                restart_policy: Some("Never".to_string()),
                service_account_name: Some(self.config.service_account_name.clone()),
                ..Default::default()
            }),
            status: None,
        };

        let pp = PostParams::default();
        self.pods()
            .create(&pp, &pod)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ContainerCreationError(format!("Failed to create pod: {}", e))
            })?;

        info!("[K8S] Pod {} created successfully", pod_name);

        // Wait for pod to be ready
        self.wait_for_pod_ready(project_id).await?;

        // Get pod info
        self.get_container_info(project_id).await?.ok_or_else(|| {
            ContainerRuntimeError::ContainerCreationError("Pod created but info not found".to_string())
        })
    }

    async fn get_container_info(
        &self,
        project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // Try cache first
        if let Some(cached) = self.pod_cache.read().await.get(project_id) {
            if cached.status == ContainerRuntimeStatus::Running {
                return Ok(Some(
                    self.build_container_basic_info(project_id, cached).await?,
                ));
            }
        }

        // Query K8s API - try to find pod with this project_id label
        let lp = ListParams::default().labels(&format!("project_id={}", project_id));
        let pods = self.pods().list(&lp).await.map_err(|e| {
            ContainerRuntimeError::K8sError(format!("Failed to list pods: {}", e))
        })?;

        for p in pods.items {
            let pod: Pod = p;
            let status = Self::extract_pod_status(&pod);
            let metadata = &pod.metadata;
            let uid = metadata.uid.clone().unwrap_or_default();
            let name = metadata.name.clone().unwrap_or_default();
            let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone()).unwrap_or_default();
            let created_at = metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| ts.0)
                .unwrap_or_else(|| Utc::now());

            let pod_info = RuntimeContainerInfo {
                container_id: uid,
                container_name: name,
                container_ip: pod_ip,
                status,
                created_at,
            };

            // Update cache if running
            if pod_info.status == ContainerRuntimeStatus::Running {
                self.pod_cache
                    .write()
                    .await
                    .insert(project_id.to_string(), pod_info.clone());
            }

            return Ok(Some(
                self.build_container_basic_info(project_id, &pod_info).await?,
            ));
        }

        Ok(None)
    }

    async fn find_container(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        // Check cache first
        if let Some(cached) = self.pod_cache.read().await.get(project_id) {
            return Ok(Some(cached.clone()));
        }

        // Query K8s
        let pod_name = self.pod_name(project_id, service_type);
        let pod: Pod = match self.pods().get(&pod_name).await {
            Ok(p) => p,
            Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(None),
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to get pod: {}",
                    e
                )))
            }
        };

        let status = Self::extract_pod_status(&pod);
        let metadata = &pod.metadata;
        let pod_info = RuntimeContainerInfo {
            container_id: metadata.uid.clone().unwrap_or_default(),
            container_name: metadata.name.clone().unwrap_or_default(),
            container_ip: pod.status.as_ref().and_then(|s| s.pod_ip.clone()).unwrap_or_default(),
            status,
            created_at: metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| ts.0)
                .unwrap_or_else(|| Utc::now()),
        };

        Ok(Some(pod_info))
    }

    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
        let pod_name = self.pod_name(project_id, &ServiceType::RCoder);

        self.pods()
            .delete(&pod_name, &DeleteParams::default())
            .await
            .map_err(|e| {
                ContainerRuntimeError::ContainerStopError(format!("Failed to delete pod: {}", e))
            })?;

        // Remove from cache
        self.pod_cache.write().await.remove(project_id);

        info!("[K8S] Pod {} deleted successfully", pod_name);
        Ok(())
    }

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(project_id, &ServiceType::RCoder)
            .await?
            .map(|p| p.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let lp = ListParams::default().labels("app=rcoder");
        let pods = self.pods().list(&lp).await.map_err(|e| {
            ContainerRuntimeError::K8sError(format!("Failed to list pods: {}", e))
        })?;

        let mut result = Vec::new();
        for p in pods.items {
            let pod: Pod = p;
            let status = Self::extract_pod_status(&pod);
            let metadata = &pod.metadata;
            let pod_info = RuntimeContainerInfo {
                container_id: metadata.uid.clone().unwrap_or_default(),
                container_name: metadata.name.clone().unwrap_or_default(),
                container_ip: pod.status.as_ref().and_then(|s| s.pod_ip.clone()).unwrap_or_default(),
                status,
                created_at: metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|ts| ts.0)
                    .unwrap_or_else(|| Utc::now()),
            };
            result.push(pod_info);
        }

        Ok(result)
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        let lp = ListParams::default().labels("app=rcoder");
        let _ = self.pods().delete_collection(&DeleteParams::default(), &lp).await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("Failed to cleanup pods: {}", e))
        })?;

        self.pod_cache.write().await.clear();
        Ok(())
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        // Try to list pods as a health check
        let lp = ListParams::default().limit(1);
        self.pods().list(&lp).await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("K8s health check failed: {}", e))
        })?;
        Ok(())
    }
}
