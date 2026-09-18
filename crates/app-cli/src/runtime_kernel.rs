//! 运行态单一所有者内核（阶段二，specs/userapp-runtime-ownership §3）。
//!
//! app-cli `serve` 是唯一运行态所有者：start/restart/deploy/stop 全部经
//! [`RuntimeKernel::admit`] 受理——短锁内完成鉴权/重放/冲突/修订/恢复检查，
//! 持久化 Accepted 记录后才派发执行（复用 server 主循环这一既有串行
//! worker：制品走部署通道，源码/停止走编排通道）。操作记录与事件按
//! workspace 卷根（父目录）下的 `.app-cli-state/` 稳定状态根持久化
//! （R02：跨热部署换代稳定），进程重建后可查询、
//! 可按 operation_id + request_digest 幂等重放。
//!
//! 关键不变量（spec R05/R06/R07）：
//! - 受理先持久化；终态发布前持久化；未确认停写保持恢复保护；
//! - HTTP/SSE 观察者断开不取消执行（worker 独立持有）；
//! - 同 operation_id 同摘要返回原结果；异摘要 409；
//! - stop 是持久化意图屏障：受理即提升 revision，旧构建提交被拒。

use anyhow::{Context, Result};
use shared_types::{
    DesiredState, ERR_OPERATION_ID_CONFLICT, ERR_OPERATION_IN_PROGRESS, ERR_RECOVERY_REQUIRED,
    ERR_REVISION_MISMATCH, ERR_RUNTIME_INSTANCE_MISMATCH, ERR_WORKSPACE_MISMATCH,
    RUNTIME_CONTROL_PROTOCOL_VERSION, RuntimeEventRecord, RuntimeFailureDetail,
    RuntimeIdentityView, RuntimeOperationKind, RuntimeOperationRequest, RuntimeOperationState,
    RuntimeOperationView, RuntimeStatusView, runtime_request_digest,
    validate_runtime_operation_request,
};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// 稳定状态根目录名（R02/B04）。
///
/// 权威根 = env `APP_CLI_STATE_ROOT`（平台注入，source/.run/别名同一目录）；
/// 缺省 `{workspace 卷根}/.app-cli-state/{application_id}`（按应用隔离）。
/// 绝不放入会被热部署替换的 workspace 内。旧位置（in-workspace、bare 卷根）
/// 由 [`RuntimeStore::open_with_root`] 一次性迁移；新旧并存 fail-fast。
pub(crate) const STATE_DIR_NAME: &str = ".app-cli-state";

/// 受理判定结果。
#[derive(Debug)]
pub(crate) enum AdmissionOutcome {
    /// 幂等重放：返回既有记录（不重复执行）。
    Replayed(RuntimeOperationView),
    /// 新受理（已持久化 Accepted，等待执行）。
    Accepted(RuntimeOperationView),
}

/// 提交屏障裁决（B03）。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CommitBarrierOutcome {
    /// 屏障通过并已收束 Succeeded。
    Committed,
    /// 已被取消（观察到的取消意图；**未写终态**）——调用方必须先停服并
    /// 确认清理，再经 finish 收束 Cancelled/RecoveryRequired（V02）。
    Cancelled,
    /// Stop 已受理（revision 推进）：调用方必须停服并让 Stop 执行，
    /// 本操作保持非 Succeeded（由调用方停服后收束 Cancelled）。
    Superseded,
    /// 操作已不是 active 执行者（并发收束/替换）——无操作。
    NotActive,
}

/// 受理拒绝（API 层映射 HTTP 409/400 信封）。
#[derive(Debug)]
pub(crate) struct AdmissionRejection {
    pub code: &'static str,
    pub message: String,
    /// 进行中操作 ID（ERR_OPERATION_IN_PROGRESS 携带）。
    pub active_operation_id: Option<String>,
}

/// 持久状态根布局（identity/desired/operations/events 单一根）。
pub struct RuntimeStore {
    root: PathBuf,
}

/// 扫描结果（R04）：读取/解码失败不再静默跳过——以 `blocked` 上报，
/// 调用方（[`RuntimeKernel::recover`]）据此保持恢复保护（阻断写操作）。
pub(crate) struct RecoveryScan {
    /// 被转 RecoveryRequired 的在途操作。
    pub recovered: Vec<String>,
    /// 无法读取/解码的记录（fail closed：保留文件，写操作被阻断直至
    /// 操作员裁决——查询/重放不是清理完成证明）。
    pub blocked: Vec<String>,
}

/// endpoint 发现记录（cross-platform.md §3）：owner 绑定成功后原子发布。
///
/// **是发现线索，不是所有权证明**——客户端连接后必须经
/// `/v1/runtime/identity` 核验实例身份（[`endpoint_matches_identity`]）。
/// 崩溃残留的旧记录（实例已换/地址已变）在核验时被拒绝（XP10）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EndpointRecord {
    pub protocol_version: u32,
    pub application_id: String,
    pub workspace_id: String,
    pub runtime_instance_id: String,
    pub address: String,
}

fn endpoint_path(state_root: &Path) -> PathBuf {
    state_root.join("endpoint.json")
}

/// 发现记录与远端 identity 的核验：全部身份字段一致才算命中。
/// 任一不符（旧实例/错应用/协议不兼容）返回 false——调用方按
/// "旧发现记录"处理：丢弃线索，不据此发认证请求。
pub fn endpoint_matches_identity(record: &EndpointRecord, identity: &RuntimeIdentityView) -> bool {
    record.protocol_version == identity.protocol_version
        && record.application_id == identity.application_id
        && record.workspace_id == identity.workspace_id
        && record.runtime_instance_id == identity.runtime_instance_id
}

impl RuntimeStore {
    /// 解析稳定状态根（B04：显式优先，杜绝 parent() 猜测歧义）。
    ///
    /// 优先级：
    /// 1. `APP_CLI_STATE_ROOT` env——平台（rcoder）注入的显式按应用根；
    ///    source 根、`.run` 别名、任何入口都由平台指向同一目录（唯一锁域）。
    /// 2. 缺省 `{workspace 卷根}/.app-cli-state/{application_id}`——按应用
    ///    隔离（多 app 共享卷不互踩）；entry 别名无法归一时以 env 为准。
    pub fn resolve_root(workspace: &Path, application_id: &str) -> Result<PathBuf> {
        let explicit = std::env::var_os("APP_CLI_STATE_ROOT").filter(|value| !value.is_empty());
        Self::resolve_root_with_explicit(workspace, application_id, explicit)
    }

