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
    Container, EnvVar, PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec,
    PodTemplateSpec, SeccompProfile, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, PostParams};

use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use shared_types::{ServiceType, WorkspaceSyncMode};

use super::*;

/// Job 等待上限（4h, 与迁移脚本 activeDeadlineSeconds 对齐）。
const JOB_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(14400);

/// 删迁移 Job 的参数：显式后台级联，让垃圾回收把 Job 的 Pod 一并删掉。
///
/// 不能用 `DeleteParams::default()`——它不带传播策略，Job 的资源级默认是"孤儿化依赖"：
/// Job 没了、Pod 永久留下（31.45 实测，每轮同步留一个已完成的 Pod，批量迁移就是
/// 每用户两个）。不用前台级联：那会让 Job 带着删除时间戳多活一会儿，紧接着重建
/// 同名 Job 会撞 409。
fn job_delete_params() -> DeleteParams {
    DeleteParams::background()
}

/// Job 的 Pod 起不来时（拉不到镜像、容器创建失败）的检查节奏：Job 创建后先等
/// 这么久再看第一次（给正常拉镜像留时间），之后每次间隔；退避状态从首次出现到
/// 判失败至少持续 [`POD_STUCK_FOR`]。镜像确实拉不到时约 30 秒判失败。
const POD_CHECK_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
const POD_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
const POD_STUCK_FOR: std::time::Duration = std::time::Duration::from_secs(20);

/// 不会自愈的容器等待态。`ErrImagePull` 不在其中：首次拉取失败可能只是网络
/// 抖动，重试即恢复；连续失败后 kubelet 才转成 `ImagePullBackOff`。
const BLOCKING_WAITING_REASONS: &[&str] = &[
    "ImagePullBackOff",
    "ErrImageNeverPull",
    "InvalidImageName",
    "CreateContainerError",
    "CreateContainerConfigError",
];

/// 拉取失败、kubelet 会按退避重试的等待态。单独出现不判失败（可能只是抖动），
/// 但也不清零已经累计的退避命中。
const RETRYING_WAITING_REASONS: &[&str] = &["ErrImagePull"];

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
    /// rclone 参数：数组字面量的元素列表（见 [`job_script`]；单测断言点）。
    ///
    /// 来自数据的每个参数值都单引号包裹（[`shared_types::shell::single_quote`]）。
    /// 脚本以 `ARGS=( … )` 接收，日志与 `exec` 都从这个数组取——参数边界只由
    /// 数组元素决定，与内容里的空白、元字符、引号无关；也不存在"同一个参数串
    /// 被第二处 shell 上下文（比如 `echo "…"` 的双引号）重新解释"的问题。
    pub fn rclone_args(&self) -> String {
        let source = shared_types::shell::single_quote(&format!("/src/{}", self.src_subpath));
        let mut args = format!("sync {source} /dst --links --metadata --transfers 8 --checkers 8");
        if let Some(limit) = &self.bwlimit {
            args.push_str(&format!(
                " --bwlimit {}",
                shared_types::shell::single_quote(limit)
            ));
        }
        for exclude in &self.excludes {
            args.push_str(&format!(
                " --exclude {}",
                shared_types::shell::single_quote(exclude)
            ));
        }
        args.push_str(" --stats-one-line --stats 300s");
        args
    }
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

/// Job 名：`migrate-{user_id 下划线→连字符}-{full|incremental}`。
/// 能通过 [`validate_migration_identifier`] 的 user_id 拼出来一定是合法的 DNS label。
fn migration_job_name(identifier: &str, mode: WorkspaceSyncMode) -> String {
    format!("migrate-{}-{}", identifier.replace('_', "-"), mode.as_str())
}

