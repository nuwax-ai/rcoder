//! Physical resource identities captured before an application deletion begins.
//! These receipts must never be reconstructed from names after a mutation.
use serde::{Deserialize, Serialize};

/// An application-wide runtime lease. Dropping it is cancellation-safe; successful
/// callers explicitly await release before reporting completion.
#[async_trait::async_trait]
pub trait AppOperationLease: Send + Sync {
    fn receipt(&self) -> Option<crate::UserAppOperationLeaseReceipt> {
        None
    }
    async fn release(self: Box<Self>) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppResourceKind {
    Container,
    Deployment,
    StatefulSet,
    Service,
    ConfigMap,
    Secret,
    HttpRoute,
    PersistentVolumeClaim,
    /// Host filesystem location (Docker bind-mount source). The identity is
    /// the resolved source path, not a cluster-scoped object.
    HostPath,
    /// Content-addressed local artifact file (for example a Docker restart
    /// archive). `uid` carries the payload digest.
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppResourceIdentity {
    pub kind: AppResourceKind,
    pub name: String,
    pub uid: String,
    pub resource_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppDeletionSnapshot {
    pub app_id: String,
    pub operation_id: String,
    pub resources: Vec<AppResourceIdentity>,
}

/// Captured resource target bound to one admitted control operation. Persist this
/// receipt before runtime mutation; never rediscover the target by name on retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppMutationTarget {
    pub context: crate::UserAppExecutionContext,
    pub resource: AppResourceIdentity,
}

/// Operation-bound restart archive for application (prod) compute. Restoring
/// always begins at zero replicas; the volume witnesses pin the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRestartTemplate {
    pub source: UserAppMutationTarget,
    pub archive: AppResourceIdentity,
    pub volumes: Vec<AppResourceIdentity>,
}

/// Restart startup witness. Flattening retains the original mutation target
/// shape while adding volume identities and an explicit single-write capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppComputeStartTarget {
    #[serde(flatten)]
    pub target: UserAppMutationTarget,
    #[serde(default)]
    pub compute_start_single_write: bool,
    #[serde(default)]
    pub volumes: Vec<AppResourceIdentity>,
    /// Platform-default image the restart operation froze at admission. The
    /// compute-start patch rolls the workload onto it; `None` keeps the image
    /// (plain restart / wake — wake never waits on image pulls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_image: Option<String>,
}

impl UserAppComputeStartTarget {
    pub fn verify_same_volumes(&self, other: &Self) -> Result<(), String> {
        if self.compute_start_single_write != other.compute_start_single_write
            || self.volumes.len() != other.volumes.len()
            || self
                .volumes
                .iter()
                .zip(&other.volumes)
                .any(|(a, b)| a.kind != b.kind || a.name != b.name || a.uid != b.uid)
        {
            return Err(
                "Restart volume identity changed or the original witness is missing".into(),
            );
        }
        Ok(())
    }
}

/// Confirmed deletion boundaries. A remote acknowledgement without confirmed
/// disappearance must never advance these stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAppDeletionStage {
    Captured,
    ComputeRemoved,
    ProductionStorageRemoved,
    DevelopmentRemoved,
}

/// Complete deletion evidence, bound to one durable operation. This is not an
/// execution lease: an uncertain writer still requires reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAppDeletionCheckpoint {
    pub stage: UserAppDeletionStage,
    pub schema_version: u32,
    pub context: crate::UserAppExecutionContext,
    pub kind: crate::UserAppOperationKind,
    pub production: AppDeletionSnapshot,
    pub development: Option<crate::UserappDevDeletionReceipt>,
}