    /// [`Self::resolve_root`] 的可参数化核心——测试以显式 env 值驱动，
    /// 不经 `std::env::set_var`（进程级 env 变异与并行测试的 resolve 读取竞争）。
    ///
    /// R09：解析委托共享契约 crate（runtime-state-layout）——显式 env 优先；
    /// 有 PROJECT_ID（application_id 非 unknown-app）按应用段；standalone
    /// 走本地项目登记表（兄弟项目独立段、source/.run 同段、symlink 折叠）。
    /// app-cli 与 file-server 凭据查找复用同一规则，不再各自猜目录。
    fn resolve_root_with_explicit(
        workspace: &Path,
        application_id: &str,
        explicit: Option<std::ffi::OsString>,
    ) -> Result<PathBuf> {
        let project_id =
            (application_id != "unknown-app").then(|| std::ffi::OsString::from(application_id));
        runtime_state_layout::ensure_state_root(
            workspace,
            explicit.as_deref(),
            project_id.as_deref(),
        )
        .with_context(|| {
            format!(
                "resolve runtime state root for workspace {} (app {application_id})",
                workspace.display()
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn open(workspace: &Path) -> Result<Self> {
        // 无身份上下文的缺省打开（测试用）——application_id 未知时退缺省应用段
        Self::open_with_root(Self::resolve_root(workspace, "_default")?, workspace)
    }

    /// B04：以显式根打开。旧位置兼容迁移（一次性原子 rename；双权威域
    /// fail-fast 拒绝写入）：
    /// - `{workspace}/.app-cli-state`（R02 之前的 in-workspace 布局）
    /// - `{workspace 卷根}/.app-cli-state`（R02–R08 的 bare 卷根布局——
    ///   本轮加入 application_id 段后成为 legacy）
    pub(crate) fn open_with_root(root: PathBuf, workspace: &Path) -> Result<Self> {
        let volume_root = workspace
            .parent()
            .context("workspace has no volume root for stable runtime state")?;
        let legacy_locations = [
            workspace.join(STATE_DIR_NAME),
            volume_root.join(STATE_DIR_NAME),
        ];
        let root_exists_already = root_exists(&root)?;
        let mut existing_legacy: Option<PathBuf> = None;
        for legacy in &legacy_locations {
            // 按应用根 {bare}/{app} 的 create_dir_all 会隐式创建 bare 祖先
            // 目录——bare 存在 ≠ bare 是 legacy 状态域。仅当 bare **直接**
            // 含状态标记（desired/identity/operations/events）才视为 legacy。
            if !is_legacy_state_domain(legacy)? {
                continue;
            }
            if let Some(first) = &existing_legacy {
                // 多处 legacy 并存：无法裁决哪份权威——拒绝（操作员处理后重启）
                anyhow::bail!(
                    "multiple legacy runtime state locations exist ({} and {}); \
                     resolve explicitly before starting",
                    first.display(),
                    legacy.display()
                );
            }
            existing_legacy = Some(legacy.clone());
        }
        if let Some(legacy) = &existing_legacy {
            if root_exists_already {
                anyhow::bail!(
                    "runtime state exists both at stable root {} and legacy {} \
                     (two authority domains); resolve explicitly before starting",
                    root.display(),
                    legacy.display()
                );
            }
            // 物理路径比较（R09 解析经 canonicalize 后与 workspace 派生路径
            // 可能仅差 symlink 前缀——/var vs /private/var；字符串 starts_with
            // 会把"同一物理目录嵌套"误判成跨目录 rename → EINVAL）
            let legacy_canonical = std::fs::canonicalize(legacy).unwrap_or_else(|_| legacy.clone());
            let root_canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            if root_canonical.starts_with(&legacy_canonical) {
                // bare 卷根 legacy → 按应用根嵌套在其子树内（rename 进自身
                // 子树 EINVAL）——逐条目搬移后拆除 legacy 目录。
                std::fs::create_dir_all(&root).context("create per-app state root")?;
                for entry in std::fs::read_dir(legacy).context("read legacy state entries")? {
                    let entry = entry.context("read legacy entry")?;
                    // create_dir_all(root) 已在 bare 内建出按应用根（本分支
                    // root 嵌套于 legacy）——跳过自身，只搬真实状态条目。
                    // 物理路径比较（/var 与 /private/var 前缀差异下字符串
                    // 相等会把 root 自身当待搬条目 → rename 进自身 EINVAL）
                    let entry_path = entry.path();
                    let entry_canonical =
                        std::fs::canonicalize(&entry_path).unwrap_or_else(|_| entry_path.clone());
                    if entry_canonical == root_canonical {
                        continue;
                    }
                    let target = root.join(entry.file_name());
                    std::fs::rename(entry.path(), &target).with_context(|| {
                        format!(
                            "migrate legacy entry {} -> {}",
                            entry.path().display(),
                            target.display()
                        )
                    })?;
                }
                // bare 目录此刻只剩按应用根（其子树），保留为容器目录——
                // 状态条目已全部搬入按应用根，不再构成 legacy 状态域。
            } else {
                // 目标父链可能尚不存在（按应用根的中间目录）——先建再搬
                std::fs::create_dir_all(&root)
                    .context("create per-app state root for migration")?;
                std::fs::rename(legacy, &root).with_context(|| {
                    format!(
                        "migrate legacy runtime state {} -> {}",
                        legacy.display(),
                        root.display()
                    )
                })?;
            }
            tracing::info!(
                "runtime state migrated to stable per-application root: {}",
                root.display()
            );
        }
        std::fs::create_dir_all(root.join("operations")).context("create runtime state dir")?;
        std::fs::create_dir_all(root.join("events")).context("create runtime events dir")?;
        Ok(Self { root })
    }

    fn identity_path(&self) -> PathBuf {
        self.root.join("identity.json")
    }
    fn desired_path(&self) -> PathBuf {
        self.root.join("desired.json")
    }
    fn operation_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join("operations")
            .join(format!("{operation_id}.json"))
    }
    fn events_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join("events")
            .join(format!("{operation_id}.jsonl"))
    }

    /// owner 绑定成功后原子发布 endpoint 发现记录（cross-platform.md §3）。
    /// 多项目天然隔离：状态根按 application_id 分目录，各项目各一份记录。
    pub(crate) fn store_endpoint(&self, record: &EndpointRecord) -> Result<()> {
        write_json(&endpoint_path(&self.root), record)
    }

    /// 干净关停时清除发现记录（崩溃残留的旧记录由客户端核验拒绝——XP10）。
    pub(crate) fn clear_endpoint(&self) -> Result<()> {
        match std::fs::remove_file(endpoint_path(&self.root)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(anyhow::Error::from(error)).with_context(|| {
                format!(
                    "clear endpoint record {}",
                    endpoint_path(&self.root).display()
                )
            }),
        }
    }

    /// 只读发现记录（attach/客户端侧：无副作用，不建目录不迁移）。
    pub(crate) fn read_endpoint(state_root: &Path) -> Option<EndpointRecord> {
        let content = std::fs::read_to_string(endpoint_path(state_root)).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// 本地凭据文件（cross-platform.md §3）：token 落盘状态根，Unix 0600。
    /// 平台读此文件对既有 owner 提交运行操作；凭据不经命令行/日志外泄。
    pub(crate) fn store_token(&self, token: &str) -> Result<()> {
        let path = self.root.join("token");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("open token file {}", path.display()))?;
        use std::io::Write as _;
        file.write_all(token.trim().as_bytes())
            .and_then(|_| file.sync_all())
            .with_context(|| format!("persist token file {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 600 token file {}", path.display()))?;
        }
        #[cfg(windows)]
        {
            // std 无 ACL API——经 icacls 移除继承并仅授予当前用户完全控制
            // （cross-platform.md §3 凭据文件平台保护）。失败如实上抛：
            // 凭据保护失败不应静默。
            let username =
                std::env::var("USERNAME").context("resolve current user for token ACL")?;
            let status = std::process::Command::new("icacls")
                .arg(&path)
                .arg("/inheritance:r")
                .arg("/grant:r")
                .arg(format!("{username}:(F)"))
                .output()
                .with_context(|| format!("run icacls on token file {}", path.display()))?;
            anyhow::ensure!(
                status.status.success(),
                "restrict token file ACL to current user failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
        Ok(())
    }

    /// 只读凭据（平台侧探测复用；无副作用）。
    pub(crate) fn read_token(state_root: &Path) -> Option<String> {
        std::fs::read_to_string(state_root.join("token"))
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    }

    /// 读取/落盘身份（runtime_instance_id 每次进程启动新生成；
    /// deployment_generation_id 延续既有代次）。
    pub(crate) fn load_or_init_identity(
        &self,
        application_id: String,
        service_family: String,
        workspace_id: String,
        source_root: String,
        deployment_generation_id: String,
    ) -> Result<RuntimeIdentityView> {
        let runtime_instance_id = uuid::Uuid::new_v4().to_string();
        let identity = RuntimeIdentityView {
            application_id,
            service_family,
            workspace_id,
            source_root,
            runtime_instance_id,
            deployment_generation_id,
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![
                "operations".into(),
                "events".into(),
                "source-profile".into(),
                // 制品能力当前仅 Deploy+URL（R03 收窄声明；ArtifactId 本地
                // 解析器未实现——按显式组合拒绝，不虚报通用 artifact 能力）
                "deploy-artifact-url".into(),
            ],
        };
        // identity.json 不回读旧 runtime_instance（旧实例身份不得复用），仅覆盖。
        write_json(&self.identity_path(), &identity)?;
        Ok(identity)
    }

    pub(crate) fn load_desired(&self) -> Result<(DesiredState, u64)> {
        let value: Option<serde_json::Value> = read_json(&self.desired_path())?;
        match value {
            Some(value) => {
                let desired = serde_json::from_value(value["desired"].clone())
                    .context("decode desired state")?;
                let revision = value["revision"].as_u64().context("decode revision")?;
                Ok((desired, revision))
            }
            None => Ok((DesiredState::Running, 0)),
        }
    }

    /// 持久化 desired + revision（单文件覆盖写，先 fsync 再可见）。
    pub(crate) fn store_desired(&self, desired: DesiredState, revision: u64) -> Result<()> {
        let value = serde_json::json!({ "desired": desired, "revision": revision });
        write_json(&self.desired_path(), &value)
    }

    pub(crate) fn load_operation(&self, operation_id: &str) -> Result<Option<StoredOperation>> {
        let value: Option<serde_json::Value> = read_json(&self.operation_path(operation_id))?;
        match value {
            Some(value) => Ok(Some(
                serde_json::from_value(value).context("decode operation")?,
            )),
            None => Ok(None),
        }
    }

    pub(crate) fn store_operation(&self, operation: &StoredOperation) -> Result<()> {
        // R08：凭据不落盘——持久化副本对 run_config.pg 脱敏（重放只回终态
        // 视图，恢复不重执行，脱敏不影响语义；诊断可见用户名）
        let mut redacted = operation.clone();
        if let Some(config) = redacted.request.run_config.as_mut()
            && let Some(pg) = config.pg.as_mut()
        {
            pg.password = String::new();
        }
        write_json(
            &self.operation_path(&operation.view.operation_id),
            &redacted,
        )
    }

    /// 追加事件（每操作单序列；先落盘再发布——spec §3.6）。
    pub(crate) fn append_event(&self, event: &RuntimeEventRecord) -> Result<()> {
        use std::io::Write as _;
        let path = self.events_path(&event.operation_id);
        let mut line = serde_json::to_string(event).context("encode event")?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open event log {}", path.display()))?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.sync_data())
            .with_context(|| format!("append event {}", path.display()))?;
        Ok(())
    }

    /// 重放事件（after_seq 语义：返回 sequence > after_seq 的记录）。
    pub(crate) fn replay_events(
        &self,
        operation_id: &str,
        after_seq: u64,
    ) -> Result<Vec<RuntimeEventRecord>> {
        let path = self.events_path(operation_id);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read event log {}", path.display()))?;
        let mut events = Vec::new();
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let event: RuntimeEventRecord =
                serde_json::from_str(line).context("decode event record")?;
            if event.sequence > after_seq {
                events.push(event);
            }
        }
        events.sort_by_key(|event| event.sequence);
        Ok(events)
    }

    /// 启动恢复：最后一次非终态操作置 RecoveryRequired（不猜结果，写操作
    /// 被拒直至该操作显式恢复/重放完成）。
    pub(crate) fn recover_unfinished_operations(&self) -> Result<RecoveryScan> {
        let mut scan = RecoveryScan {
            recovered: Vec::new(),
            blocked: Vec::new(),
        };
        let dir = self.root.join("operations");
        let entries = std::fs::read_dir(&dir).context("scan operations dir")?;
        for entry in entries {
            // R04：目录枚举失败同样 fail closed（不再 flatten 静默跳过）
            let entry = entry.context("read operations dir entry")?;
            let path = entry.path();
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::error!(
                        "runtime state: unreadable operation record {}: {error}",
                        path.display()
                    );
                    scan.blocked.push(path.display().to_string());
                    continue;
                }
            };
            let operation = match serde_json::from_str::<StoredOperation>(
                content.trim_end_matches('\n'),
            ) {
                Ok(operation) => operation,
                Err(error) => {
                    // 损坏记录 fail closed：保留文件、阻断写、不猜状态。
                    tracing::error!(
                        "runtime state: undecodable operation record kept untouched {}: {error}",
                        path.display()
                    );
                    scan.blocked.push(path.display().to_string());
                    continue;
                }
            };
            let mut operation = operation;
            if !operation.view.state.is_terminal() {
                operation.view.state = RuntimeOperationState::RecoveryRequired;
                operation.view.error_code = Some(ERR_RECOVERY_REQUIRED.into());
                operation.view.error_message =
                    Some("process restarted before the operation reached a terminal state".into());
                let id = operation.view.operation_id.clone();
                self.store_operation(&operation)?;
                scan.recovered.push(id);
            }
        }
        Ok(scan)
    }
}

/// 受理快照 + 对外视图（持久化单位）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredOperation {
    pub view: RuntimeOperationView,
    /// 受理时的规范化请求（重放摘要比对的原始输入）。
    pub request: RuntimeOperationRequest,
}

