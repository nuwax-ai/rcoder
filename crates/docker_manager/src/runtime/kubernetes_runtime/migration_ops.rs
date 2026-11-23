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
//! Job 镜像: env `RCODER_MIGRATION_IMAGE`（专用 rclone-migrate, rclone 已在
//! 镜像内, 零下载）。未设即报错——不再回退字面默认值（原缺省指向**测试仓库**
//! 镜像, 生产集群拉不到 → Job 卡死, 编排最长等 4 小时, 期间被迁移用户的
//! agent 一直停着）。确需"镜像内无 rclone + 容器内在线下载"的测试期形态,
//! 显式设 `RCODER_MIGRATION_ALT_IMAGE`（自行承担 32MB 外网下载耗时）。

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
    ///
    /// 每个来自数据的参数都单引号包裹：脚本是 `bash -c` 收字符串，
    /// `set -f` 只防 glob 展开、不防分词——`src_subpath` 里进一个空白或
    /// shell 元字符就会把一条参数裂成两条（或变成重定向）。引用后
    /// 参数的边界只由引号决定，与内容无关。
    pub fn rclone_args(&self) -> String {
        let source = shell_quote(&format!("/src/{}", self.src_subpath));
        let mut args = format!("sync {source} /dst --links --metadata --transfers 8 --checkers 8");
        if let Some(limit) = &self.bwlimit {
            args.push_str(&format!(" --bwlimit {}", shell_quote(limit)));
        }
        for exclude in &self.excludes {
            args.push_str(&format!(" --exclude {}", shell_quote(exclude)));
        }
        args.push_str(" --stats-one-line --stats 300s");
        args
    }
}

/// 单引号 shell 字面量；内嵌单引号用 `'\''` 闭合-转义-重开。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// src 子路径（= user_id）白名单校验，规则取全仓单一事实源
/// [`shared_types::validate_identifier`]（file-server 解析 computer 路径时
/// 用的就是它，合法用户不会被这里新拦一道）。
///
/// 为什么必须拦：这个值被拼进 `/src/<subpath>` 当 rclone 的源路径段，
/// 放进 `/`、`..` 或空白就是越出共享卷读取（rclone 会老老实实跟着走）。
/// PVC 名那边对 identifier 做的是 **sanitize**（非法字符替成 `-`）——
/// 洗干净名字的同时原值仍会进路径，sanitize 不等于校验。
fn validate_src_subpath(identifier: &str) -> ContainerRuntimeResult<()> {
    if shared_types::validate_identifier(identifier, "userId").is_err() {
        return Err(ContainerRuntimeError::ConfigurationError(format!(
            "migration source identifier '{identifier}' is not a valid userId: \
             expected 1-64 characters of letters, digits, '_' or '-' \
             (it is joined under the shared volume as a path segment)"
        )));
    }
    Ok(())
}

