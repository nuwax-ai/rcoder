//! kubernetes_config 卷/挂载/sidecar 规格翻译为 kube API 类型（纯翻译，无副作用）。
//!
//! 从 kubernetes_runtime.rs 拆出。三类翻译：
//! - `translate_k8s_volume`：`K8sVolumeSpec` → kube `Volume`（EmptyDir/Pvc/ConfigMap；HostPath 策略禁用；`workspace` 名保留）
//! - `translate_k8s_volume_mount`：`K8sVolumeMountSpec` → kube `VolumeMount`
//! - `translate_k8s_sidecar`：`K8sSidecarSpec` → kube `Container`（含 imagePullPolicy 默认 IfNotPresent、resources 经 build_resource_requirements）

use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container as K8sContainer, EmptyDirVolumeSource,
    PersistentVolumeClaimVolumeSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use tracing::warn;

use shared_types::{K8sSidecarSpec, K8sVolumeMountSpec, K8sVolumeSpec, K8sVolumeType};

use super::KubernetesRuntime;

impl KubernetesRuntime {
    /// 翻译 kubernetes_config 卷规格 → kube `Volume`。
    ///
    /// - EmptyDir：可选 size_limit
    /// - Pvc/ConfigMap：缺 claim_name/config_map_name → 跳过+告警
    /// - 返回 None 表示该卷被丢弃（调用方 flat_map 跳过）
    pub(crate) fn translate_k8s_volume(spec: &K8sVolumeSpec) -> Option<Volume> {
        // 卷名冲突保护：workspace 由 builder 硬编码管理
        if spec.name == "workspace" {
            warn!("[K8S] config volume name 'workspace' is reserved (builder-managed), skipping");
            return None;
        }
        match spec.volume_type {
            K8sVolumeType::EmptyDir => {
                let mut ed = EmptyDirVolumeSource::default();
                if let Some(sl) = &spec.size_limit {
                    ed.size_limit = Some(Quantity(sl.clone()));
                }
                Some(Volume {
                    name: spec.name.clone(),
                    empty_dir: Some(ed),
                    ..Default::default()
                })
            }
            K8sVolumeType::Pvc => {
                let Some(claim_name) = spec.claim_name.clone() else {
                    warn!(
                        "[K8S] pvc volume '{}' missing claim_name, skipping",
                        spec.name
                    );
                    return None;
                };
                Some(Volume {
                    name: spec.name.clone(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name,
                        read_only: Some(spec.read_only),
                    }),
                    ..Default::default()
                })
            }
            K8sVolumeType::ConfigMap => {
                let Some(cm_name) = spec.config_map_name.clone() else {
                    warn!(
                        "[K8S] configMap volume '{}' missing config_map_name, skipping",
                        spec.name
                    );
                    return None;
                };
                Some(Volume {
                    name: spec.name.clone(),
                    config_map: Some(ConfigMapVolumeSource {
                        name: cm_name,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
            K8sVolumeType::HostPath => {
                // 策略禁用：hostPath 绑宿主机路径，动态 agent pod 多节点漂移不安全。
                warn!(
                    "[K8S] hostPath volume '{}' is forbidden by policy, skipping",
                    spec.name
                );
                None
            }
        }
    }

    /// 翻译 kubernetes_config 卷挂载规格 → kube `VolumeMount`
    pub(crate) fn translate_k8s_volume_mount(spec: &K8sVolumeMountSpec) -> VolumeMount {
        VolumeMount {
            name: spec.name.clone(),
            mount_path: spec.mount_path.clone(),
            sub_path: spec.sub_path.clone(),
            read_only: Some(spec.read_only),
            ..Default::default()
        }
    }

    /// 翻译 kubernetes_config sidecar 规格 → kube `Container`（与主 agent 同 Pod）
    pub(crate) fn translate_k8s_sidecar(spec: &K8sSidecarSpec) -> K8sContainer {
        K8sContainer {
            name: spec.name.clone(),
            image: Some(spec.image.clone()),
            // 用户硬性要求：imagePullPolicy 必须 IfNotPresent（动态 pod 频繁创建，节点已缓存）
            image_pull_policy: Some(
                spec.image_pull_policy
                    .clone()
                    .unwrap_or_else(|| "IfNotPresent".to_string()),
            ),
            command: if spec.command.is_empty() {
                None
            } else {
                Some(spec.command.clone())
            },
            volume_mounts: Some(
                spec.volume_mounts
                    .iter()
                    .map(Self::translate_k8s_volume_mount)
                    .collect(),
            ),
            resources: Self::build_resource_requirements(&spec.resources),
            ..Default::default()
        }
    }
}

#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    use super::*;

    #[test]
    fn test_translate_volume_emptydir_default() {
        let spec = K8sVolumeSpec {
            name: "cache".into(),
            volume_type: K8sVolumeType::EmptyDir,
            ..Default::default()
        };
        let v = KubernetesRuntime::translate_k8s_volume(&spec).expect("emptyDir should translate");
        let ed = v.empty_dir.expect("empty_dir set");
        assert!(
            ed.size_limit.is_none(),
            "default emptyDir has no size_limit"
        );
    }

    #[test]
    fn test_translate_volume_emptydir_with_size_limit() {
        let spec = K8sVolumeSpec {
            name: "cache".into(),
            volume_type: K8sVolumeType::EmptyDir,
            size_limit: Some("512Mi".into()),
            ..Default::default()
        };
        let v = KubernetesRuntime::translate_k8s_volume(&spec).expect("emptyDir should translate");
        let ed = v.empty_dir.expect("empty_dir set");
        let sl = ed.size_limit.expect("size_limit set");
        assert_eq!(sl.0, "512Mi");
    }

    #[test]
    fn test_translate_volume_pvc_ok() {
        let spec = K8sVolumeSpec {
            name: "data".into(),
            volume_type: K8sVolumeType::Pvc,
            claim_name: Some("my-pvc".into()),
            read_only: true,
            ..Default::default()
        };
        let v = KubernetesRuntime::translate_k8s_volume(&spec).expect("pvc should translate");
        let pvc = v.persistent_volume_claim.expect("pvc set");
        assert_eq!(pvc.claim_name, "my-pvc");
        assert_eq!(pvc.read_only, Some(true));
    }

    #[test]
    fn test_translate_volume_pvc_missing_claim_name_skipped() {
        let spec = K8sVolumeSpec {
            name: "data".into(),
            volume_type: K8sVolumeType::Pvc,
            ..Default::default()
        };
        assert!(
            KubernetesRuntime::translate_k8s_volume(&spec).is_none(),
            "pvc without claim_name must be skipped"
        );
    }

    #[test]
    fn test_translate_volume_configmap_ok() {
        let spec = K8sVolumeSpec {
            name: "cfg".into(),
            volume_type: K8sVolumeType::ConfigMap,
            config_map_name: Some("my-cm".into()),
            ..Default::default()
        };
        let v = KubernetesRuntime::translate_k8s_volume(&spec).expect("configMap should translate");
        let cm = v.config_map.expect("config_map set");
        assert_eq!(cm.name, "my-cm");
    }

    #[test]
    fn test_translate_volume_configmap_missing_name_skipped() {
        let spec = K8sVolumeSpec {
            name: "cfg".into(),
            volume_type: K8sVolumeType::ConfigMap,
            ..Default::default()
        };
        assert!(
            KubernetesRuntime::translate_k8s_volume(&spec).is_none(),
            "configMap without config_map_name must be skipped"
        );
    }

    #[test]
    fn test_translate_volume_hostpath_forbidden() {
        // HostPath 被策略禁用 → 必须跳过（None），即使配了也要被拒绝
        let spec = K8sVolumeSpec {
            name: "forbidden".into(),
            volume_type: K8sVolumeType::HostPath,
            ..Default::default()
        };
        assert!(
            KubernetesRuntime::translate_k8s_volume(&spec).is_none(),
            "hostPath must be forbidden (policy)"
        );
    }

    #[test]
    fn test_translate_volume_workspace_name_reserved() {
        // "workspace" 卷名被 builder 硬编码占用 → 任何类型的同名卷都该被拒
        for vt in [
            K8sVolumeType::EmptyDir,
            K8sVolumeType::Pvc,
            K8sVolumeType::ConfigMap,
        ] {
            let spec = K8sVolumeSpec {
                name: "workspace".into(),
                volume_type: vt,
                claim_name: Some("x".into()),
                config_map_name: Some("y".into()),
                ..Default::default()
            };
            assert!(
                KubernetesRuntime::translate_k8s_volume(&spec).is_none(),
                "volume name 'workspace' is reserved, must be rejected for {:?}",
                vt
            );
        }
    }

    // ---- translate_k8s_volume_mount ----

    #[test]
    fn test_translate_volume_mount_basic() {
        let spec = K8sVolumeMountSpec {
            name: "container-logs".into(),
            mount_path: "/app/container-logs".into(),
            ..Default::default()
        };
        let m = KubernetesRuntime::translate_k8s_volume_mount(&spec);
        assert_eq!(m.name, "container-logs");
        assert_eq!(m.mount_path, "/app/container-logs");
        assert_eq!(m.sub_path, None);
        assert_eq!(m.read_only, Some(false), "default read_only=false");
    }

    #[test]
    fn test_translate_volume_mount_with_subpath_readonly() {
        let spec = K8sVolumeMountSpec {
            name: "data".into(),
            mount_path: "/data".into(),
            sub_path: Some("user-123".into()),
            read_only: true,
        };
        let m = KubernetesRuntime::translate_k8s_volume_mount(&spec);
        assert_eq!(m.sub_path.as_deref(), Some("user-123"));
        assert_eq!(m.read_only, Some(true));
    }

    // ---- translate_k8s_sidecar ----

    #[test]
    fn test_translate_sidecar_defaults_and_image_pull_policy() {
        let spec = K8sSidecarSpec {
            name: "log-collector".into(),
            image: "registry/alpine:3.22.4".into(),
            command: vec!["/bin/sh".into(), "-c".into(), "sleep 1".into()],
            volume_mounts: vec![K8sVolumeMountSpec {
                name: "container-logs".into(),
                mount_path: "/app/container-logs".into(),
                read_only: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let c = KubernetesRuntime::translate_k8s_sidecar(&spec);
        assert_eq!(c.name, "log-collector");
        assert_eq!(c.image.as_deref(), Some("registry/alpine:3.22.4"));
        // 用户硬性要求：image_pull_policy 缺省必须 IfNotPresent
        assert_eq!(
            c.image_pull_policy.as_deref(),
            Some("IfNotPresent"),
            "image_pull_policy must default to IfNotPresent"
        );
        // command 非空 → Some
        assert_eq!(
            c.command.as_deref(),
            Some(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "sleep 1".to_string()
                ][..]
            )
        );
        // volume_mounts 翻译
        let mounts = c.volume_mounts.expect("volume_mounts set");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "container-logs");
        assert_eq!(mounts[0].read_only, Some(true));
        // resources 全 None → build_resource_requirements 返回 None
        assert!(c.resources.is_none());
    }

    #[test]
    fn test_translate_sidecar_empty_command_becomes_none() {
        let spec = K8sSidecarSpec {
            name: "s".into(),
            image: "img".into(),
            ..Default::default()
        };
        let c = KubernetesRuntime::translate_k8s_sidecar(&spec);
        assert!(
            c.command.is_none(),
            "empty command should yield None (use image ENTRYPOINT/CMD)"
        );
    }

    #[test]
    fn test_translate_sidecar_explicit_image_pull_policy_honored() {
        let spec = K8sSidecarSpec {
            name: "s".into(),
            image: "img".into(),
            image_pull_policy: Some("Always".into()),
            ..Default::default()
        };
        let c = KubernetesRuntime::translate_k8s_sidecar(&spec);
        assert_eq!(c.image_pull_policy.as_deref(), Some("Always"));
    }
}