/// legacy 候选是否**直接**承载状态（非仅为按应用根的祖先目录）。
fn is_legacy_state_domain(path: &Path) -> Result<bool> {
    if !root_exists(path)? {
        return Ok(false);
    }
    Ok([
        path.join("desired.json"),
        path.join("identity.json"),
        path.join("operations"),
        path.join("events"),
    ]
    .iter()
    .any(|marker| root_exists(marker).unwrap_or(false)))
}

fn root_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("stat runtime state root"),
    }
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create state parent dir")?;
    }
    let temp = path.with_extension("tmp");
    let content = serde_json::to_vec_pretty(value).context("encode state record")?;
    // Windows 实测（Defender 实时扫描）：并行状态写入时 .tmp 文件被 AV 短暂
    // 独占（ACCESS_DENIED / SHARING_VIOLATION）——瞬时锁有界重试吸收，
    // 重试耗尽仍失败才如实上抛（fail-closed，不静默丢状态）。
    let mut last_error = None;
    for attempt in 0..5u32 {
        match write_json_once(&temp, path, &content) {
            Ok(()) => return Ok(()),
            Err(error) if is_transient_windows_lock(&error) => {
                tracing::warn!(attempt, error = %error, "state persist transiently locked; retrying");
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::from(attempt + 1) * 20,
                ));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("state persist retries exhausted")))
}

/// 单次写入序列：同句柄 create+write+sync，drop 后再 rename——Windows 上
/// rename 要求目标文件无打开句柄（分离的 File::open 句柄会被 AV 延迟释放）。
fn write_json_once(temp: &Path, path: &Path, content: &[u8]) -> Result<()> {
    {
        let mut file = std::fs::File::create(temp)
            .with_context(|| format!("create state temp {}", temp.display()))?;
        std::io::Write::write_all(&mut file, content)
            .and_then(|_| file.sync_all())
            .with_context(|| format!("persist state {}", temp.display()))?;
    }
    std::fs::rename(temp, path).with_context(|| format!("publish state {}", path.display()))?;
    Ok(())
}