impl UserAppDeletionCheckpoint {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("Unsupported application deletion checkpoint version".into());
        }
        self.context.validate_identity(&self.context.app_id)?;
        let needs_development = match self.kind {
            crate::UserAppOperationKind::DeleteCompute => false,
            crate::UserAppOperationKind::PurgeResources
            | crate::UserAppOperationKind::DeleteApplication => true,
            _ => return Err("Operation kind does not support this deletion checkpoint".into()),
        };
        if self.production.app_id != self.context.app_id
            || self.development.is_some() != needs_development
        {
            return Err("Deletion checkpoint application or resource scope mismatch".into());
        }
        if !needs_development
            && matches!(
                self.stage,
                UserAppDeletionStage::ProductionStorageRemoved
                    | UserAppDeletionStage::DevelopmentRemoved
            )
        {
            return Err("Compute-only deletion cannot complete storage cleanup".into());
        }
        validate_deletion_resources(&self.production.operation_id, &self.production.resources)?;
        if let Some(development) = &self.development {
            if development.runtime.app_id != self.context.app_id {
                return Err(
                    "Development deletion checkpoint belongs to another application".into(),
                );
            }
            validate_deletion_resources(
                &development.runtime.operation_id,
                &development.runtime.resources,
            )?;
            if development.registry.as_ref().is_some_and(|identity| {
                identity.generation.is_empty() || identity.container_id.is_empty()
            }) {
                return Err("Development registry identity is incomplete".into());
            }
        }
        Ok(())
    }

    /// Verify stored evidence against the authoritative operation, not caller IDs.
    pub fn validate_operation(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<(), String> {
        self.validate()?;
        if self.context.app_id != operation.app_id
            || self.context.lifecycle_id != operation.lifecycle_id
            || self.context.operation_id != operation.operation_id
            || operation.executor_id.as_deref() != Some(self.context.executor_id.as_str())
            || self.context.request_fingerprint != operation.request_fingerprint
            || self.kind != operation.kind
        {
            return Err("Deletion checkpoint does not belong to the stored operation".into());
        }
        Ok(())
    }
}

fn validate_deletion_resources(
    operation_id: &str,
    resources: &[AppResourceIdentity],
) -> Result<(), String> {
    crate::validate_identifier(operation_id, "resource_operation_id")?;
    for resource in resources {
        if resource.name.is_empty() || resource.uid.is_empty() {
            return Err("Deletion resource identity is incomplete".into());
        }
        if resource.kind != AppResourceKind::Container
            && resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err("Kubernetes deletion resource version is missing".into());
        }
    }
    Ok(())
}

/// Original storage-destruction scope. Production storage destruction retains
/// its existing contract of also removing development resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAppStorageDestruction {
    pub context: crate::UserAppExecutionContext,
    pub production: Option<AppDeletionSnapshot>,
    pub development: crate::UserappDevDeletionReceipt,
}

/// Evidence for clearing contents. This records selected targets, not authority
/// to replay an uncertain operation or to substitute replacement resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAppStorageClear {
    pub context: crate::UserAppExecutionContext,
    pub target: UserAppStorageClearTarget,
}