/// 解析迁移 Job 镜像（纯函数, 供单测）→ `(镜像, 是否需容器内下载 rclone)`。
///
/// - `migration_image` 显式设置 → 直接用（rclone 已在镜像内, 零下载）；
/// - 否则 `alt_image`（调用方自行承担容器内在线下载 rclone 的耗时）；
/// - 两者都缺 → **报错**。没有字面默认值：那会指向测试仓库镜像, 生产集群
///   拉不到, 表现为 Job 卡死 + 编排干等 4 小时 + 用户 agent 全程停机,
///   比一句"没配镜像"难查得多。
fn resolve_migration_image(
    migration_image: Option<String>,
    alt_image: Option<String>,
) -> ContainerRuntimeResult<(String, bool)> {
    let configured = |value: Option<String>| {
        value
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(image) = configured(migration_image) {
        return Ok((image, false));
    }
    if let Some(image) = configured(alt_image) {
        return Ok((image, true));
    }
    Err(ContainerRuntimeError::ConfigurationError(
        "migration image is not configured: set RCODER_MIGRATION_IMAGE \
         (chart key rcoder.perAgentPvcRbd.migrationImage) to a rclone-migrate image; \
         set RCODER_MIGRATION_ALT_IMAGE instead to allow downloading rclone inside the job"
            .to_string(),
    ))
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
        // 先校验再动手：src 子路径要拼进 rclone 源路径，非法值必须在建 Job
        // 之前就拒掉（否则 Job 建起来也只是同步一个越界路径或直接失败）。
        validate_src_subpath(identifier)?;
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
        // 镜像选择（显式配置即信任——部署方设 RCODER_MIGRATION_IMAGE 意味着
        // 专用 rclone-migrate 镜像已构建推送; 无法在本地可靠探测远端镜像存在性,
        // 之前"恒 false 走 fallback"会让显式配置被静默忽略）。两者都缺即报错,
        // 不再回退字面默认值（见 `resolve_migration_image`）。
        let (final_image, download_rclone) = resolve_migration_image(
            std::env::var("RCODER_MIGRATION_IMAGE").ok(),
            std::env::var("RCODER_MIGRATION_ALT_IMAGE").ok(),
        )?;
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
            // 清理 best-effort：删除失败不改变主结果，但必须可观测
            // （Job 静默残留会误导后续迁移与排障）。
            if let Err(e) = jobs.delete(&plan.job_name, &DeleteParams::default()).await {
                tracing::warn!(
                    job = %plan.job_name,
                    error = %e,
                    "migration sync job cleanup delete failed after job failure"
                );
            }
            return Err(ContainerRuntimeError::K8sError(format!(
                "migration sync job '{}' did not succeed ({reason}): {tail}",
                plan.job_name
            )));
        }
        if let Err(e) = jobs.delete(&plan.job_name, &DeleteParams::default()).await {
            tracing::warn!(
                job = %plan.job_name,
                error = %e,
                "migration sync job cleanup delete failed after success"
            );
        }
        Ok(())
    }
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
        assert!(args.starts_with("sync '/src/u_1' /dst --links --metadata"));
        assert!(args.contains("--bwlimit '150M'"));
        assert!(args.contains("--exclude 'node_modules/'"));
        assert!(args.contains("--exclude 'core.[0-9]*'"));
    }

    #[test]
    fn incremental_mode_omits_bwlimit() {
        let args = plan(WorkspaceSyncMode::Incremental).rclone_args();
        assert!(!args.contains("--bwlimit"));
        assert!(args.contains("--exclude 'dist/'"));
    }

    #[test]
    fn every_data_derived_argument_is_shell_quoted() {
        // 引用后参数边界只由引号决定：subpath 里的空白/元字符不会被
        // bash 分词或当成重定向。set -f 只防 glob，不防这个。
        let mut p = plan(WorkspaceSyncMode::Full);
        p.src_subpath = "u 1;rm -rf /".to_string();
        p.bwlimit = Some("150M".to_string());
        p.excludes = vec!["a b".to_string(), "it's".to_string()];
        let args = p.rclone_args();
        assert!(args.starts_with("sync '/src/u 1;rm -rf /' /dst"), "{args}");
        assert!(args.contains("--exclude 'a b'"), "{args}");
        // 内嵌单引号必须闭合-转义-重开，不能直接把引号断掉
        assert!(args.contains("--exclude 'it'\\''s'"), "{args}");
    }

    #[test]
    fn src_subpath_whitelist_accepts_real_user_ids() {
        for id in ["u_1", "user-42", "ABC123", "a", "migrate-rbd-test-user-1"] {
            assert!(validate_src_subpath(id).is_ok(), "{id} 应通过");
        }
    }

    #[test]
    fn src_subpath_whitelist_rejects_path_and_shell_escape() {
        for id in [
            "", " ", "../etc", "a/b", "a b", "u;id", "u$(id)", "u`id`", "u|cat", "u*", "..", ".",
            "u\n", "用户",
        ] {
            let error = validate_src_subpath(id).unwrap_err().to_string();
            assert!(
                error.contains("not a valid userId"),
                "'{id}' 必须以 userId 白名单为由拒绝: {error}"
            );
        }
        // 超长同样拒（规则与 file-server 同一份：1-64）
        let long = "u".repeat(65);
        assert!(validate_src_subpath(&long).is_err());
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
    fn explicit_migration_image_skips_download() {
        let (image, download) =
            resolve_migration_image(Some("reg/rclone-migrate:1.76.0".into()), None).unwrap();
        assert_eq!(image, "reg/rclone-migrate:1.76.0");
        assert!(!download, "专用镜像内已有 rclone, 不该走下载");
    }

    #[test]
    fn alt_image_is_explicit_opt_in_for_download() {
        let (image, download) =
            resolve_migration_image(None, Some("reg/agent-runner:0.1.479".into())).unwrap();
        assert_eq!(image, "reg/agent-runner:0.1.479");
        assert!(download, "alt 镜像内没有 rclone, 必须容器内下载");
    }

    #[test]
    fn blank_env_values_are_not_configuration() {
        // 空串/纯空格 = 没配（K8s env 留空即是这种值），不能当成有效镜像
        assert!(resolve_migration_image(Some("   ".into()), None).is_err());
        assert!(resolve_migration_image(None, Some(String::new())).is_err());
    }

    #[test]
    fn missing_image_fails_fast_instead_of_defaulting_to_test_registry() {
        let err = resolve_migration_image(None, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("RCODER_MIGRATION_IMAGE"),
            "错误必须点名该设的 env, 便于运维直接照做: {msg}"
        );
        assert!(
            !msg.contains("nuwax-k8s-test"),
            "不得再把测试仓库镜像写进错误文案: {msg}"
        );
    }

    #[test]
    fn explicit_image_wins_over_alt() {
        let (image, download) = resolve_migration_image(
            Some("reg/rclone-migrate:1.76.0".into()),
            Some("reg/agent-runner:0.1.479".into()),
        )
        .unwrap();
        assert_eq!(image, "reg/rclone-migrate:1.76.0");
        assert!(!download);
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