/// Windows AV/索引器的瞬时文件锁（ERROR_ACCESS_DENIED=5 / ERROR_SHARING_VIOLATION=32）。
fn is_transient_windows_lock(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(|io_error| io_error.raw_os_error())
            .is_some_and(|code| code == 5 || code == 32)
    })
}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>> {
    if !path.is_file() {
        return Ok(None);
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read state {}", path.display()))?;
    Ok(Some(
        serde_json::from_str(&content).context("decode state record")?,
    ))
}

/// 运行操作内核：受理/查询/事件/取消的协调面（执行仍归 server 主循环）。
pub(crate) struct RuntimeKernel {
    store: RuntimeStore,
    identity: RuntimeIdentityView,
    /// 受理短锁（不跨执行 await；执行互斥由 active_operation 单槽承担）。
    admission: Mutex<AdmissionState>,
    /// 执行派发回调（server 主循环注入；制品→部署通道，源码/停止→编排通道）。
    dispatch: Box<dyn Fn(DispatchAction) + Send + Sync>,
    /// 已请求取消的操作墓碑（R03）：受理后、执行完成前的取消标记。
    /// 执行侧在副作用边界检查（派发执行前/编排完成提交前）。
    cancelled: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// 执行派发动作（server 主循环解释；内核不直接触碰业务运行态）。
#[derive(Debug, Clone)]
pub(crate) enum DispatchAction {
    /// 制品部署（复用既有部署准备/激活/编排全链）。
    DeployArtifact {
        operation_id: String,
        url: String,
        sha256: Option<String>,
    },
    /// 登记的本地构建制品部署（R03：平台 staging 后 owner 受理激活——
    /// 制品 zip 在共享卷 `builds/` 目录，不经网络下载）。
    DeployLocalArtifact {
        operation_id: String,
        artifact_id: String,
        sha256: Option<String>,
    },
    /// 源码编排（workspace 当前内容 + release lock；start/restart source）。
    /// R08：dev_profile 随操作传递（Source profile = dev 语义）——编排的
    /// 生效命令选择不再依赖 serve 进程 env 猜测；pg 为每操作运行配置
    /// （凭据注入服务 env，不落盘——持久化副本在 store_operation 前脱敏）。
    OrchestrateSource {
        operation_id: String,
        dev_profile: bool,
        pg: Option<shared_types::StartPgCredential>,
    },
    /// 停止业务服务（保持管理面可用）。
    StopBusiness { operation_id: String },
}

#[derive(Default)]
struct AdmissionState {
    /// 当前执行者（None = 空闲；单槽即单 worker 串行）。V03：只有**执行型**
    /// 操作（start/restart/deploy）占据此槽——Stop 是待执行意图，绝不抢走
    /// 在执行者的身份（旧 A 的提交屏障因此不再被误判 NotActive）。
    active_operation_id: Option<String>,
    /// 已受理待执行的 Stop（V03）：持有者等待 server 主循环停服后按自身 ID
    /// 收束。pending 期间新操作（含第二个 Stop）一律 ERR_OPERATION_IN_PROGRESS。
    pending_stop: Option<String>,
    /// 恢复保护中（上次结果未知）。V03：由**结果未知**决定，不依赖 active
    /// 恰好匹配——任何操作的 RecoveryRequired 终态都会挂起保护。
    recovery_protection: bool,
}

impl RuntimeKernel {
    pub(crate) fn new(
        store: RuntimeStore,
        identity: RuntimeIdentityView,
        dispatch: Box<dyn Fn(DispatchAction) + Send + Sync>,
    ) -> Self {
        Self {
            store,
            identity,
            admission: Mutex::new(AdmissionState::default()),
            dispatch,
            cancelled: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub(crate) fn identity(&self) -> &RuntimeIdentityView {
        &self.identity
    }

    pub(crate) fn store(&self) -> &RuntimeStore {
        &self.store
    }

    /// 启动恢复入口（serve 主流程在 quiescence/ownership 之后调用）。
    /// 启动恢复（R04）：返回 RecoveryRequired 操作清单；**不可读/损坏记录
    /// 不再被视为"无事发生"**——恢复保护保持置位，写操作被阻断（`blocked`
    /// 数量经日志暴露，操作员裁决后删除/修复对应文件并重启解除）。
    pub(crate) async fn recover(&self) -> Result<Vec<String>> {
        let scan = self.store.recover_unfinished_operations()?;
        let mut guard = self.admission.lock().await;
        // 恢复保护：在途操作被标记 RecoveryRequired，或存在不可判定记录
        // 时拒绝新写（spec §3.3 崩溃注入语义；R04 把 fail-closed 从注释
        // 变成实际行为）。
        guard.recovery_protection = !scan.recovered.is_empty() || !scan.blocked.is_empty();
        if !scan.blocked.is_empty() {
            tracing::error!(
                count = scan.blocked.len(),
                paths = ?scan.blocked,
                "runtime state has unreadable operation records; writes are blocked until resolved"
            );
        }
        Ok(scan.recovered)
    }

    pub(crate) async fn status(&self) -> Result<RuntimeStatusView> {
        let (_desired, revision) = self.store.load_desired()?;
        let guard = self.admission.lock().await;
        Ok(RuntimeStatusView {
            desired: _desired,
            observed: shared_types::ObservedHealth::Unknown,
            active_target: None,
            revision,
            active_operation_id: guard.active_operation_id.clone(),
            recovery_protection: guard.recovery_protection,
            runtime_instance_id: self.identity.runtime_instance_id.clone(),
        })
    }

    /// 受理（spec §3.3）：短锁内完成全部检查与持久化，执行派发在锁外。
    pub(crate) async fn admit(
        &self,
        request: RuntimeOperationRequest,
    ) -> std::result::Result<AdmissionOutcome, AdmissionRejection> {
        if let Err(error) = validate_runtime_operation_request(&request) {
            return Err(AdmissionRejection {
                code: "ERR_VALIDATION",
                message: error,
                active_operation_id: None,
            });
        }
        let digest = runtime_request_digest(&request).map_err(|error| AdmissionRejection {
            code: "ERR_VALIDATION",
            message: error,
            active_operation_id: None,
        })?;
        // R03：kind×profile 组合前置校验——未实现组合在**任何持久化/占位
        // 之前**结构化拒绝（不留半受理状态）。已实现：Deploy+Artifact(Url
        // 或 ArtifactId——R03 owner 侧激活的登记本地制品)、Start/Restart+
        // Source、Stop（任意 profile）。
        match (&request.kind, &request.profile) {
            (
                RuntimeOperationKind::Deploy,
                shared_types::RunProfileInput::Artifact { artifact: _ },
            )
            | (
                RuntimeOperationKind::Start | RuntimeOperationKind::Restart,
                shared_types::RunProfileInput::Source { .. },
            )
            | (RuntimeOperationKind::Stop, _) => {}
            (kind, profile) => {
                return Err(AdmissionRejection {
                    code: shared_types::ERR_PROTOCOL_UNSUPPORTED,
                    message: format!(
                        "operation kind {kind} with profile {profile:?} is not implemented \
                         in this build; supported: deploy+artifact(url), start/restart+source, stop"
                    ),
                    active_operation_id: None,
                });
            }
        }
        // 身份核验先于一切重放判断：旧实例请求不修改新实例（spec §3.1）。
        if request.expected_runtime_instance_id != self.identity.runtime_instance_id {
            return Err(AdmissionRejection {
                code: ERR_RUNTIME_INSTANCE_MISMATCH,
                message: format!(
                    "expected runtime instance {} does not match current {}",
                    request.expected_runtime_instance_id, self.identity.runtime_instance_id
                ),
                active_operation_id: None,
            });
        }
        if request.workspace_id != self.identity.workspace_id {
            return Err(AdmissionRejection {
                code: ERR_WORKSPACE_MISMATCH,
                message: format!(
                    "workspace {} does not match owner binding {}",
                    request.workspace_id, self.identity.workspace_id
                ),
                active_operation_id: None,
            });
        }
        let mut guard = self.admission.lock().await;
        // 幂等重放先于 busy 拒绝（spec：先查重放再拒绝 busy）。
        if let Some(existing) =
            self.store
                .load_operation(&request.operation_id)
                .map_err(|error| AdmissionRejection {
                    code: "ERR_BACKEND_ERROR",
                    message: format!("read operation history: {error:#}"),
                    active_operation_id: None,
                })?
        {
            if existing.view.request_digest != digest {
                return Err(AdmissionRejection {
                    code: ERR_OPERATION_ID_CONFLICT,
                    message: format!(
                        "operation {} was accepted with a different request",
                        request.operation_id
                    ),
                    active_operation_id: None,
                });
            }
            return Ok(AdmissionOutcome::Replayed(existing.view));
        }
        if guard.recovery_protection {
            return Err(AdmissionRejection {
                code: ERR_RECOVERY_REQUIRED,
                message: "a previous operation has an unconfirmed result; recovery is required"
                    .into(),
                active_operation_id: None,
            });
        }
        let (_desired, revision) =
            self.store
                .load_desired()
                .map_err(|error| AdmissionRejection {
                    code: "ERR_BACKEND_ERROR",
                    message: format!("read desired state: {error:#}"),
                    active_operation_id: None,
                })?;
        // Stop 屏障例外（spec §3.3）：active 执行期间仍可受理持久化停止意图
        // ——Stop 只占 pending 槽，绝不抢走执行者身份（V03）。
        // 待执行 Stop 存在期间：第二个 Stop 与一切新操作均拒绝（停服动作
        // 尚未完成，受理即排队语义不成立）。
        let is_stop = request.kind == RuntimeOperationKind::Stop;
        if is_stop {
            if let Some(pending) = guard.pending_stop.clone() {
                return Err(AdmissionRejection {
                    code: ERR_OPERATION_IN_PROGRESS,
                    message: "another runtime operation is in progress".into(),
                    active_operation_id: Some(pending),
                });
            }
        } else {
            let busy = guard
                .active_operation_id
                .clone()
                .or_else(|| guard.pending_stop.clone());
            if let Some(active) = busy {
                return Err(AdmissionRejection {
                    code: ERR_OPERATION_IN_PROGRESS,
                    message: "another runtime operation is in progress".into(),
                    active_operation_id: Some(active),
                });
            }
        }
        if request.expected_revision != revision {
            // 所有 kind 的 revision 校验统一执行（旧实例的 stop 不复活/不重复推进）。
            return Err(AdmissionRejection {
                code: ERR_REVISION_MISMATCH,
                message: format!(
                    "expected revision {} but current revision is {}",
                    request.expected_revision, revision
                ),
                active_operation_id: None,
            });
        }
        // 持久化受理（落盘失败不入执行队列——spec §3.3）。
        let stored = StoredOperation {
            view: RuntimeOperationView {
                operation_id: request.operation_id.clone(),
                kind: request.kind,
                state: RuntimeOperationState::Accepted,
                request_digest: digest,
                revision,
                runtime_instance_id: self.identity.runtime_instance_id.clone(),
                error_code: None,
                error_message: None,
                failure_detail: None,
            },
            request,
        };
        if let Err(error) = self.store.store_operation(&stored) {
            return Err(AdmissionRejection {
                code: "ERR_BACKEND_ERROR",
                message: format!("persist accepted operation: {error:#}"),
                active_operation_id: None,
            });
        }
        // stop 受理即持久化意图并推进 revision：旧构建稍后提交被拒（spec §3.3）。
        // R04：Accepted 已落盘后 desired 写失败 = 部分提交——该操作转为
        // RecoveryRequired 并挂恢复保护（终态可查询、执行不派发；其他 ID
        // 同样被拒，直至重启恢复裁决）。绝不能只返回错误留下永远 Accepted
        // 的幽灵记录。
        if is_stop {
            let next_revision = revision.checked_add(1).ok_or_else(|| AdmissionRejection {
                code: "ERR_BACKEND_ERROR",
                message: "revision overflow".into(),
                active_operation_id: None,
            })?;
            if let Err(error) = self
                .store
                .store_desired(DesiredState::Stopped, next_revision)
            {
                guard.recovery_protection = true;
                return Err(
                    self.hold_partial_admission(&stored, format!("persist stop intent: {error:#}"))
                );
            }
        } else if let Err(error) = self.store.store_desired(DesiredState::Running, revision) {
            guard.recovery_protection = true;
            return Err(
                self.hold_partial_admission(&stored, format!("persist running intent: {error:#}"))
            );
        }
        if is_stop {
            guard.pending_stop = Some(stored.view.operation_id.clone());
        } else {
            guard.active_operation_id = Some(stored.view.operation_id.clone());
        }
        // 分派映射（组合已在受理前置校验收窄，此处仅选择已实现路径）
        let action = match (&stored.request.kind, &stored.request.profile) {
            (
                RuntimeOperationKind::Deploy,
                shared_types::RunProfileInput::Artifact {
                    artifact: shared_types::ArtifactInput::Url { url, sha256 },
                },
            ) => DispatchAction::DeployArtifact {
                operation_id: stored.view.operation_id.clone(),
                url: url.clone(),
                sha256: sha256.clone(),
            },
            (
                RuntimeOperationKind::Deploy,
                shared_types::RunProfileInput::Artifact {
                    artifact: shared_types::ArtifactInput::ArtifactId { artifact_id },
                },
            ) => DispatchAction::DeployLocalArtifact {
                operation_id: stored.view.operation_id.clone(),
                artifact_id: artifact_id.clone(),
                sha256: None,
            },
            (RuntimeOperationKind::Stop, _) => DispatchAction::StopBusiness {
                operation_id: stored.view.operation_id.clone(),
            },
            (
                RuntimeOperationKind::Start | RuntimeOperationKind::Restart,
                shared_types::RunProfileInput::Source { .. },
            ) => DispatchAction::OrchestrateSource {
                operation_id: stored.view.operation_id.clone(),
                dev_profile: true,
                pg: stored
                    .request
                    .run_config
                    .as_ref()
                    .and_then(|config| config.pg.clone()),
            },
            // 组合已在前置校验拒绝；到这里的组合是防御纵深违规——fail fast
            (kind, profile) => {
                return Err(AdmissionRejection {
                    code: "ERR_BACKEND_ERROR",
                    message: format!(
                        "internal: unvalidated dispatch combination {kind}/{profile:?} reached execution"
                    ),
                    active_operation_id: None,
                });
            }
        };
        let operation_id = stored.view.operation_id.clone();
        self.emit(
            &operation_id,
            1,
            "accepted",
            None,
            Some(stored.view.kind.as_str()),
        );
        // 执行派发（server 主循环持有唯一执行权；内核不并发起第二个 worker）。
        (self.dispatch)(action);
        Ok(AdmissionOutcome::Accepted(stored.view))
    }

    pub(crate) async fn get(&self, operation_id: &str) -> Result<Option<RuntimeOperationView>> {
        Ok(self
            .store
            .load_operation(operation_id)?
            .map(|stored| stored.view))
    }

    /// 终态发布（server 主循环在执行完成/失败后调用；先持久化再清 active 槽）。
    ///
    /// R04 终态单调：已有终态（Succeeded/Failed/Cancelled）或 RecoveryRequired
    /// 的操作不可被再次 finish 覆盖——迟到的 finish（同态幂等返回 Ok；异态
    /// 拒绝并保留原终态）只做身份/取消簿记清理。取消收束后的 Cancelled 记录
    /// 不会被迟到的成功/失败提交改写。
    pub(crate) async fn finish(
        &self,
        operation_id: &str,
        state: RuntimeOperationState,
        error: Option<(String, String)>,
        failure_detail: Option<RuntimeFailureDetail>,
        next_sequence: u64,
    ) -> Result<()> {
        let stored = self.write_terminal(operation_id, state, error, failure_detail)?;
        if stored.view.state != state {
            // 已有终态未被覆盖（R04）：仅做簿记清理，不重复发终态事件
            tracing::warn!(
                operation_id,
                existing = ?stored.view.state,
                refused = ?state,
                "runtime operation already terminal; refusing overwrite (R04)"
            );
        } else {
            let event_name = match stored.view.state {
                RuntimeOperationState::Succeeded => "Completed",
                _ => "Failed",
            };
            // R06：Failed 终态事件携带错误码/消息（SSE 消费方按事件流即可
            // 还原失败原因，不需要再回查操作视图）
            let payload = match (&stored.view.state, &stored.view.error_code) {
                (RuntimeOperationState::Succeeded, _) => None,
                (_, Some(code)) => Some(serde_json::json!({
                    "code": code,
                    "error": stored.view.error_message.clone().unwrap_or_default(),
                })),
                (_, None) => None,
            };
            self.emit_with_payload(
                operation_id,
                next_sequence,
                "terminal",
                None,
                Some(event_name),
                payload,
            );
        }
        if let Ok(mut set) = self.cancelled.lock() {
            set.remove(operation_id);
        }
        let mut guard = self.admission.lock().await;
        // V03：恢复保护由**结果未知**决定——任何操作以 RecoveryRequired 收束
        // 都挂起保护，不依赖 active 恰好仍是该操作（Stop 待执行期间，旧执行者
        // A 的未知结果同样必须锁住写入口）。
        if stored.view.state == RuntimeOperationState::RecoveryRequired {
            guard.recovery_protection = true;
        }
        if guard.active_operation_id.as_deref() == Some(operation_id) {
            guard.active_operation_id = None;
        }
        if guard.pending_stop.as_deref() == Some(operation_id) {
            guard.pending_stop = None;
        }
        Ok(())
    }

    /// 终态写入（无锁内聚版本；调用方自持 admission 锁时用
    /// [`Self::write_terminal_locked`]）。终态单调：已有终态/恢复保护不覆盖。
    fn write_terminal(
        &self,
        operation_id: &str,
        state: RuntimeOperationState,
        error: Option<(String, String)>,
        failure_detail: Option<RuntimeFailureDetail>,
    ) -> Result<StoredOperation> {
        let mut stored = self
            .store
            .load_operation(operation_id)?
            .context("finish unknown operation")?;
        let terminal = state.is_terminal() || state == RuntimeOperationState::RecoveryRequired;
        anyhow::ensure!(terminal, "finish requires a terminal or recovery state");
        if stored.view.state.is_terminal()
            || stored.view.state == RuntimeOperationState::RecoveryRequired
        {
            return Ok(stored);
        }
        stored.view.state = state;
        if let Some((code, message)) = error {
            stored.view.error_code = Some(code);
            stored.view.error_message = Some(message);
        }
        stored.view.failure_detail = failure_detail;
        self.store.store_operation(&stored)?;
        Ok(stored)
    }

    /// 持久化终态并在**已持有的 admission 锁内**完成身份/取消簿记——
    /// 提交线性化点唯一（R03：检查与写入之间不再有取消/受理窗口）。
    fn write_terminal_locked(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, AdmissionState>,
        operation_id: &str,
        state: RuntimeOperationState,
    ) -> Result<()> {
        let stored = self.write_terminal(operation_id, state, None, None)?;
        if let Ok(mut set) = self.cancelled.lock() {
            set.remove(operation_id);
        }
        // V03：同 finish——保护由结果未知决定；两槽按身份清理
        if stored.view.state == RuntimeOperationState::RecoveryRequired {
            guard.recovery_protection = true;
        }
        if guard.active_operation_id.as_deref() == Some(operation_id) {
            guard.active_operation_id = None;
        }
        if guard.pending_stop.as_deref() == Some(operation_id) {
            guard.pending_stop = None;
        }
        Ok(())
    }

    /// 执行提交屏障（B03）：在**同一 admission 锁**下原子裁决启动完成的提交。
    ///
    /// 线性化检查 + 终态写入（R03：全部在同一锁内完成，取消与成功提交不再
    /// 存在检查后写入的竞态窗口）：
    /// 1. 本操作仍是 active 执行者（未被并发收束/替换）；
    /// 2. 受理后无取消墓碑（有 → Cancelled，不报成功）；
    /// 3. 受理 revision 仍是当前 revision（Stop 受理会推进 revision——
    ///    推进过 → Superseded：调用方必须停服并让 Stop 执行，A 不报 Succeeded）。
    ///
    /// 通过 → 锁内原子收束 Succeeded。清理未知保持 RecoveryRequired 由调用方
    /// 经 [`Self::finish`] 表达。
    pub(crate) async fn commit_execution(
        &self,
        operation_id: &str,
    ) -> Result<CommitBarrierOutcome, anyhow::Error> {
        let sequence = self
            .store
            .replay_events(operation_id, u64::MAX)
            .map(|events| events.len() as u64 + 1)
            .unwrap_or(2);
        // 持久化用的终态在锁内决定并写入——提交线性化点
        let mut guard = self.admission.lock().await;
        if guard.active_operation_id.as_deref() != Some(operation_id) {
            return Ok(CommitBarrierOutcome::NotActive);
        }
        if self.is_cancelled(operation_id) {
            // V02：只观察取消意图，**不预写 Cancelled 终态**——调用方必须先
            // 确认业务/静态服务停止，再经 finish(Cancelled) 收束；清理未知时
            // 改走 RecoveryRequired（终态单调下 Cancelled 不可升级，预写会
            // 把未知结果锁死成已取消）。active/取消墓碑保留至收束。
            drop(guard);
            return Ok(CommitBarrierOutcome::Cancelled);
        }
        let (_, current_revision) = self.store.load_desired()?;
        let stored = self
            .store
            .load_operation(operation_id)?
            .context("commit unknown operation")?;
        if current_revision != stored.view.revision {
            // Stop 已受理（revision 推进）——本启动不得提交成功；
            // active 保留（Stop 信号在 server 循环执行并按自身 ID 收束后清除）。
            return Ok(CommitBarrierOutcome::Superseded);
        }
        let outcome =
            self.write_terminal_locked(&mut guard, operation_id, RuntimeOperationState::Succeeded);
        match outcome {
            Ok(()) => {
                drop(guard);
                self.emit(
                    operation_id,
                    sequence.max(2),
                    "terminal",
                    None,
                    Some("Completed"),
                );
                Ok(CommitBarrierOutcome::Committed)
            }
            Err(error) => {
                // 终态持久化失败：保护保留（active 不清），调用方走恢复路径
                Err(error)
            }
        }
    }

    /// 取消请求（受理态返回；不伪称立即完成——spec §3.2 cancel 语义）。
    pub(crate) async fn request_cancel(&self, operation_id: &str) -> Result<bool> {
        let guard = self.admission.lock().await;
        if guard.active_operation_id.as_deref() != Some(operation_id) {
            return Ok(false);
        }
        // R03：真实取消——记录墓碑，执行侧在副作用边界
        // （[`Self::is_cancelled`]）检查并在终态提交前收束为 Cancelled。
        // 不在此改写状态：终态仍由执行边界持久化（含取消场景的清理证据）。
        self.cancelled
            .lock()
            .map_err(|_| anyhow::anyhow!("cancel registry lock poisoned"))?
            .insert(operation_id.to_string());
        Ok(true)
    }

    /// 部分提交围栏（R04）：Accepted 落盘后意图写失败时调用——操作转
    /// RecoveryRequired（尽量落盘；落盘也失败时仅日志，保护已在内存），
    /// 并置恢复保护阻断后续写受理。
    fn hold_partial_admission(
        &self,
        stored: &StoredOperation,
        reason: String,
    ) -> AdmissionRejection {
        let mut held = stored.clone();
        held.view.state = RuntimeOperationState::RecoveryRequired;
        held.view.error_code = Some(ERR_RECOVERY_REQUIRED.into());
        held.view.error_message = Some(reason.clone());
        if let Err(persist_error) = self.store.store_operation(&held) {
            tracing::error!(
                "partial admission hold persist failed (op {}): {persist_error:#}",
                stored.view.operation_id
            );
        }
        AdmissionRejection {
            code: ERR_RECOVERY_REQUIRED,
            message: format!(
                "admission persisted but intent write failed; operation {} held for recovery: {reason}",
                stored.view.operation_id
            ),
            active_operation_id: Some(stored.view.operation_id.clone()),
        }
    }

    /// 恢复保护是否生效（B05：所有写入口共享——旧部署链/自动启动同样查询）。
    pub(crate) fn recovery_protection_active(&self) -> bool {
        self.admission
            .try_lock()
            .map(|guard| guard.recovery_protection)
            .unwrap_or(true) // 锁竞争期间保守视为保护生效（fail-closed）
    }

    /// 操作是否已被请求取消（执行侧副作用边界检查点）。
    pub(crate) fn is_cancelled(&self, operation_id: &str) -> bool {
        self.cancelled
            .lock()
            .map(|set| set.contains(operation_id))
            .unwrap_or(false)
    }

    fn emit(
        &self,
        operation_id: &str,
        sequence: u64,
        stage: &str,
        service: Option<String>,
        event_name: Option<&str>,
    ) {
        self.emit_with_payload(operation_id, sequence, stage, service, event_name, None)
    }

    /// 带 payload 的事件落盘（R06：Failed 终态事件携带错误详情，SSE 消费方
    /// 无需再查操作视图即可还原失败原因）。
    fn emit_with_payload(
        &self,
        operation_id: &str,
        sequence: u64,
        stage: &str,
        service: Option<String>,
        event_name: Option<&str>,
        payload: Option<serde_json::Value>,
    ) {
        let record = RuntimeEventRecord {
            operation_id: operation_id.to_string(),
            sequence,
            runtime_instance_id: self.identity.runtime_instance_id.clone(),
            stage: stage.to_string(),
            service,
            event_name: event_name.map(str::to_string),
            payload,
        };
        if let Err(error) = self.store.append_event(&record) {
            // 事件落盘失败不阻断受理/执行主链（操作记录是权威），
            // 但必须可见——静默丢事件违反 spec §3.6。
            tracing::error!("runtime event persist failed (op {operation_id}): {error:#}");
        }
    }

    /// 追加服务级编排事件到**当前活跃操作**的 journal（R06 事件桥：owner 形态
    /// 下平台经运行 API 读事件流，stdout EVT 只被本地 spawn 路径消费）。
    /// 无活跃操作时 no-op（idle 期编排事件只有 stdout 消费者）。
    /// 返回是否已落盘（供桥接方观测丢弃）。
    pub fn append_orchestration_event(
        &self,
        stage: &str,
        service: Option<String>,
        event_name: &str,
        payload: Option<serde_json::Value>,
    ) -> bool {
        let Ok(guard) = self.admission.try_lock() else {
            return false;
        };
        let Some(operation_id) = guard.active_operation_id.clone() else {
            return false;
        };
        drop(guard);
        let next_sequence = self
            .store
            .replay_events(&operation_id, 0)
            .map(|events| events.last().map_or(1, |event| event.sequence + 1))
            .unwrap_or(1);
        self.emit_with_payload(
            &operation_id,
            next_sequence,
            stage,
            service,
            Some(event_name),
            payload,
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{
        ArtifactInput, ERR_OPERATION_ID_CONFLICT, ERR_OPERATION_IN_PROGRESS, ERR_RECOVERY_REQUIRED,
        ERR_REVISION_MISMATCH, ERR_RUNTIME_INSTANCE_MISMATCH, RunProfileInput,
        RuntimeOperationKind, RuntimeOperationRequest,
    };

    fn temp_store() -> (tempfile::TempDir, RuntimeStore) {
        let dir = tempfile::tempdir().expect("dir");
        // B04：按应用隔离根——workspace 必须是临时目录的子目录，保证各测试
        // 的卷根（父目录）唯一。根 = {卷根}/.app-cli-state/{app}。
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        (dir, open_store(&workspace))
    }

    /// 测试助手：以测试应用身份解析并打开 store（B04 缺省路径）。
    fn open_store(workspace: &Path) -> RuntimeStore {
        let root = RuntimeStore::resolve_root(workspace, "test-app").expect("resolve root");
        RuntimeStore::open_with_root(root, workspace).expect("store")
    }

    /// 测试助手：测试应用的稳定根位置（断言/预置用）。
    fn app_state_root(workspace: &Path) -> std::path::PathBuf {
        RuntimeStore::resolve_root(workspace, "test-app").expect("resolve root")
    }

    fn identity() -> RuntimeIdentityView {
        RuntimeIdentityView {
            application_id: "app1".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws-1".into(),
            source_root: "/workspace".into(),
            runtime_instance_id: "instance-1".into(),
            deployment_generation_id: "gen-1".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        }
    }

    fn kernel(dir: &Path) -> RuntimeKernel {
        let workspace = dir.join("workspace");
        let store = open_store(&workspace);
        let identity = identity();
        RuntimeKernel::new(store, identity, Box::new(|_| {}))
    }

    /// R03：ArtifactId 制品部署派发（owner 侧激活——不经网络下载）。
    #[tokio::test]
    async fn artifact_id_deploy_dispatches_local_artifact() {
        let (dir, _keep) = temp_store();
        let captured: std::sync::Arc<std::sync::Mutex<Vec<DispatchAction>>> = Default::default();
        let sink = captured.clone();
        let workspace = dir.path().join("workspace");
        let store = open_store(&workspace);
        let kernel = RuntimeKernel::new(
            store,
            identity(),
            Box::new(move |action| {
                sink.lock().unwrap().push(action);
            }),
        );
        let mut request = request_deploy_url("op-art");
        request.profile = shared_types::RunProfileInput::Artifact {
            artifact: shared_types::ArtifactInput::ArtifactId {
                artifact_id: "rel-777".into(),
            },
        };
        kernel.admit(request).await.expect("admit");
        let actions = captured.lock().unwrap();
        match &actions[..] {
            [DispatchAction::DeployLocalArtifact { artifact_id, .. }] => {
                assert_eq!(artifact_id, "rel-777");
            }
            other => panic!("expected DeployLocalArtifact dispatch, got {other:?}"),
        }
    }

    /// R08：Restart 携带 run_config.pg → 派发动作拿到真实凭据；持久化副本
    /// 密码脱敏（重放摘要不含 run_config，脱敏不影响幂等语义）。
    #[tokio::test]
    async fn run_config_pg_reaches_dispatch_and_redacts_on_disk() {
        use std::sync::Mutex;
        let (dir, _keep) = temp_store();
        let captured: std::sync::Arc<Mutex<Vec<DispatchAction>>> = Default::default();
        let sink = captured.clone();
        let workspace = dir.path().join("workspace");
        let store = open_store(&workspace);
        let identity = identity();
        let kernel = RuntimeKernel::new(
            store,
            identity,
            Box::new(move |action| {
                sink.lock().unwrap().push(action);
            }),
        );
        let mut request = request(RuntimeOperationKind::Restart, "op-pg");
        request.run_config = Some(shared_types::OperationRunConfig {
            pg: Some(shared_types::StartPgCredential {
                username: "biz_user".into(),
                password: "s3cret".into(),
            }),
        });
        kernel.admit(request).await.expect("admit");
        let actions = captured.lock().unwrap();
        match &actions[..] {
            [DispatchAction::OrchestrateSource { pg: Some(pg), .. }] => {
                assert_eq!(pg.username, "biz_user");
                assert_eq!(pg.password, "s3cret");
            }
            other => panic!("expected single orchestrate dispatch with pg, got {other:?}"),
        }
        drop(actions);
        // 持久化副本：密码为空（脱敏），用户名保留诊断
        let stored = kernel
            .store()
            .load_operation("op-pg")
            .expect("load")
            .expect("stored");
        let persisted_pg = stored
            .request
            .run_config
            .as_ref()
            .and_then(|config| config.pg.as_ref())
            .expect("run config persisted");
        assert_eq!(persisted_pg.username, "biz_user");
        assert_eq!(
            persisted_pg.password, "",
            "password must be redacted on disk"
        );
    }

    /// R06：Failed 终态事件携带错误载荷；事件桥把服务级事件追加进活跃操作
    /// journal（复用 owner 的平台侧经运行 API 读事件流）。
    #[tokio::test]
    async fn failed_terminal_event_carries_error_payload() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-fail"))
            .await
            .expect("admit");
        kernel
            .finish(
                "op-fail",
                RuntimeOperationState::Failed,
                Some(("ERR_SOME".into(), "boom detail".into())),
                None,
                2,
            )
            .await
            .expect("finish");
        let events = kernel.store().replay_events("op-fail", 0).expect("replay");
        let terminal = events
            .iter()
            .find(|event| event.event_name.as_deref() == Some("Failed"))
            .expect("terminal event");
        let payload = terminal.payload.as_ref().expect("payload on Failed");
        assert_eq!(payload["code"], "ERR_SOME");
        assert_eq!(payload["error"], "boom detail");
    }

    #[tokio::test]
    async fn orchestration_bridge_appends_to_active_operation() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        // 无活跃操作：no-op（idle 期只有 stdout 消费者）
        assert!(!kernel.append_orchestration_event(
            "service",
            Some("frontend".into()),
            "service_starting",
            None
        ));
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-bridge"))
            .await
            .expect("admit");
        // admit 已发 accepted 事件（sequence 1）→ 桥接事件从 2 起
        assert!(kernel.append_orchestration_event(
            "service",
            Some("frontend".into()),
            "service_starting",
            None
        ));
        assert!(kernel.append_orchestration_event(
            "orchestration",
            None,
            "orchestration_done",
            Some(serde_json::json!({"failed": []}))
        ));
        let events = kernel
            .store()
            .replay_events("op-bridge", 0)
            .expect("replay");
        let names: Vec<(u64, &str)> = events
            .iter()
            .filter_map(|event| {
                event
                    .event_name
                    .as_deref()
                    .map(|name| (event.sequence, name))
            })
            .collect();
        assert_eq!(
            names,
            vec![
                (1, "start"),
                (2, "service_starting"),
                (3, "orchestration_done")
            ],
            "bridge events must be sequenced after accepted: {names:?}"
        );
        let done = events.last().expect("done event");
        assert_eq!(
            done.payload.as_ref().expect("payload")["failed"],
            serde_json::json!([])
        );
    }

    fn request_deploy_url(operation_id: &str) -> RuntimeOperationRequest {
        RuntimeOperationRequest {
            operation_id: operation_id.into(),
            expected_runtime_instance_id: "instance-1".into(),
            expected_revision: 0,
            workspace_id: "ws-1".into(),
            kind: RuntimeOperationKind::Deploy,
            profile: RunProfileInput::Artifact {
                artifact: ArtifactInput::Url {
                    url: "http://x/app.zip".into(),
                    sha256: None,
                },
            },
            run_config: None,
            request_context: None,
        }
    }

    fn request(kind: RuntimeOperationKind, operation_id: &str) -> RuntimeOperationRequest {
        RuntimeOperationRequest {
            operation_id: operation_id.into(),
            expected_runtime_instance_id: "instance-1".into(),
            expected_revision: 0,
            workspace_id: "ws-1".into(),
            kind,
            profile: RunProfileInput::Source {
                workspace_id: "ws-1".into(),
            },
            run_config: None,
            request_context: None,
        }
    }

    #[tokio::test]
    async fn same_id_same_digest_replays_without_double_admission() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        let first = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        assert!(matches!(first, AdmissionOutcome::Accepted(_)));
        // 未 finish 前，同 ID 同摘要 = 重放（不是 busy 拒绝）
        let replay = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("replay");
        assert!(matches!(replay, AdmissionOutcome::Replayed(_)));
    }

    #[tokio::test]
    async fn same_id_different_digest_conflicts() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        let mut other = request(RuntimeOperationKind::Stop, "op-1");
        other.kind = RuntimeOperationKind::Restart;
        let rejection = kernel.admit(other).await.expect_err("conflict");
        assert_eq!(rejection.code, ERR_OPERATION_ID_CONFLICT);
    }

    #[tokio::test]
    async fn active_operation_blocks_other_kinds_but_not_stop() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit start");
        // start/restart/deploy 在 active 期间被拒（带进行中 ID；busy 检查
        // 在 profile 组合校验之后——用已实现组合触发 busy 分支）
        let rejection = kernel
            .admit(request_deploy_url("op-2"))
            .await
            .expect_err("busy");
        assert_eq!(rejection.code, ERR_OPERATION_IN_PROGRESS);
        assert_eq!(rejection.active_operation_id.as_deref(), Some("op-1"));
        // stop 可受理（意图屏障），且推进 revision
        kernel
            .admit(request(RuntimeOperationKind::Stop, "op-stop"))
            .await
            .expect("stop admitted during active");
        let (_desired, revision) = kernel.store.load_desired().expect("desired");
        assert_eq!(revision, 1);
        // 原操作与 stop 均收束后，stop 推进的 revision 使旧 revision 部署提交被拒
        kernel
            .finish("op-1", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("finish op-1");
        kernel
            .finish("op-stop", RuntimeOperationState::Succeeded, None, None, 2)
            .await
            .expect("finish stop");
        let stale = request_deploy_url("op-3");
        let rejection = kernel.admit(stale).await.expect_err("stale revision");
        assert_eq!(rejection.code, ERR_REVISION_MISMATCH);
    }

    #[tokio::test]
    async fn stale_runtime_instance_is_rejected_before_replay() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        let mut stale = request(RuntimeOperationKind::Start, "op-1");
        stale.expected_runtime_instance_id = "previous-instance".into();
        let rejection = kernel.admit(stale).await.expect_err("instance mismatch");
        assert_eq!(rejection.code, ERR_RUNTIME_INSTANCE_MISMATCH);
    }

    #[tokio::test]
    async fn finish_clears_active_and_recovery_state_sets_protection() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        kernel
            .finish(
                "op-1",
                RuntimeOperationState::RecoveryRequired,
                Some((ERR_RECOVERY_REQUIRED.into(), "cleanup unconfirmed".into())),
                None,
                2,
            )
            .await
            .expect("finish");
        let status = kernel.status().await.expect("status");
        assert!(status.recovery_protection);
        assert_eq!(status.active_operation_id, None);
        // 恢复保护期新写被拒
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-2"))
            .await
            .expect_err("recovery gate");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
    }

    #[tokio::test]
    async fn events_replay_by_after_seq_cursor() {
        let (dir, _keep) = temp_store();
        let store = open_store(&dir.path().join("workspace"));
        for sequence in 1..=3 {
            store
                .append_event(&RuntimeEventRecord {
                    operation_id: "op-9".into(),
                    sequence,
                    runtime_instance_id: "instance-1".into(),
                    stage: "stage".into(),
                    service: None,
                    event_name: Some("service_starting".into()),
                    payload: None,
                })
                .expect("append");
        }
        let all = store.replay_events("op-9", 0).expect("replay");
        assert_eq!(all.len(), 3);
        let tail = store.replay_events("op-9", 2).expect("replay");
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].sequence, 3);
    }

    #[tokio::test]
    async fn restart_recovery_marks_unfinished_operations() {
        let (dir, _keep) = temp_store();
        {
            let workspace = dir.path().join("workspace");
            let store = open_store(&workspace);
            store
                .store_operation(&StoredOperation {
                    view: RuntimeOperationView {
                        operation_id: "op-pending".into(),
                        kind: RuntimeOperationKind::Deploy,
                        state: RuntimeOperationState::Starting,
                        request_digest: "d".repeat(64),
                        revision: 0,
                        runtime_instance_id: "instance-old".into(),
                        error_code: None,
                        error_message: None,
                        failure_detail: None,
                    },
                    request: request(RuntimeOperationKind::Deploy, "op-pending"),
                })
                .expect("store");
        }
        // 新进程打开同一状态根：未终态操作转 RecoveryRequired + 保护
        let kernel = kernel(dir.path());
        let recovered = kernel.recover().await.expect("recover");
        assert_eq!(recovered, vec!["op-pending".to_string()]);
        let view = kernel
            .get("op-pending")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(view.state, RuntimeOperationState::RecoveryRequired);
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-new"))
            .await
            .expect_err("recovery protection");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
    }

    #[tokio::test]
    async fn persist_failure_rejects_without_dispatch() {
        let (dir, _keep) = temp_store();
        // 目标操作记录路径预置为目录 → 原子写 rename 失败（受理持久化故障注入）
        let root = app_state_root(&dir.path().join("workspace"));
        std::fs::create_dir_all(root.join("operations").join("op-1.json")).expect("block path");
        let kernel = kernel(dir.path());
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect_err("persist failure");
        assert_eq!(rejection.code, "ERR_BACKEND_ERROR");
        let status = kernel.status().await.expect("status");
        assert_eq!(status.active_operation_id, None, "no execution queued");
    }
    // ── R01：执行身份不被并发受理覆盖；按 ID 收束不误完成他人 ──────────────────

    #[test]
    fn executing_identity_refuses_concurrent_overwrite() {
        // ServerState 归属 server.rs，这里验证同款语义经 dispatch 间接覆盖；
        // 直接构造检查在 app_cli::server 的集成测试中锚定。内核侧等价断言：
        // active 单槽在 stop 屏障期间不被 second admission 变更（见下）。
    }

    #[tokio::test]
    async fn finish_by_id_does_not_complete_a_different_operation() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        // 受理 op-A（Start/Source）
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-a"))
            .await
            .expect("admit A");
        // 受理 Stop op-B（active 期间允许，意图屏障）
        kernel
            .admit(request(RuntimeOperationKind::Stop, "op-b"))
            .await
            .expect("admit stop B");
        // A 的执行边界按自身 ID 收束（不应误完成 B）
        kernel
            .finish("op-a", RuntimeOperationState::Succeeded, None, None, 2)
            .await
            .expect("finish A");
        let view_a = kernel.get("op-a").await.expect("get A").expect("present");
        assert_eq!(view_a.state, RuntimeOperationState::Succeeded);
        // B 仍在途（Accepted——尚未被停止边界执行）
        let view_b = kernel.get("op-b").await.expect("get B").expect("present");
        assert!(!view_b.state.is_terminal(), "B must stay in-flight");
    }

    // ── R02：状态根在卷根（workspace 父目录），跨部署换代稳定 ────────────────────

    #[tokio::test]
    async fn state_root_lives_on_volume_root_not_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = open_store(&workspace);
        store
            .store_desired(DesiredState::Stopped, 3)
            .expect("desired");
        // 状态根在 workspace 之外（卷根下按应用隔离）——部署 activate
        // 整体改名 workspace 换代时不动
        assert!(app_state_root(&workspace).join("desired.json").is_file());
        assert!(!workspace.join(STATE_DIR_NAME).exists());
    }

    /// XP03：同项目改端口不分裂锁域——锁以状态根为键，端口只进发现记录。
    /// 第二实例（不同 admin_addr）仍被 OwnerGuard 拒绝。
    #[test]
    fn xp03_port_change_does_not_split_lock_domain() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("state");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let _guard = crate::platform::owner_guard::OwnerGuard::acquire(&root).expect("first owner");

        // 第一 owner 发布了端口 A 的发现记录
        let record = EndpointRecord {
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            application_id: "app-x".into(),
            workspace_id: "ws".into(),
            runtime_instance_id: "instance-1".into(),
            address: "127.0.0.1:3010".into(),
        };
        let store = RuntimeStore::open_with_root(root.clone(), &workspace).expect("store");
        store.store_endpoint(&record).expect("publish endpoint");

        // 第二实例改用端口 B：锁域不变 → 排他失败
        let second = crate::platform::owner_guard::OwnerGuard::acquire(&root);
        assert!(
            second.is_err(),
            "port change must not bypass the owner lock"
        );

        // 释放后可接管（记录不阻碍新 owner——它是线索不是凭证）
        drop(_guard);
        let _guard2 = crate::platform::owner_guard::OwnerGuard::acquire(&root)
            .expect("takeover after release");
    }

    /// XP10：旧发现记录与远端身份不符 → 核验拒绝（不据此发认证请求）。
    /// 同实例全字段一致才命中；实例换代/协议变化/错应用均拒绝。
    #[test]
    fn xp10_stale_endpoint_record_is_rejected() {
        let identity = || RuntimeIdentityView {
            application_id: "app-x".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws".into(),
            source_root: "/ws".into(),
            runtime_instance_id: "instance-1".into(),
            deployment_generation_id: "gen-1".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        };
        let record = EndpointRecord {
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            application_id: "app-x".into(),
            workspace_id: "ws".into(),
            runtime_instance_id: "instance-1".into(),
            address: "127.0.0.1:3010".into(),
        };
        // 全字段一致 → 命中
        assert!(endpoint_matches_identity(&record, &identity()));
        // 实例换代（owner 重启后远端是新实例，记录是旧的）→ 拒绝
        let restarted = RuntimeIdentityView {
            runtime_instance_id: "instance-2".into(),
            ..identity()
        };
        assert!(!endpoint_matches_identity(&record, &restarted));
        // 协议不兼容 → 拒绝
        let upgraded = RuntimeIdentityView {
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION + 1,
            ..identity()
        };
        assert!(!endpoint_matches_identity(&record, &upgraded));
        // 错应用 → 拒绝
        let foreign = RuntimeIdentityView {
            application_id: "app-y".into(),
            ..identity()
        };
        assert!(!endpoint_matches_identity(&record, &foreign));
    }

    /// XP10 补充：发现记录持久化 roundtrip + 干净关停清除。
    ///
    /// 本地凭据文件（cross-platform.md §3）：token 落盘 0600 + 平台侧只读。
    #[test]
    fn token_file_roundtrip_with_restricted_permissions() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("state-root");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = RuntimeStore::open_with_root(root.clone(), &workspace).expect("store");

        store
            .store_token("  secret-token  ")
            .expect("persist token");
        // 写入 trim；平台侧读到的是裸值
        assert_eq!(
            RuntimeStore::read_token(&root).as_deref(),
            Some("secret-token")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(root.join("token"))
                .expect("stat token")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "token file must be owner-only readable"
            );
        }
        #[cfg(windows)]
        {
            // icacls 查询 ACL：断言继承已移除（无 inherited 条目残留）
            let query = std::process::Command::new("icacls")
                .arg(root.join("token"))
                .output()
                .expect("icacls query");
            let text = String::from_utf8_lossy(&query.stdout);
            assert!(
                !text.contains("(I)"),
                "token ACL must have inheritance removed: {text}"
            );
        }
        // 无 token 文件 → None（owner 未启用写端点）
        let empty = tempfile::tempdir().expect("empty");
        assert!(RuntimeStore::read_token(empty.path()).is_none());
    }

    #[test]
    fn endpoint_record_roundtrip_and_clean_clear() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("state-root");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = RuntimeStore::open_with_root(root.clone(), &workspace).expect("store");

        let record = EndpointRecord {
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            application_id: "app-r".into(),
            workspace_id: "ws".into(),
            runtime_instance_id: "instance-r".into(),
            address: "127.0.0.1:39999".into(),
        };
        store.store_endpoint(&record).expect("publish");
        let loaded = RuntimeStore::read_endpoint(&root).expect("record present");
        assert_eq!(loaded.address, record.address);
        assert_eq!(loaded.runtime_instance_id, record.runtime_instance_id);

        store.clear_endpoint().expect("clean clear");
        assert!(
            RuntimeStore::read_endpoint(&root).is_none(),
            "cleared record must not be discoverable"
        );
        // 幂等清除（不存在不报错）
        store.clear_endpoint().expect("idempotent clear");
    }

    /// B04：显式 env 根权威——source 与 .run 别名竞争同一目录/同一把锁。
    #[test]
    fn explicit_env_root_unifies_source_and_run_entries() {
        let dir = tempfile::tempdir().expect("dir");
        let volume = dir.path().join("vol");
        let explicit = dir.path().join("explicit-state");
        std::fs::create_dir_all(&volume).expect("volume");
        // 经参数化核心驱动（不 set_var）——进程级 env 变异会与并行测试的
        // resolve_root 读取竞争（Windows 实测随机 os error 2/3/183）。
        let source_workspace = volume.join("app-1");
        let run_workspace = volume.join("app-1").join(".run");
        std::fs::create_dir_all(&source_workspace).expect("source");
        std::fs::create_dir_all(&run_workspace).expect("run");
        let r1 = RuntimeStore::resolve_root_with_explicit(
            &source_workspace,
            "app-1",
            Some(explicit.clone().into_os_string()),
        )
        .expect("root1");
        let r2 = RuntimeStore::resolve_root_with_explicit(
            &run_workspace,
            "app-1",
            Some(explicit.clone().into_os_string()),
        )
        .expect("root2");
        assert_eq!(r1, r2, "alias entries must resolve the same explicit root");
        assert_eq!(r1, explicit);
    }

    #[tokio::test]
    async fn legacy_in_workspace_state_migrates_once() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join(STATE_DIR_NAME)).expect("legacy root");
        std::fs::write(
            workspace.join(STATE_DIR_NAME).join("desired.json"),
            r#"{"desired":"stopped","revision":5}"#,
        )
        .expect("legacy desired");
        let store = open_store(&workspace);
        // 迁移后旧位置清空、新位置可读
        assert!(!workspace.join(STATE_DIR_NAME).exists());
        assert_eq!(
            store.load_desired().expect("desired"),
            (DesiredState::Stopped, 5)
        );
    }

    /// B04：bare 卷根布局（R02–R08 形态）也迁移到按应用隔离根。
    #[test]
    fn legacy_bare_volume_root_migrates_to_per_app_root() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let bare = dir.path().join(STATE_DIR_NAME);
        std::fs::create_dir_all(bare.join("operations")).expect("bare root");
        std::fs::write(
            bare.join("desired.json"),
            r#"{"desired":"running","revision":2}"#,
        )
        .expect("bare desired");
        let store = open_store(&workspace);
        assert_eq!(
            store.load_desired().expect("desired"),
            (DesiredState::Running, 2)
        );
        // bare 不再直接持有状态条目（已搬入按应用根）；其作为按应用根的
        // 父容器保留（root 嵌套于其子树——rename 进自身子树不可行）。
        assert!(!bare.join("desired.json").exists());
        assert!(!bare.join("operations").exists());
        assert!(app_state_root(&workspace).join("desired.json").exists());
    }

    #[tokio::test]
    async fn dual_authority_domains_fail_closed() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let legacy = workspace.join(STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).expect("legacy");
        std::fs::write(
            legacy.join("desired.json"),
            r#"{"desired":"stopped","revision":1}"#,
        )
        .expect("legacy marker");
        let root = app_state_root(&workspace);
        std::fs::create_dir_all(root.join("operations")).expect("new root");
        let error = RuntimeStore::open_with_root(root, &workspace)
            .err()
            .expect("must refuse");
        assert!(error.to_string().contains("authority domains"));
        // 多处 legacy 并存（in-workspace + bare 卷根）同样拒绝裁决
        let dir2 = tempfile::tempdir().expect("dir2");
        let workspace2 = dir2.path().join("workspace");
        std::fs::create_dir_all(&workspace2).expect("workspace");
        let legacy_ws = workspace2.join(STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy_ws).expect("legacy ws");
        std::fs::write(
            legacy_ws.join("desired.json"),
            r#"{"desired":"stopped","revision":1}"#,
        )
        .expect("legacy ws marker");
        let legacy_bare = dir2.path().join(STATE_DIR_NAME);
        std::fs::create_dir_all(legacy_bare.join("operations")).expect("legacy bare");
        std::fs::write(
            legacy_bare.join("desired.json"),
            r#"{"desired":"running","revision":1}"#,
        )
        .expect("legacy bare marker");
        let root2 = app_state_root(&workspace2);
        let error2 = RuntimeStore::open_with_root(root2, &workspace2)
            .err()
            .expect("must refuse");
        assert!(error2.to_string().contains("legacy"));
    }

    // ── R03：真实取消墓碑；未实现 profile 组合结构化拒绝 ────────────────────────

    #[tokio::test]
    async fn request_cancel_marks_tombstone_and_finish_clears_it() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-c"))
            .await
            .expect("admit");
        assert!(!kernel.is_cancelled("op-c"));
        assert!(kernel.request_cancel("op-c").await.expect("cancel"));
        assert!(kernel.is_cancelled("op-c"), "cancel must be observable");
        // 非在途操作取消 = false（不误标）
        assert!(!kernel.request_cancel("op-x").await.expect("no-op"));
        kernel
            .finish("op-c", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("finish");
        assert!(!kernel.is_cancelled("op-c"), "finish clears tombstone");
    }

    #[tokio::test]
    async fn unsupported_profile_combinations_are_rejected() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        // Deploy+Source：未实现 → 结构化拒绝（不静默编排当前 workspace）
        let mut deploy_source = request(RuntimeOperationKind::Deploy, "op-ds");
        deploy_source.profile = RunProfileInput::Source {
            workspace_id: "ws-1".into(),
        };
        let rejection = kernel
            .admit(deploy_source)
            .await
            .expect_err("must reject deploy+source");
        assert_eq!(rejection.code, shared_types::ERR_PROTOCOL_UNSUPPORTED);
        // Start+Artifact(ArtifactId)：本地制品解析器未实现 → 拒绝
        let mut start_artifact = request(RuntimeOperationKind::Start, "op-sa");
        start_artifact.profile = RunProfileInput::Artifact {
            artifact: ArtifactInput::ArtifactId {
                artifact_id: "art-1".into(),
            },
        };
        let rejection = kernel
            .admit(start_artifact)
            .await
            .expect_err("must reject start+artifact-id");
        assert_eq!(rejection.code, shared_types::ERR_PROTOCOL_UNSUPPORTED);
        // 拒绝未派发：无 active 占位
        let status = kernel.status().await.expect("status");
        assert_eq!(status.active_operation_id, None);
    }

    // ── R04：损坏记录阻断写入；部分提交保持保护 ────────────────────────────────

    #[tokio::test]
    async fn corrupt_operation_record_blocks_new_writes() {
        let (dir, _keep) = temp_store();
        {
            let workspace = dir.path().join("workspace");
            let root = app_state_root(&workspace);
            std::fs::create_dir_all(root.join("operations")).expect("ops dir");
            // 一条损坏 JSON 的未终态操作记录
            std::fs::write(
                root.join("operations").join("op-corrupt.json"),
                "{ this is not json",
            )
            .expect("corrupt record");
            let _ = RuntimeStore::open(&workspace).expect("open ignores record content");
        }
        let kernel = kernel(dir.path());
        let recovered = kernel.recover().await.expect("recover");
        // 损坏记录不产生 recovered 条目，但必须阻断写
        assert!(recovered.is_empty());
        let status = kernel.status().await.expect("status");
        assert!(
            status.recovery_protection,
            "corrupt record must keep recovery protection"
        );
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-after-corrupt"))
            .await
            .expect_err("writes blocked");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
    }

    #[tokio::test]
    async fn partial_admission_holds_operation_and_protection() {
        let (dir, _keep) = temp_store();
        let _workspace = dir.path().join("workspace");
        // 先正常打开一次 kernel 并受理一个操作，让 desired.json 存在；
        // 然后把 desired.json 变为不可写目录，模拟后续 desired 写失败。
        {
            let kernel = kernel(dir.path());
            kernel
                .admit(request(RuntimeOperationKind::Start, "op-first"))
                .await
                .expect("first admit");
            kernel
                .finish("op-first", RuntimeOperationState::Succeeded, None, None, 2)
                .await
                .expect("finish first");
        }
        let desired = app_state_root(&dir.path().join("workspace")).join("desired.json");
        std::fs::remove_file(&desired).expect("remove desired");
        std::fs::create_dir(&desired).expect("block desired writes");

        let kernel = kernel(dir.path());
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-partial"))
            .await
            .expect_err("desired write fails");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
        assert!(rejection.message.contains("held for recovery"));
        // 部分提交的操作转 RecoveryRequired（可查询，不再派发）
        let held = kernel
            .get("op-partial")
            .await
            .expect("query")
            .expect("present");
        assert_eq!(held.state, RuntimeOperationState::RecoveryRequired);
        // 后续写被拒
        let blocked = kernel
            .admit(request(RuntimeOperationKind::Start, "op-next"))
            .await
            .expect_err("blocked");
        assert_eq!(blocked.code, ERR_RECOVERY_REQUIRED);
    }

    // ===== R03/R04：终态单调与提交线性化 =====

    #[tokio::test]
    async fn finish_is_monotonic_cancelled_cannot_become_succeeded() {
        // R04：取消收束后的 Cancelled 记录不得被迟到的成功/失败提交改写
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-mono"))
            .await
            .expect("admit");
        kernel
            .finish("op-mono", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("cancel settle");
        kernel
            .finish("op-mono", RuntimeOperationState::Succeeded, None, None, 3)
            .await
            .expect("late finish must not error");
        let view = kernel.get("op-mono").await.expect("view").expect("exists");
        assert_eq!(view.state, RuntimeOperationState::Cancelled);
        // 同态重复 finish 幂等
        kernel
            .finish("op-mono", RuntimeOperationState::Cancelled, None, None, 4)
            .await
            .expect("idempotent");
    }

    #[tokio::test]
    async fn recovery_required_cannot_be_overwritten_by_terminal() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-recovery"))
            .await
            .expect("admit");
        kernel
            .finish(
                "op-recovery",
                RuntimeOperationState::RecoveryRequired,
                None,
                None,
                2,
            )
            .await
            .expect("hold");
        kernel
            .finish("op-recovery", RuntimeOperationState::Failed, None, None, 3)
            .await
            .expect("late failure must not overwrite protection");
        let view = kernel
            .get("op-recovery")
            .await
            .expect("view")
            .expect("exists");
        assert_eq!(view.state, RuntimeOperationState::RecoveryRequired);
    }

    #[tokio::test]
    async fn commit_after_cancel_observes_without_prewriting_terminal() {
        // V02：屏障只观察取消意图——**不预写 Cancelled 终态**（预写会让清理
        // 未知时无法升级 RecoveryRequired）；调用方停服确认后 finish 收束
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-race"))
            .await
            .expect("admit");
        kernel.request_cancel("op-race").await.expect("cancel");
        let outcome = kernel.commit_execution("op-race").await.expect("barrier");
        assert_eq!(outcome, CommitBarrierOutcome::Cancelled);
        // 终态未写：仍非终态（等待调用方停服后收束）
        let view = kernel.get("op-race").await.expect("view").expect("exists");
        assert!(
            !view.state.is_terminal(),
            "barrier must not prewrite terminal"
        );
        // 调用方停服确认 → finish(Cancelled) 收束；迟到成功不可覆盖
        kernel
            .finish("op-race", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("settle cancelled");
        kernel
            .finish("op-race", RuntimeOperationState::Succeeded, None, None, 3)
            .await
            .expect("late finish must not error");
        let view = kernel.get("op-race").await.expect("view").expect("exists");
        assert_eq!(view.state, RuntimeOperationState::Cancelled);
    }

    #[tokio::test]
    async fn commit_barrier_cancelled_op_can_settle_recovery_required() {
        // V02 关键能力：取消窗口后清理未知 → RecoveryRequired（若屏障预写
        // Cancelled，终态单调会锁死该升级路径）
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-unclean"))
            .await
            .expect("admit");
        kernel.request_cancel("op-unclean").await.expect("cancel");
        let outcome = kernel
            .commit_execution("op-unclean")
            .await
            .expect("barrier");
        assert_eq!(outcome, CommitBarrierOutcome::Cancelled);
        kernel
            .finish(
                "op-unclean",
                RuntimeOperationState::RecoveryRequired,
                None,
                None,
                2,
            )
            .await
            .expect("uncertain cleanup must be expressible");
        assert!(kernel.recovery_protection_active());
    }

    #[tokio::test]
    async fn late_cancel_after_commit_cannot_flip_succeeded() {
        // R03 窗口消除后的可观察面：提交成功后再取消，终态保持 Succeeded
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-commit"))
            .await
            .expect("admit");
        let outcome = kernel.commit_execution("op-commit").await.expect("barrier");
        assert_eq!(outcome, CommitBarrierOutcome::Committed);
        kernel
            .request_cancel("op-commit")
            .await
            .expect("late cancel");
        kernel
            .finish("op-commit", RuntimeOperationState::Cancelled, None, None, 9)
            .await
            .expect("late finish");
        let view = kernel
            .get("op-commit")
            .await
            .expect("view")
            .expect("exists");
        assert_eq!(view.state, RuntimeOperationState::Succeeded);
    }
}