impl UserAppStorageClear {
    pub fn validate(&self) -> Result<(), String> {
        self.context.validate_identity(&self.context.app_id)?;
        match &self.target {
            UserAppStorageClearTarget::Production {
                snapshot,
                directories,
            } => {
                if snapshot.app_id != self.context.app_id
                    || directories
                        .iter()
                        .any(|directory| !directory.path.is_absolute())
                {
                    return Err("Storage clear production target mismatch".into());
                }
                validate_deletion_resources(&snapshot.operation_id, &snapshot.resources)
            }
            UserAppStorageClearTarget::Development {
                base_url,
                instance_id,
                endpoint,
                receipt,
            } => {
                if base_url.is_empty()
                    || instance_id.is_empty()
                    || endpoint.container_id.is_empty()
                    || endpoint.address.is_unspecified()
                    || *base_url != endpoint.base_url()
                    || receipt.runtime.app_id != self.context.app_id
                {
                    return Err("Storage clear development target mismatch".into());
                }
                validate_deletion_resources(
                    &receipt.runtime.operation_id,
                    &receipt.runtime.resources,
                )?;
                if receipt.runtime.docker_bind_cleanup {
                    let [container] = receipt.runtime.resources.as_slice() else {
                        return Err("Storage clear Docker target must name one container".into());
                    };
                    if container.kind != AppResourceKind::Container
                        || container.uid != endpoint.container_id
                    {
                        return Err("Storage clear endpoint differs from captured container".into());
                    }
                } else if !receipt
                    .runtime
                    .resources
                    .iter()
                    .any(|resource| resource.kind == AppResourceKind::StatefulSet)
                {
                    return Err("Storage clear Kubernetes workload receipt is missing".into());
                }
                if receipt.registry.as_ref().is_some_and(|identity| {
                    identity.generation.is_empty() || identity.container_id.is_empty()
                }) {
                    return Err("Storage clear registry identity is incomplete".into());
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserAppStorageClearTarget {
    Development {
        base_url: String,
        instance_id: String,
        endpoint: crate::UserAppBuilderWorkspaceEndpoint,
        receipt: Box<crate::UserappDevDeletionReceipt>,
    },
    Production {
        snapshot: AppDeletionSnapshot,
        directories: Vec<crate::storage_contents::CapturedStorageDirectory>,
    },
}

impl UserAppStorageDestruction {
    pub fn validate(&self) -> Result<(), String> {
        self.context.validate_identity(&self.context.app_id)?;
        if let Some(production) = &self.production {
            if production.app_id != self.context.app_id {
                return Err("Storage destruction production ownership mismatch".into());
            }
            validate_deletion_resources(&production.operation_id, &production.resources)?;
        }
        if self.development.runtime.app_id != self.context.app_id {
            return Err("Storage destruction development ownership mismatch".into());
        }
        validate_deletion_resources(
            &self.development.runtime.operation_id,
            &self.development.runtime.resources,
        )?;
        if self.development.registry.as_ref().is_some_and(|identity| {
            identity.generation.is_empty() || identity.container_id.is_empty()
        }) {
            return Err("Storage destruction registry identity is incomplete".into());
        }
        Ok(())
    }
}

/// PVC expansion receipt captured before runtime effects and durable checkpointed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppStorageResizeTarget {
    pub context: crate::UserAppExecutionContext,
    pub resource: AppResourceIdentity,
    pub current_size: String,
}

/// Durable mutation ownership inside an exclusively locked application lock file.
/// A nonempty marker is never reclaimed by time or process liveness heuristics.
pub struct AppFileMutationMarker {
    operation_id: String,
}

impl Default for AppFileMutationMarker {
    fn default() -> Self {
        Self::new()
    }
}
impl AppFileMutationMarker {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    /// Bind a marker to an admitted operation. This does not authorize reclaiming
    /// a nonempty file; callers still acquire the file lock and check cleanliness.
    pub fn for_operation(operation_id: &str) -> std::io::Result<Self> {
        crate::validate_identifier(operation_id, "operation_id").map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid application operation ID",
            )
        })?;
        Ok(Self {
            operation_id: operation_id.into(),
        })
    }
    pub fn new() -> Self {
        Self {
            operation_id: uuid::Uuid::new_v4().to_string(),
        }
    }
    pub fn check_clean(file: &std::fs::File) -> std::io::Result<()> {
        if file.metadata()?.len() == 0 {
            return Ok(());
        }
        Err(std::io::Error::other(
            "incomplete application mutation requires operator recovery",
        ))
    }
    pub fn begin(&self, mut file: &std::fs::File) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut owner = String::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_string(&mut owner)?;
        if owner == self.operation_id {
            return Ok(());
        }
        if !owner.is_empty() {
            return Err(std::io::Error::other(
                "application mutation ownership changed",
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        file.write_all(self.operation_id.as_bytes())?;
        file.sync_all()
    }
    pub fn complete(&self, mut file: &std::fs::File) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut owner = String::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_string(&mut owner)?;
        if !owner.is_empty() && owner != self.operation_id {
            return Err(std::io::Error::other(
                "application mutation ownership changed",
            ));
        }
        file.set_len(0)?;
        file.sync_all()
    }
}

/// Docker production identity label, shared by create, inspect and deletion.
pub const USERAPP_DOCKER_APP_ID_LABEL: &str = "app-id";

/// Preparation completed without changing the running application's container
/// or stored content. Image-cache and empty-directory preparation may have run;
/// there is no outstanding application mutation to retain the operation lease for.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct AppPreparationFailure {
    pub message: String,
}

#[cfg(test)]
mod compute_start_target_tests {
    use super::*;

    fn sample_target() -> UserAppComputeStartTarget {
        UserAppComputeStartTarget {
            target: UserAppMutationTarget {
                context: crate::UserAppExecutionContext {
                    app_id: "app".into(),
                    lifecycle_id: "life".into(),
                    operation_id: "op".into(),
                    executor_id: "worker".into(),
                    request_fingerprint: "f".repeat(64),
                },
                resource: AppResourceIdentity {
                    kind: AppResourceKind::Deployment,
                    name: "rcoder-app-app".into(),
                    uid: "uid".into(),
                    resource_version: Some("7".into()),
                },
            },
            compute_start_single_write: true,
            volumes: Vec::new(),
            restart_image: Some("registry.test/app-runtime:0.2.0".into()),
        }
    }