/// 同步开始前（任何 I/O 之前）对 user_id 的全部要求：能当共享卷下的路径段，
/// 也能拼出合法的 Job 名与标签值。编排入口已按同一规则拒过一次，这里是纵深防御。
fn validate_migration_identifier(identifier: &str) -> ContainerRuntimeResult<()> {
    validate_src_subpath(identifier)?;
    if let Err(reason) = shared_types::validate_migration_user_id(identifier) {
        return Err(ContainerRuntimeError::ConfigurationError(format!(
            "cannot run workspace migration for '{identifier}': {reason}"
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

/// 按 env 解析迁移 Job 镜像（见 [`resolve_migration_image`]）。
fn resolve_migration_image_from_env() -> ContainerRuntimeResult<(String, bool)> {
    resolve_migration_image(
        std::env::var("RCODER_MIGRATION_IMAGE").ok(),
        std::env::var("RCODER_MIGRATION_ALT_IMAGE").ok(),
    )
}

/// 容器内执行脚本（rclone 缺失时下载 fallback——与 k8s/migration job-template 同语义）。
///
/// 参数只在一处进入脚本：`ARGS=( … )` 数组字面量，每个数据值已单引号包裹。
/// 之后日志（printf）与 `exec rclone` 都展开 `"${ARGS[@]}"`——每个元素恰好
/// 是一个 argv，不存在第二次 shell 解析。`set -f` 保留为纵深防御（数组赋值
/// 与带引号的 `"${ARGS[@]}"` 展开本就不做 glob）。
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
ARGS=({args})
printf '[migrate] exec:'; printf ' %s' "${{ARGS[@]}}"; printf '\n'
exec rclone "${{ARGS[@]}}""#,
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
                    // Job 只跑 rclone, 不访问 API server——不挂 ServiceAccount token
                    // （与沙箱 Pod 同款加固）；系统调用面收敛到运行时默认 profile。
                    automount_service_account_token: Some(false),
                    security_context: Some(PodSecurityContext {
                        seccomp_profile: Some(SeccompProfile {
                            type_: "RuntimeDefault".to_string(),
                            localhost_profile: None,
                        }),
                        ..Default::default()
                    }),
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

/// Job 的 Pod 里有没有卡在"起不来"状态的容器；有则返回可读原因。
fn blocked_container<'a>(pods: impl IntoIterator<Item = &'a Pod>) -> Option<String> {
    waiting_reasons(pods).find_map(|(pod_name, container, reason, message)| {
        BLOCKING_WAITING_REASONS.contains(&reason).then(|| {
            let detail = message.map(|m| format!(": {m}")).unwrap_or_default();
            format!("{pod_name}/{container}: {reason}{detail}")
        })
    })
}

/// 所有容器当前的等待态：`(Pod 名, 容器名, 原因, 消息)`。
fn waiting_reasons<'a>(
    pods: impl IntoIterator<Item = &'a Pod>,
) -> impl Iterator<Item = (&'a str, &'a str, &'a str, Option<&'a str>)> {
    pods.into_iter().flat_map(|pod| {
        let pod_name = pod.metadata.name.as_deref().unwrap_or("<unnamed>");
        let statuses = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref());
        statuses.into_iter().flatten().filter_map(move |status| {
            let waiting = status
                .state
                .as_ref()
                .and_then(|state| state.waiting.as_ref())?;
            Some((
                pod_name,
                status.name.as_str(),
                waiting.reason.as_deref()?,
                waiting.message.as_deref(),
            ))
        })
    })
}

/// Job 的 Pod 当前卡在哪一类"起不来"的状态上。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PodBlockage {
    /// 没有异常：在拉镜像、在创建、在跑、已退出。
    #[default]
    None,
    /// 拉取失败后 kubelet 在重试。中性：不累计，也不清零。
    Retrying,
    /// 停在不会自愈的等待态上。
    Blocked,
}

fn pod_blockage<'a>(pods: impl IntoIterator<Item = &'a Pod>) -> PodBlockage {
    let mut blockage = PodBlockage::None;
    for (_, _, reason, _) in waiting_reasons(pods) {
        if BLOCKING_WAITING_REASONS.contains(&reason) {
            return PodBlockage::Blocked;
        }
        if RETRYING_WAITING_REASONS.contains(&reason) {
            blockage = PodBlockage::Retrying;
        }
    }
    blockage
}

/// 起不来要确认才判失败：单次观测可能是瞬时状态。
///
/// 拉不到镜像时 kubelet 的状态在**退避**与**重试**之间来回切——`ImagePullBackOff`
/// →（重试的一瞬）`ErrImagePull` → `ImagePullBackOff`，退避窗口越拉越长（10s、
/// 20s、40s…）。此前要求"连续两次观测到同一个原因"，中间夹一次重试就清零重来，
/// 31.45 实测同一套逻辑要 100 秒才判出来（cutover 的 agent 全程停着）。现在按
/// 累计判：退避状态计一次命中，重试的一瞬不计也不清零，没有异常才清零；首次命中
/// 之后满 `min_stuck` 且至少两次命中即确认。
#[derive(Default)]
struct StuckObservation {
    hits: u32,
    first_hit: Option<std::time::Instant>,
}

