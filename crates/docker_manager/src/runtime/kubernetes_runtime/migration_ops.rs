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

/// 读 Job 状态连续失败容忍多久。
///
/// kube 客户端内置的重试只覆盖 429/503/504；连接重置、502、超时会直接报错。
/// 同步一跑几分钟到几小时，期间 API server 抖一下，不该让几个小时的 phase-a
/// 白跑、更不该让 cutover 在 agent 已经停了的状态下失败。短暂的读失败按"这一轮
/// 状态未知"跳过，持续失败到这里的时间才判失败。Job 被人删掉（NotFound）
/// 没什么可等的，立刻失败。
const JOB_GET_ERROR_TOLERANCE: std::time::Duration = std::time::Duration::from_secs(120);

/// 读 Job 状态的连续失败计时（纯逻辑，便于单测）。
#[derive(Default)]
struct GetErrorBudget {
    failed_since: Option<std::time::Instant>,
}

impl GetErrorBudget {
    /// 记一次失败；返回 `true` = 已经连续失败太久，应当放弃。
    fn exhausted(&mut self, now: std::time::Instant, tolerance: std::time::Duration) -> bool {
        let failed_since = *self.failed_since.get_or_insert(now);
        now.duration_since(failed_since) >= tolerance
    }

    fn recovered(&mut self) {
        self.failed_since = None;
    }
}

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
    /// rclone 过滤规则：每条带 `+` / `-` 号，顺序即优先级（见
    /// [`shared_types::WORKSPACE_MIGRATION_FILTERS`]）。
    pub filters: Vec<String>,
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
        // `--create-empty-src-dirs`：rclone 默认不建空目录。用户工作区里的空目录是
        // 数据的一部分（PG 的 pg_notify / pg_stat / pg_wal/archive_status 等必需目录，
        // 用户建的空项目目录）——丢了 PG 起不来（实测 "could not open directory
        // pg_notify"）。`rclone check --size-only` 看不见这种缺失，只有目录树比对能发现。
        let mut args = format!(
            "sync {source} /dst --links --metadata --create-empty-src-dirs --transfers 8 --checkers 8"
        );
        if let Some(limit) = &self.bwlimit {
            args.push_str(&format!(
                " --bwlimit {}",
                shared_types::shell::single_quote(limit)
            ));
        }
        for filter in &self.filters {
            args.push_str(&format!(
                " --filter {}",
                shared_types::shell::single_quote(filter)
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
            filters: shared_types::WORKSPACE_MIGRATION_FILTERS
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
        let mut get_errors = GetErrorBudget::default();
        let outcome = loop {
            if std::time::Instant::now() > deadline {
                break Err("timeout".to_string());
            }
            match jobs.get(&plan.job_name).await {
                Ok(job) => {
                    get_errors.recovered();
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
                Err(kube::Error::Api(error)) if error.code == 404 => {
                    break Err(format!("job is gone: {}", error.message));
                }
                Err(e) => {
                    // 瞬时读失败: 这一轮状态未知, 跳过; 连续失败太久才放弃
                    if get_errors.exhausted(std::time::Instant::now(), JOB_GET_ERROR_TOLERANCE) {
                        break Err(format!(
                            "reading the job status kept failing for at least {JOB_GET_ERROR_TOLERANCE:?}: {e}"
                        ));
                    }
                    tracing::debug!(
                        job = %plan.job_name,
                        error = %e,
                        "migration sync job status read failed; will retry"
                    );
                }
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
mod tests;
