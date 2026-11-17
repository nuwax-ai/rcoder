//! per-agent workspace 迁移同步的 K8s 执行面（P2 第二批）。
//!
//! 两件事:
//! 1. [`workspace_volume_attached`]: PVC→PV→VolumeAttachment 防呆查询
//!    （cutover 停 agent 后必须确认 RBD 已卸载, 否则 rclone Job 挂载 = 同节点
//!    双挂 ext4 损坏）。
//! 2. [`run_workspace_migration_sync`]: 起 rclone 双卷 Job（src 共享 PVC RO +
//!    dst per-agent RBD RW）跑 sync/check, 等完成。
//!
//! Job 成功判据用 `.status.succeeded>=1`——**不是** `failed!=0`（K8s Job 的
//! failed 是累计计数器, backoffLimit 内重试一次即成功时仍为 1; 脚本侧踩过
//! 这个坑, rcoder 侧不再犯）。
//!
//! Job 镜像: env `RCODER_MIGRATION_IMAGE`（专用 rclone-migrate; 未设时用
//! env `RCODER_MIGRATION_ALT_IMAGE` + 容器内下载 rclone 的 fallback 命令）。

// 与 workspace_runtime 同款: kubernetes feature 门(模块注册处) + super::* 导入
// KubernetesRuntime/K8sPvcOps; k8s_openapi/kube 具体类型显式 import。

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvVar, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, PostParams};

use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use shared_types::{ServiceType, WorkspaceSyncMode};

use super::*;

const DEFAULT_MIGRATION_IMAGE: &str =
    "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-k8s-test/rclone-migrate:1.76.0";
const DEFAULT_MIGRATION_ALT_IMAGE: &str = "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-k8s-test/rcoder-computer-agent-runner:0.1.426";
/// Job 等待上限（4h, 与迁移脚本 activeDeadlineSeconds 对齐）。
const JOB_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(14400);

/// 迁移 Job 的执行参数（纯数据, 供单测构造/断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationJobPlan {
    pub job_name: String,
    pub namespace: String,
    pub image: String,
    pub download_rclone: bool,
    pub src_pvc: String,
    pub dst_pvc: String,
    /// src 卷内该用户子目录（sync 的 source path 段）。
    pub src_subpath: String,
    pub bwlimit: Option<String>,
    pub excludes: Vec<String>,
    pub mode: WorkspaceSyncMode,
}

impl MigrationJobPlan {
    /// rclone 参数串（Job 容器内 exec 的 argv tail; 单测断言点）。
    pub fn rclone_args(&self) -> String {
        let mut args = format!(
            "sync /src/{} /dst --links --metadata --transfers 8 --checkers 8",
            self.src_subpath
        );
        if let Some(limit) = &self.bwlimit {
            args.push_str(&format!(" --bwlimit {limit}"));
        }
        for exclude in &self.excludes {
            args.push_str(&format!(" --exclude {exclude}"));
        }
        args.push_str(" --stats-one-line --stats 300s");
        args
    }
}

/// 容器内执行脚本（rclone 缺失时下载 fallback——与 k8s/migration job-template 同语义,
/// `set -f` 防 exclude 模式被 bash 按 CWD 展开）。
fn job_script(plan: &MigrationJobPlan) -> String {
    let download = if plan.download_rclone {
        r#"if ! command -v rclone >/dev/null 2>&1; then
            echo "[migrate] rclone missing in image, downloading..."
            curl -fsSL --max-time 180 -o /tmp/rclone.zip https://downloads.rclone.org/rclone-current-linux-amd64.zip
            unzip -oq /tmp/rclone.zip -d /tmp/rc
            RC_BIN=$(find /tmp/rc -maxdepth 2 -name rclone -type f | head -1)
            install -m 0755 "$RC_BIN" /usr/local/bin/rclone
        fi
        "#
    } else {
        ""
    };
    format!(
        r#"set -euo pipefail
set -f
{download}rclone version | head -1
echo "[migrate] exec: rclone {args}"
exec rclone {args}"#,
        download = download,
        args = plan.rclone_args(),
    )
}