    /// restart_image 经 compute checkpoint 序列化往返保留（恢复/续跑确定性），
    /// 且 skip 序列化不破坏旧读端解析。
    #[test]
    fn restart_image_round_trips_and_legacy_payload_stays_compatible() {
        let target = sample_target();
        let encoded = serde_json::to_value(&target).expect("encode");
        assert_eq!(
            encoded["restart_image"], "registry.test/app-runtime:0.2.0",
            "frozen image must persist in the checkpoint payload"
        );
        let decoded: UserAppComputeStartTarget =
            serde_json::from_value(encoded.clone()).expect("decode");
        assert_eq!(decoded.restart_image, target.restart_image);
        let legacy = serde_json::json!({
            "context": encoded["context"],
            "resource": encoded["resource"],
            "compute_start_single_write": true,
            "volumes": [],
        });
        let legacy_decoded: UserAppComputeStartTarget =
            serde_json::from_value(legacy).expect("legacy payload parses");
        assert_eq!(legacy_decoded.restart_image, None);
        assert!(
            target.verify_same_volumes(&legacy_decoded).is_ok(),
            "volume witness comparison ignores the image field"
        );
    }
}

/// Kani 有界证明：删除身份 Fail-closed（P5 不完整资源必拒）。
/// 契约: `specs/002-kani-high-value-proofs/contracts/identity-fail-closed.md`。
#[cfg(kani)]
mod kani_proofs {
    use super::*;
    use crate::{UserAppExecutionContext, UserAppOperationKind};

    fn short_id(raw: [u8; 4]) -> String {
        let mut s = String::new();
        for b in raw {
            let c = match b % 38 {
                0..=25 => b'a' + (b % 26),
                26..=35 => b'0' + (b % 10),
                36 => b'_',
                _ => b'-',
            };
            s.push(c as char);
        }
        s
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn incomplete_resources_reject() {
        let app = short_id(kani::any());
        let life = short_id(kani::any());
        let op = short_id(kani::any());
        let exec = short_id(kani::any());
        let name = short_id(kani::any());
        let uid = short_id(kani::any());
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let context = UserAppExecutionContext {
            app_id: app.clone(),
            lifecycle_id: life,
            operation_id: op.clone(),
            executor_id: exec,
            request_fingerprint: fp.to_string(),
        };
        // 至少一个身份字段为空 → validate 必败（P5）
        let empty_name = kani::any();
        let empty_uid = kani::any();
        let resource = AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: if empty_name { String::new() } else { name },
            uid: if empty_uid { String::new() } else { uid },
            resource_version: None,
        };
        kani::assume(empty_name || empty_uid);
        let checkpoint = UserAppDeletionCheckpoint {
            stage: UserAppDeletionStage::Captured,
            schema_version: 1,
            context,
            kind: UserAppOperationKind::DeleteCompute,
            production: AppDeletionSnapshot {
                app_id: app,
                operation_id: op,
                resources: vec![resource],
            },
            development: None,
        };
        assert!(
            checkpoint.validate().is_err(),
            "incomplete deletion resource identity must fail closed"
        );
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn k8s_resource_requires_resource_version() {
        let app = short_id(kani::any());
        let op = short_id(kani::any());
        let name = short_id(kani::any());
        let uid = short_id(kani::any());
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let context = UserAppExecutionContext {
            app_id: app.clone(),
            lifecycle_id: short_id(kani::any()),
            operation_id: op.clone(),
            executor_id: short_id(kani::any()),
            request_fingerprint: fp.to_string(),
        };
        // 非 Container 的 K8s 资源缺 resource_version → 必拒
        let resource = AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name,
            uid,
            resource_version: None,
        };
        let checkpoint = UserAppDeletionCheckpoint {
            stage: UserAppDeletionStage::Captured,
            schema_version: 1,
            context,
            kind: UserAppOperationKind::DeleteCompute,
            production: AppDeletionSnapshot {
                app_id: app,
                operation_id: op,
                resources: vec![resource],
            },
            development: None,
        };
        assert!(
            checkpoint.validate().is_err(),
            "K8s deletion resource without resource_version must fail"
        );
    }
}