impl StuckObservation {
    fn confirmed(
        &mut self,
        blockage: PodBlockage,
        now: std::time::Instant,
        min_stuck: std::time::Duration,
    ) -> bool {
        match blockage {
            PodBlockage::Blocked => {
                self.hits += 1;
                let first_hit = *self.first_hit.get_or_insert(now);
                self.hits >= 2 && now.duration_since(first_hit) >= min_stuck
            }
            PodBlockage::Retrying => false,
            PodBlockage::None => {
                self.hits = 0;
                self.first_hit = None;
                false
            }
        }
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

    /// 迁移 Job 的前置配置校验：镜像必须已配置（纯 env 读取，无 I/O、无副作用）。
    ///
    /// 供编排入口在产生任何副作用（建 PVC、停 agent）**之前**调用。配置问题
    /// 拖到 `run_workspace_migration_sync_inner` 里才报，留下的是 phase-a 的
    /// 残留 PVC 或 cutover 的停机残留。本函数保留为纵深防御，不替代入口检查。
    pub(crate) fn workspace_migration_config_check(&self) -> ContainerRuntimeResult<()> {
        resolve_migration_image_from_env().map(|_| ())
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
        // 纯检查排在任何 I/O 之前（纵深防御：编排入口已先做过一遍）。
        // - user_id 既要能当 rclone 源路径段，也要能拼出合法的 Job 名与标签值，
        //   非法值必须在建 Job 之前就拒掉（否则 Job 建起来也只是同步一个越界
        //   路径，或者直接被 apiserver 打回）；
        // - 镜像是纯 env 读取，配置缺失不该等到读过 PVC 之后才报。
        validate_migration_identifier(identifier)?;
        // 镜像选择（显式配置即信任——部署方设 RCODER_MIGRATION_IMAGE 意味着
        // 专用 rclone-migrate 镜像已构建推送; 无法在本地可靠探测远端镜像存在性,
        // 之前"恒 false 走 fallback"会让显式配置被静默忽略）。两者都缺即报错,
        // 不再回退字面默认值（见 `resolve_migration_image`）。
        let (final_image, download_rclone) = resolve_migration_image_from_env()?;
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
        let plan = MigrationJobPlan {
            job_name: migration_job_name(identifier, mode),
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
        drop(jobs.delete(&plan.job_name, &job_delete_params()).await);
        jobs.create(&PostParams::default(), &build_job(&plan))
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create migration Job: {e}")))?;
        // 等完成: 轮询 condition=Complete / succeeded>=1（不依赖 wait 子命令,
        // 超时即失败——Job 的 activeDeadlineSeconds 会兜底杀掉 rclone）
        let deadline = std::time::Instant::now() + JOB_WAIT_TIMEOUT;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let selector = format!("job-name={}", plan.job_name);
        let mut stuck = StuckObservation::default();
        let mut next_pod_check = std::time::Instant::now() + POD_CHECK_DELAY;
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
                    // Failed 条件（backoffLimit 耗尽；不依赖具体的重试次数），或
                    // 失败计数耗尽且没有在跑的 Pod
                    let failed = status.and_then(|s| s.failed).unwrap_or(0);
                    let active = status.and_then(|s| s.active).unwrap_or(0);
                    let failed_condition =
                        status
                            .and_then(|s| s.conditions.as_ref())
                            .is_some_and(|conditions| {
                                conditions
                                    .iter()
                                    .any(|c| c.type_ == "Failed" && c.status == "True")
                            });
                    if failed_condition || (failed >= 3 && active == 0) {
                        break Err(format!("failed={failed}, active={active}"));
                    }
                }
                Err(e) => break Err(format!("get job: {e}")),
            }
            // Pod 起不来时（拉不到镜像、容器创建失败）Job 的 active 恒为 1、failed
            // 恒为 0——上面的判据要等到 activeDeadlineSeconds（4h）才成立，而这期间
            // cutover 的 agent 停着，写门禁与该用户的编排锁都被占着。这类等待态
            // 不会自愈，确认后立即失败：调用方拿到原因，用户的下一个请求会把
            // agent 重建回共享卷。
            if std::time::Instant::now() >= next_pod_check {
                next_pod_check = std::time::Instant::now() + POD_CHECK_EVERY;
                let now = std::time::Instant::now();
                match pods.list(&ListParams::default().labels(&selector)).await {
                    Ok(list) => {
                        let blockage = pod_blockage(list.items.iter());
                        if stuck.confirmed(blockage, now, POD_STUCK_FOR) {
                            break Err(blocked_container(list.items.iter())
                                .unwrap_or_else(|| "pod is not starting".to_string()));
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            job = %plan.job_name,
                            error = %e,
                            "migration sync job pod status check failed"
                        );
                        // 这次没观测到：按"没有异常"处理，重新计数
                        stuck.confirmed(PodBlockage::None, now, POD_STUCK_FOR);
                    }
                }
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
            if let Err(e) = jobs.delete(&plan.job_name, &job_delete_params()).await {
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
        if let Err(e) = jobs.delete(&plan.job_name, &job_delete_params()).await {
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

    /// 端到端：生成的脚本在**真实 bash** 下执行，rclone 收到的 argv 必须与
    /// 逐参数的预期完全一致——数据里的空白 / 元字符 / 引号 / `$()` / glob
    /// 既不得改变参数边界，也不得在 `exec` 之前被日志行提前解释。
    /// 没有 bash 的环境跳过。
    #[cfg(unix)]
    #[test]
    fn job_script_delivers_each_argument_as_one_argv_element() {
        let Ok(probe) = std::process::Command::new("bash")
            .arg("--version")
            .stderr(std::process::Stdio::null())
            .output()
        else {
            eprintln!("skipping: bash not found");
            return;
        };
        assert!(probe.status.success(), "bash --version 失败");

        let mut p = plan(WorkspaceSyncMode::Full);
        p.src_subpath = "u 1;rm -rf /".to_string();
        p.bwlimit = Some("1M".to_string());
        p.excludes = vec![
            "a b".to_string(),
            "it's".to_string(),
            "$(id)".to_string(),
            "core.[0-9]*".to_string(),
            "`id`".to_string(),
        ];

        // 假 rclone：把收到的 argv 逐行打到 stdout（`version` 子命令照常应答）
        let dir = std::env::temp_dir().join(format!("rcoder-migrate-argv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("rclone");
        std::fs::write(
            &stub,
            "#!/bin/sh\n\
             if [ \"$1\" = version ]; then echo rclone v0; exit 0; fi\n\
             for a in \"$@\"; do printf '<%s>\\n' \"$a\"; done\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(job_script(&p))
            // PATH 只含我们的目录 + 系统默认：不继承测试进程可能被改过的 PATH
            .env(
                "PATH",
                format!("{}:/usr/local/bin:/usr/bin:/bin", dir.display()),
            )
            .stderr(std::process::Stdio::inherit())
            .output()
            .unwrap();
        drop(std::fs::remove_dir_all(&dir));
        assert!(
            output.status.success(),
            "脚本应以 rclone 的退出码结束；stderr 见上"
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let argv: Vec<String> = stdout
            .lines()
            .filter(|line| line.starts_with('<'))
            .map(|line| line.trim_matches(['<', '>']).to_string())
            .collect();
        assert_eq!(
            argv,
            vec![
                "sync",
                "/src/u 1;rm -rf /",
                "/dst",
                "--links",
                "--metadata",
                "--transfers",
                "8",
                "--checkers",
                "8",
                "--bwlimit",
                "1M",
                "--exclude",
                "a b",
                "--exclude",
                "it's",
                "--exclude",
                "$(id)",
                "--exclude",
                "core.[0-9]*",
                "--exclude",
                "`id`",
                "--stats-one-line",
                "--stats",
                "300s",
            ],
            "rclone 实际收到的 argv 与预期不一致；脚本输出：\n{stdout}"
        );
        // 日志行照常打印了这条命令（不是静默执行）
        assert!(stdout.contains("[migrate] exec:"), "{stdout}");
    }

    /// Job 只跑 rclone，不访问 API server：不挂 ServiceAccount token（namespace 里
    /// 的 SA token 能读到什么，挂进数据面 Job 就是多大的面），系统调用面收敛到
    /// 运行时默认 seccomp profile。与沙箱 Pod 同款加固。
    /// 反例（31.45 实测）：不带传播策略删 Job，Pod 被孤儿化、永久留在命名空间里。
    #[test]
    fn deleting_the_job_cascades_to_its_pods() {
        assert_eq!(
            job_delete_params().propagation_policy,
            Some(kube::api::PropagationPolicy::Background)
        );
    }

    #[test]
    fn job_pod_mounts_no_service_account_token_and_sets_seccomp_profile() {
        let job = build_job(&plan(WorkspaceSyncMode::Full));
        let pod = job
            .spec
            .as_ref()
            .expect("spec")
            .template
            .spec
            .as_ref()
            .expect("pod spec");
        assert_eq!(pod.automount_service_account_token, Some(false));
        let profile = pod
            .security_context
            .as_ref()
            .and_then(|context| context.seccomp_profile.as_ref())
            .expect("pod seccomp profile");
        assert_eq!(profile.type_, "RuntimeDefault");
        assert_eq!(profile.localhost_profile, None);
        assert_eq!(pod.service_account_name, None);
    }

    #[test]
    fn job_name_is_a_valid_dns_label_for_every_accepted_user_id() {
        for id in ["6", "mig-u1", &"u".repeat(43)] {
            for mode in [WorkspaceSyncMode::Full, WorkspaceSyncMode::Incremental] {
                let name = migration_job_name(id, mode);
                assert!(
                    name.len() <= 63
                        && name.starts_with("migrate-")
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                    "{name}"
                );
            }
        }
    }

    /// 反例（部署评审）：这些 id 能通过标识符白名单，却拼不出合法的 Job 名或
    /// 标签值——拖到 Job 创建时才被 apiserver 拒绝（422）就晚了。
    #[test]
    fn migration_identifier_rejects_ids_that_cannot_name_the_job() {
        for id in ["6", "mig-u1", "user_42", &"u".repeat(43)] {
            assert!(validate_migration_identifier(id).is_ok(), "{id}");
        }
        for id in ["ABC123", "-mig", "mig-", "mig_", &"u".repeat(44)] {
            let error = validate_migration_identifier(id).unwrap_err();
            assert!(
                matches!(error, ContainerRuntimeError::ConfigurationError(_)),
                "{id}: {error:?}"
            );
            assert!(
                error
                    .to_string()
                    .contains("cannot name a Kubernetes migration Job")
            );
        }
        // 路径段规则仍在：越出共享卷的值以它为由拒绝
        let error = validate_migration_identifier("../etc").unwrap_err();
        assert!(error.to_string().contains("not a valid userId"), "{error}");
    }

    fn pod_with_container_state(name: &str, state: serde_json::Value) -> Pod {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name},
            "status": {"containerStatuses": [{"name": "sync", "state": state}]}
        }))
        .unwrap()
    }

    /// Job 起不来时 active 恒为 1、failed 恒为 0，只看 Job 状态要等满 4 小时——
    /// 期间 cutover 的 agent 停着、写门禁与编排锁都被占着。
    #[test]
    fn blocked_container_reports_only_waits_that_do_not_recover() {
        let pulling = pod_with_container_state(
            "migrate-u1-full-aaaaa",
            serde_json::json!({"waiting": {"reason": "ContainerCreating"}}),
        );
        // 首次拉取失败可能只是抖动, 重试即恢复: 不算
        let first_failure = pod_with_container_state(
            "migrate-u1-full-bbbbb",
            serde_json::json!({"waiting": {"reason": "ErrImagePull", "message": "timeout"}}),
        );
        let running = pod_with_container_state(
            "migrate-u1-full-ccccc",
            serde_json::json!({"running": {"startedAt": "2026-11-23T00:00:00Z"}}),
        );
        let empty = pod_with_container_state(
            "migrate-u1-full-ddddd",
            serde_json::json!({"terminated": {"exitCode": 1}}),
        );
        assert_eq!(
            blocked_container([&pulling, &first_failure, &running, &empty]),
            None
        );

        let backoff = pod_with_container_state(
            "migrate-u1-full-eeeee",
            serde_json::json!({"waiting": {
                "reason": "ImagePullBackOff",
                "message": "Back-off pulling image \"reg/rclone-migrate:1.76.0\""
            }}),
        );
        assert_eq!(
            blocked_container([&pulling, &backoff]),
            Some(
                "migrate-u1-full-eeeee/sync: ImagePullBackOff: Back-off pulling image \
                 \"reg/rclone-migrate:1.76.0\""
                    .to_string()
            )
        );
        let broken = pod_with_container_state(
            "migrate-u1-full-fffff",
            serde_json::json!({"waiting": {"reason": "CreateContainerError"}}),
        );
        assert_eq!(
            blocked_container([&broken]),
            Some("migrate-u1-full-fffff/sync: CreateContainerError".to_string())
        );
    }

    /// 单次观测可能是瞬时状态：退避状态要累计到"满 20 秒且至少两次"才确认。
    #[test]
    fn stuck_observation_needs_time_and_two_hits_in_backoff() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let stuck_for = Duration::from_secs(20);
        let mut stuck = StuckObservation::default();

        // 一直在退避: 第一次只记录, 满 20 秒才确认
        assert!(!stuck.confirmed(PodBlockage::Blocked, t0, stuck_for));
        assert!(!stuck.confirmed(
            PodBlockage::Blocked,
            t0 + Duration::from_secs(19),
            stuck_for
        ));
        assert!(stuck.confirmed(
            PodBlockage::Blocked,
            t0 + Duration::from_secs(20),
            stuck_for
        ));

        // 只命中一次、之后一直只是"在重试": 不确认（至少两次命中）
        let mut stuck = StuckObservation::default();
        assert!(!stuck.confirmed(PodBlockage::Blocked, t0, stuck_for));
        assert!(
            !stuck.confirmed(
                PodBlockage::Retrying,
                t0 + Duration::from_secs(600),
                stuck_for
            ),
            "重试的一瞬既不计也不清零"
        );
        assert!(stuck.confirmed(
            PodBlockage::Blocked,
            t0 + Duration::from_secs(605),
            stuck_for
        ));

        // 没有异常: 清零重来
        let mut stuck = StuckObservation::default();
        assert!(!stuck.confirmed(PodBlockage::Blocked, t0, stuck_for));
        assert!(!stuck.confirmed(PodBlockage::None, t0 + Duration::from_secs(5), stuck_for));
        assert!(!stuck.confirmed(
            PodBlockage::Blocked,
            t0 + Duration::from_secs(60),
            stuck_for
        ));
    }

    /// 反例（31.45 实测）：kubelet 在退避与重试之间来回切——`ImagePullBackOff`
    /// → `ErrImagePull` → `ImagePullBackOff`。旧判据要求连续两次同一个原因，
    /// 中间夹一次重试就清零，100 秒才判出来。
    #[test]
    fn a_retry_between_two_backoffs_does_not_reset_the_stuck_detection() {
        use std::time::{Duration, Instant};
        let stuck_for = Duration::from_secs(20);
        let t0 = Instant::now();
        let backoff = pod_with_container_state(
            "migrate-u1-full-aaaaa",
            serde_json::json!({"waiting": {"reason": "ImagePullBackOff", "message": "Back-off pulling image"}}),
        );
        let retrying = pod_with_container_state(
            "migrate-u1-full-aaaaa",
            serde_json::json!({"waiting": {"reason": "ErrImagePull", "message": "manifest unknown"}}),
        );
        let pulling = pod_with_container_state(
            "migrate-u1-full-aaaaa",
            serde_json::json!({"waiting": {"reason": "ContainerCreating"}}),
        );
        assert_eq!(pod_blockage([&pulling]), PodBlockage::None);
        assert_eq!(pod_blockage([&retrying]), PodBlockage::Retrying);
        assert_eq!(pod_blockage([&backoff]), PodBlockage::Blocked);

        // 每 5 秒一次的真实节奏: 10s 退避, 15s 正在重试, 20s 退避, ... 30s 确认
        let mut stuck = StuckObservation::default();
        let timeline = [
            (10, &backoff),
            (15, &retrying),
            (20, &backoff),
            (25, &backoff),
            (30, &backoff),
        ];
        let mut confirmed_at = None;
        for (secs, pod) in timeline {
            if stuck.confirmed(
                pod_blockage(std::iter::once(pod)),
                t0 + Duration::from_secs(secs),
                stuck_for,
            ) && confirmed_at.is_none()
            {
                confirmed_at = Some(secs);
            }
        }
        assert_eq!(
            confirmed_at,
            Some(30),
            "中间夹一次重试不该把判定推到 100 秒以后"
        );
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