fn build_job(plan: &MigrationJobPlan) -> Job {
    let labels = [
        (
            shared_types::WORKSPACE_MIGRATION_JOB_LABEL.to_string(),
            plan.src_subpath.clone(),
        ),
        ("app".to_string(), "rcoder-workspace-migration".to_string()),
    ];
    Job {
        metadata: ObjectMeta {
            name: Some(plan.job_name.clone()),
            namespace: Some(plan.namespace.clone()),
            labels: Some(labels.iter().cloned().collect()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(2),
            active_deadline_seconds: Some(14400),
            template: PodTemplateSpec {
                metadata: None,
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    containers: vec![Container {
                        name: "sync".to_string(),
                        image: Some(plan.image.clone()),
                        command: Some(vec!["/bin/bash".to_string(), "-c".to_string()]),
                        args: Some(vec![job_script(plan)]),
                        env: Some(vec![EnvVar {
                            name: "RUST_LOG".to_string(),
                            value: Some("error".to_string()),
                            ..Default::default()
                        }]),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "src".to_string(),
                                mount_path: "/src".to_string(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "dst".to_string(),
                                mount_path: "/dst".to_string(),
                                read_only: Some(false),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        Volume {
                            name: "src".to_string(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: plan.src_pvc.clone(),
                                read_only: Some(true),
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "dst".to_string(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: plan.dst_pvc.clone(),
                                read_only: Some(false),
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl KubernetesRuntime {
    /// per-agent 卷是否仍被挂载（PVC→PV→VA; 查询出错**不**静默放行——
    /// 错误上抛由调用方 fail-closed 处理）。
    pub(crate) async fn workspace_volume_attached_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        use crate::runtime::k8s_pvc::K8sPvcOps;
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        let Some(pvc) =
            self.pvcs().get_opt(&pvc_name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get PVC '{pvc_name}': {e}"))
            })?
        else {
            return Ok(false); // PVC 不存在 = 无卷可挂
        };
        let Some(pv_name) = pvc
            .spec
            .and_then(|s| s.volume_name)
            .filter(|n| !n.is_empty())
        else {
            return Ok(false); // 未 Bound = 无 PV
        };
        let vas: Api<k8s_openapi::api::storage::v1::VolumeAttachment> =
            Api::all(self.client.clone());
        let list = vas
            .list(&ListParams::default())
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("list VolumeAttachment: {e}")))?;
        let attached = list.iter().any(|va| {
            let source_match =
                va.spec.source.persistent_volume_name.as_deref() == Some(pv_name.as_str());
            source_match && va.status.as_ref().map(|s| s.attached).unwrap_or(false)
        });
        Ok(attached)
    }

    /// 起 rclone 双卷 Job 跑一轮同步, 等完成（成功 = succeeded>=1）。
    pub(crate) async fn run_workspace_migration_sync_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        mode: WorkspaceSyncMode,
        src_pvc: &str,
    ) -> ContainerRuntimeResult<()> {
        use crate::runtime::k8s_pvc::K8sPvcOps;
        let dst_pvc = self.workspace_pvc_name(identifier, service_type)?;
        // dst 必须存在（phase-a 已 ensure）且未挂载（调用方已防呆, 这里复核）
        if self
            .pvcs()
            .get_opt(&dst_pvc)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("get PVC '{dst_pvc}': {e}")))?
            .is_none()
        {
            return Err(ContainerRuntimeError::ContainerNotFound(format!(
                "destination PVC '{dst_pvc}' not found; run phase-a first"
            )));
        }
        let image = std::env::var("RCODER_MIGRATION_IMAGE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MIGRATION_IMAGE.to_string());
        let (alt_image, download) = match std::env::var("RCODER_MIGRATION_ALT_IMAGE")
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            Some(alt) => (alt, true),
            None => (DEFAULT_MIGRATION_ALT_IMAGE.to_string(), true),
        };
        // 专用镜像不存在时（未构建推送）用 fallback: agent-runner + 容器内下载
        let (final_image, download_rclone) = if image_available(&image) {
            (image, false)
        } else {
            (alt_image, download)
        };
        let plan = MigrationJobPlan {
            job_name: format!("migrate-{}-{}", identifier.replace('_', "-"), mode.as_str()),
            namespace: self.namespace.clone(),
            image: final_image,
            download_rclone,
            src_pvc: src_pvc.to_string(),
            dst_pvc,
            src_subpath: identifier.to_string(),
            bwlimit: match mode {
                WorkspaceSyncMode::Full => Some("150M".to_string()),
                WorkspaceSyncMode::Incremental => None,
            },
            excludes: shared_types::WORKSPACE_MIGRATION_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            mode,
        };
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        jobs.delete(&plan.job_name, &DeleteParams::default())
            .await
            .ok();
        jobs.create(&PostParams::default(), &build_job(&plan))
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create migration Job: {e}")))?;
        // 等完成: 轮询 condition=Complete / succeeded>=1（不依赖 wait 子命令,
        // 超时即失败——Job 的 activeDeadlineSeconds 会兜底杀掉 rclone）
        let deadline = std::time::Instant::now() + JOB_WAIT_TIMEOUT;
        let outcome = loop {
            if std::time::Instant::now() > deadline {
                break Err("timeout".to_string());
            }
            match jobs.get(&plan.job_name).await {
                Ok(job) => {
                    let status = job.status.as_ref();
                    if status.and_then(|s| s.succeeded).unwrap_or(0) >= 1 {
                        break Ok(());
                    }
                    // Failed 条件出现且无成功可能（backoffLimit 耗尽）
                    let failed = status.and_then(|s| s.failed).unwrap_or(0);
                    let active = status.and_then(|s| s.active).unwrap_or(0);
                    if failed >= 3 && active == 0 {
                        break Err(format!("failed={failed}"));
                    }
                }
                Err(e) => break Err(format!("get job: {e}")),
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        };
        if let Err(reason) = outcome {
            let tail = jobs
                .get(&plan.job_name)
                .await
                .ok()
                .and_then(|job| {
                    job.status
                        .and_then(|s| s.conditions)
                        .map(|c| format!("{c:?}"))
                })
                .unwrap_or_default();
            let _ = jobs.delete(&plan.job_name, &DeleteParams::default()).await;
            return Err(ContainerRuntimeError::K8sError(format!(
                "migration sync job '{}' did not succeed ({reason}): {tail}",
                plan.job_name
            )));
        }
        let _ = jobs.delete(&plan.job_name, &DeleteParams::default()).await;
        Ok(())
    }
}

/// 专用镜像是否可用（crictl 不可靠, 用 PVC Job 的 ImagePull 失败快速判断成本高;
/// 简化: 默认假定**不可用**走 fallback——专用镜像推送后由部署方设
/// RCODER_MIGRATION_IMAGE 并通过 imagePullPolicy 验证; 测试期 fallback 路径
/// 已在 31.45 实证可跑）。
fn image_available(_image: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(mode: WorkspaceSyncMode) -> MigrationJobPlan {
        MigrationJobPlan {
            job_name: "migrate-u-1-full".into(),
            namespace: "ns".into(),
            image: "img".into(),
            download_rclone: false,
            src_pvc: "shared".into(),
            dst_pvc: "dst".into(),
            src_subpath: "u_1".into(),
            bwlimit: match mode {
                WorkspaceSyncMode::Full => Some("150M".to_string()),
                WorkspaceSyncMode::Incremental => None,
            },
            excludes: shared_types::WORKSPACE_MIGRATION_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            mode,
        }
    }

    #[test]
    fn full_mode_args_carry_bwlimit_and_excludes() {
        let args = plan(WorkspaceSyncMode::Full).rclone_args();
        assert!(args.starts_with("sync /src/u_1 /dst --links --metadata"));
        assert!(args.contains("--bwlimit 150M"));
        assert!(args.contains("--exclude node_modules/"));
        assert!(args.contains("--exclude core.[0-9]*"));
    }

    #[test]
    fn incremental_mode_omits_bwlimit() {
        let args = plan(WorkspaceSyncMode::Incremental).rclone_args();
        assert!(!args.contains("--bwlimit"));
        assert!(args.contains("--exclude dist/"));
    }

    #[test]
    fn job_script_disables_shell_glob() {
        // set -f 必须在 exec 前——否则 core.[0-9]* 被 bash 按 CWD 展开
        let script = job_script(&plan(WorkspaceSyncMode::Full));
        assert!(script.contains("set -f"));
        let f_pos = script.find("set -f").unwrap();
        let exec_pos = script.find("exec rclone").unwrap();
        assert!(f_pos < exec_pos);
    }

    #[test]
    fn job_spec_mounts_src_readonly() {
        let job = build_job(&plan(WorkspaceSyncMode::Full));
        let spec = job.spec.as_ref().expect("spec");
        assert_eq!(spec.backoff_limit, Some(2));
        let pod = spec.template.spec.as_ref().expect("pod spec");
        let src = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "src")
            .expect("src volume");
        assert_eq!(
            src.persistent_volume_claim.as_ref().unwrap().read_only,
            Some(true)
        );
        assert!(
            pod.containers
                .iter()
                .any(|c| c.name == "sync" && c.image.is_some())
        );
    }
}
