use super::*;

#[cfg(test)]
#[path = "store_recovery_tests.rs"]
mod recovery_tests;

pub struct RuntimeStore {
    pub(super) root: PathBuf,
    // Keep the legacy lock domain fenced while the migrated owner is alive.
    _legacy_owners: Vec<runtime_supervisor::Owner>,
    legacy_bindings: Vec<runtime_supervisor::Binding>,
    legacy_roots: std::sync::Mutex<Vec<PathBuf>>,
    legacy_desired: Option<(DesiredState, u64)>,
    #[cfg(test)]
    pub(super) desired_write_fault: std::sync::atomic::AtomicBool,
}

/// 扫描结果（R04）：读取/解码失败不再静默跳过——损坏记录**隔离**进
/// `operations-quarantine/`（原文件保留可查，不再阻断启动：产品环境没有
/// 操作员，任何"等人工裁决"的状态都是用户不可恢复的死锁）。
pub(crate) struct RecoveryScan {
    /// 被转 RecoveryRequired 的在途操作（Stop 交精确收敛；其余由启动序列
    /// 末尾的 [`RuntimeKernel::settle_unresolved_recoveries`] 自动收敛）。
    pub recovered: Vec<String>,
    /// 已隔离的损坏记录（移入 quarantine 目录，业务不受阻断）。
    pub quarantined: Vec<String>,
    /// 隔离失败的记录（存储层故障——此时才保持恢复保护阻断写）。
    pub blocked: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CancellationReceipt {
    operation_id: String,
    request_digest: String,
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

pub(super) fn endpoint_path(state_root: &Path) -> PathBuf {
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
    pub(super) fn resolve_root_with_explicit(
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

    /// B04：以显式根打开。旧位置在新旧锁域保护下逐项迁移；中断导致
    /// 双持久权威域时 fail-fast，不覆盖或猜测残留内容：
    /// - `{workspace}/.app-cli-state`（R02 之前的 in-workspace 布局）
    /// - `{workspace 卷根}/.app-cli-state`（R02–R08 的 bare 卷根布局——
    ///   本轮加入 application_id 段后成为 legacy）
    pub(crate) fn open_with_root(root: PathBuf, workspace: &Path) -> Result<Self> {
        let authorities = Self::discover_legacy_authorities(&root, workspace)?;
        let (existing_legacy, legacy_bindings): (Vec<_>, Vec<_>) = authorities.into_iter().unzip();
        // Acquire every actual legacy authority before interpreting any of its
        // ordinary records. Mere coexistence/mtime is not a live second owner.
        let mut legacy_owners = Vec::new();
        for legacy in &existing_legacy {
            let guard = runtime_supervisor::Owner::try_acquire(legacy)?
                .context("legacy runtime owner still owns its execution scope")?;
            legacy_owners.push(guard);
        }
        // Expose an unambiguous old desired revision while management assembles,
        // so early Stop/new intent cannot be superseded by the later migration.
        let legacy_desired = legacy_desired_base(&existing_legacy)?;
        std::fs::create_dir_all(root.join("operations")).context("create runtime state dir")?;
        std::fs::create_dir_all(root.join("events")).context("create runtime events dir")?;
        Ok(Self {
            root,
            _legacy_owners: legacy_owners,
            legacy_bindings,
            legacy_roots: std::sync::Mutex::new(existing_legacy),
            legacy_desired,
            #[cfg(test)]
            desired_write_fault: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Shared, read-only scope selection for preparation and retained leases.
    /// Caller still authenticates any live native owner before its Shutdown.
    pub(crate) fn discover_legacy_authorities(
        root: &Path,
        workspace: &Path,
    ) -> Result<Vec<(PathBuf, runtime_supervisor::Binding)>> {
        let project = runtime_state_layout::canonical_project_root(workspace);
        let volume = project
            .parent()
            .context("workspace has no volume root for stable runtime state")?;
        let legacy_locations = [
            workspace.join(STATE_DIR_NAME),
            project.join(STATE_DIR_NAME),
            volume.join(STATE_DIR_NAME),
        ];
        let root_identity = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut checked = std::collections::BTreeSet::new();
        let mut existing_legacy = Vec::new();
        let mut legacy_bindings = Vec::new();
        for legacy in legacy_locations {
            let physical = std::fs::canonicalize(&legacy).unwrap_or_else(|_| legacy.clone());
            if physical == root_identity
                || !checked.insert(physical)
                || !(is_legacy_state_domain(&legacy)?
                    || legacy.join("supervisor.json").try_exists()?
                    || legacy.join("work").try_exists()?
                    || legacy.join("resident").try_exists()?)
            {
                continue;
            }
            if !legacy_belongs_to_project(&legacy, &project)? {
                tracing::warn!(root = %legacy.display(), "foreign legacy application/project retained without importing its records");
                continue;
            }
            let Some(binding) = legacy_native_binding(&legacy, &project)? else {
                tracing::warn!(root = %legacy.display(), "foreign legacy native component retained without touching its authority");
                continue;
            };
            existing_legacy.push(legacy);
            legacy_bindings.push(binding);
        }
        Ok(existing_legacy.into_iter().zip(legacy_bindings).collect())
    }

    pub(crate) async fn drain_legacy_execution(
        &self,
        application_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        for (owner, binding) in self._legacy_owners.iter().zip(&self.legacy_bindings) {
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "legacy execution cleanup deadline exceeded"
            );
            let cleanup = crate::supervision::cleanup_command(owner.scope_root())?;
            owner
                .drain_previous_ranges(binding, application_id, &cleanup, deadline)
                .await?;
        }
        Ok(())
    }

    /// Called only after startup stopped the actual prior business execution.
    /// Retain current intent/credentials; import missing history and archive
    /// colliding bytes. Database/migration records are outside this allowlist.
    pub(crate) fn reconcile_legacy_after_quiescence(&self) -> Result<()> {
        let mut roots = self
            .legacy_roots
            .lock()
            .map_err(|_| anyhow::anyhow!("legacy runtime recovery lock poisoned"))?;
        if !roots.is_empty()
            && !root_exists(&self.desired_path())?
            && let Some((desired, revision)) = self.legacy_desired
        {
            self.store_desired(desired, revision)?;
        }
        for legacy in roots.iter() {
            let backup = legacy.join(format!(
                ".runtime-history-{}",
                uuid::Uuid::new_v4().simple()
            ));
            for name in RUNTIME_STATE_ENTRIES {
                let source = legacy.join(name);
                if !root_exists(&source)? {
                    continue;
                }
                let target = self.root.join(name);
                if std::fs::symlink_metadata(&source)?.file_type().is_dir() && target.is_dir() {
                    for entry in std::fs::read_dir(&source)? {
                        let entry = entry?;
                        let destination = target.join(entry.file_name());
                        let destination = if root_exists(&destination)? {
                            let archived = backup.join(name);
                            std::fs::create_dir_all(&archived)?;
                            archived.join(entry.file_name())
                        } else {
                            destination
                        };
                        std::fs::rename(entry.path(), destination)
                            .context("preserve legacy runtime operation history")?;
                    }
                    std::fs::remove_dir(&source)
                        .context("retire empty legacy runtime history directory")?;
                } else {
                    let destination = if root_exists(&target)? {
                        std::fs::create_dir_all(&backup)?;
                        backup.join(name)
                    } else {
                        target
                    };
                    std::fs::rename(&source, destination)
                        .context("preserve legacy runtime control record")?;
                }
            }
            #[cfg(unix)]
            {
                std::fs::File::open(legacy)?.sync_all()?;
                std::fs::File::open(&self.root)?.sync_all()?;
            }
            tracing::warn!(legacy = %legacy.display(), authority = %self.root.display(),
                "runtime control history reconciled after actual business quiescence");
        }
        roots.clear();
        Ok(())
    }

    pub(super) fn identity_path(&self) -> PathBuf {
        self.root.join("identity.json")
    }
    pub(super) fn desired_path(&self) -> PathBuf {
        self.root.join("desired.json")
    }
    pub(super) fn operation_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join("operations")
            .join(format!("{operation_id}.json"))
    }
    pub(super) fn events_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join("events")
            .join(format!("{operation_id}.jsonl"))
    }

    pub(super) fn cancellation_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join("cancellations")
            .join(format!("{operation_id}.json"))
    }

    pub(super) fn store_cancellation(&self, operation: &StoredOperation) -> Result<()> {
        write_json(
            &self.cancellation_path(&operation.view.operation_id),
            &CancellationReceipt {
                operation_id: operation.view.operation_id.clone(),
                request_digest: operation.view.request_digest.clone(),
            },
        )
    }

    pub(super) fn cancellation_recorded(&self, operation: &StoredOperation) -> Result<bool> {
        let Some(value) = read_json(&self.cancellation_path(&operation.view.operation_id))? else {
            return Ok(false);
        };
        let receipt: CancellationReceipt =
            serde_json::from_value(value).context("decode cancellation receipt")?;
        anyhow::ensure!(
            receipt.operation_id == operation.view.operation_id
                && receipt.request_digest == operation.view.request_digest,
            "cancellation receipt identity mismatch"
        );
        Ok(true)
    }

    /// owner 绑定成功后原子发布 endpoint 发现记录（cross-platform.md §3）。
    /// 多项目天然隔离：状态根按 application_id 分目录，各项目各一份记录。
    pub(crate) fn store_endpoint(&self, record: &EndpointRecord) -> Result<()> {
        write_json(&endpoint_path(&self.root), record)
    }

    /// Discovery only: a failed kernel still owns its actual bound listener.
    /// Clients authenticate its native owner before deciding to rebuild it.
    pub(crate) fn write_endpoint_hint(root: &Path, record: &EndpointRecord) -> Result<()> {
        write_json(&endpoint_path(root), record)
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
        self.store_private_bytes("token", token.trim().as_bytes())
    }

    pub(super) fn store_private_bytes(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let destination = self.root.join(name);
        let mut file = tempfile::NamedTempFile::new_in(&self.root)
            .context("create private owner credential file")?;
        let path = file.path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
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
                .arg(path)
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
        // Protect the empty temporary file before writing credentials, then
        // publish atomically so a concurrent CLI never reads a partial token.
        use std::io::Write as _;
        file.write_all(bytes)
            .and_then(|_| file.as_file().sync_all())
            .context("persist owner credential")?;
        file.persist(&destination)
            .map_err(|error| error.error)
            .context("publish owner credential file")?;
        #[cfg(unix)]
        {
            let parent = destination
                .parent()
                .context("private record parent missing")?;
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .context("sync private record directory")?;
            if parent != self.root.as_path() {
                std::fs::File::open(&self.root)
                    .and_then(|directory| directory.sync_all())
                    .context("sync private temporary record directory")?;
            }
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
                "project-origin-execution".into(),
                // Both routes use owner-controlled preparation and activation.
                "deploy-artifact-url".into(),
                "deploy-artifact-id".into(),
                workspace_manifest::STARTUP_PROBE_CAPABILITY.into(),
                shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into(),
            ],
        };
        match std::fs::read(self.identity_path()) {
            Ok(bytes) => {
                use sha2::{Digest, Sha256};
                let name = format!(
                    "identity.history-{}.json",
                    hex::encode(Sha256::digest(&bytes))
                );
                match std::fs::read(self.root.join(&name)) {
                    Ok(retained) => {
                        anyhow::ensure!(retained == bytes, "original identity backup changed")
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        self.store_private_bytes(&name, &bytes)?;
                    }
                    Err(error) => return Err(error).context("read original identity backup"),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("read previous identity before replacement"),
        }
        // identity.json 不回读旧 runtime_instance（旧实例身份不得复用），仅覆盖。
        write_json(&self.identity_path(), &identity)?;
        Ok(identity)
    }

    pub(crate) fn load_desired(&self) -> Result<(DesiredState, u64)> {
        let value: Option<serde_json::Value> = match read_json(&self.desired_path()) {
            Ok(value) => value,
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
                tracing::warn!(%error, "historical desired metadata is damaged; automatic launch suppressed until quiescence recovery");
                return Ok((DesiredState::Stopped, 0));
            }
            Err(error) => return Err(error),
        };
        match value {
            Some(value) => {
                match (
                    serde_json::from_value(value["desired"].clone()),
                    value["revision"].as_u64(),
                ) {
                    (Ok(desired), Some(revision)) => Ok((desired, revision)),
                    _ => {
                        tracing::warn!(
                            "historical desired fields are damaged; automatic launch suppressed until quiescence recovery"
                        );
                        Ok((DesiredState::Stopped, 0))
                    }
                }
            }
            None => Ok(self.legacy_desired.unwrap_or((DesiredState::Running, 0))),
        }
    }

    /// Startup owns the runtime exclusively and has stopped old processes.
    /// Preserve malformed intent for diagnosis and start management in Stopped;
    /// an explicit request can then choose Running without deleting user data.
    pub(crate) fn repair_desired_after_quiescence(&self) -> Result<()> {
        let path = self.desired_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("read desired state during recovery"),
        };
        let decoded = serde_json::from_slice::<serde_json::Value>(&bytes);
        let valid = decoded.as_ref().is_ok_and(|value| {
            serde_json::from_value::<DesiredState>(value["desired"].clone()).is_ok()
                && value["revision"].as_u64().is_some()
        });
        if !valid {
            let saved = self.quarantine_record(&path)?;
            self.store_desired(DesiredState::Stopped, 0)?;
            tracing::warn!(backup = %saved.display(), "repaired invalid desired state; waiting for explicit start");
        }
        Ok(())
    }

    /// 持久化 desired + revision（单文件覆盖写，先 fsync 再可见）。
    pub(crate) fn store_desired(&self, desired: DesiredState, revision: u64) -> Result<()> {
        let mut value: serde_json::Value =
            read_json(&self.desired_path())?.unwrap_or_else(|| serde_json::json!({}));
        let object = value
            .as_object_mut()
            .context("desired state must be an object")?;
        object.insert("desired".into(), serde_json::to_value(desired)?);
        object.insert("revision".into(), revision.into());
        // Inject only the final write in protocol tests. The preceding reads
        // and durable Accepted operation still use the actual filesystem.
        #[cfg(test)]
        if self
            .desired_write_fault
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected desired state write failure",
            ))
            .context("persist desired state");
        }
        write_json(&self.desired_path(), &value)
    }

    /// One physical management wake is consumed atomically with its Stopped
    /// intent. It is bookkeeping, not a lease or an extra proof file. A later
    /// Source/Stop update retains this marker and selects the current intent.
    pub(crate) fn consume_management_boot(&self, boot_id: &str) -> Result<bool> {
        shared_types::validate_identifier(boot_id, "management_boot_id")
            .map_err(anyhow::Error::msg)?;
        let mut value: serde_json::Value =
            read_json(&self.desired_path())?.unwrap_or_else(|| serde_json::json!({}));
        if value
            .get("management_boot_id")
            .and_then(serde_json::Value::as_str)
            == Some(boot_id)
        {
            return Ok(false);
        }
        let (desired, revision) = self.load_desired()?;
        let revision = if desired == DesiredState::Running {
            revision
                .checked_add(1)
                .context("management boot revision overflow")?
        } else {
            revision
        };
        let object = value
            .as_object_mut()
            .context("desired state must be an object")?;
        object.insert(
            "desired".into(),
            serde_json::to_value(DesiredState::Stopped)?,
        );
        object.insert("revision".into(), revision.into());
        object.insert("management_boot_id".into(), boot_id.into());
        write_json(&self.desired_path(), &value)?;
        Ok(true)
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
        // Internal input retains the actual dispatched credentials for audit.
        // Protect the temporary file before writing, then publish atomically.
        // HTTP/SSE expose only operation views and events, never this request.
        let content = serde_json::to_vec_pretty(operation).context("encode operation record")?;
        let name = format!("operations/{}.json", operation.view.operation_id);
        let mut last_error = None;
        for attempt in 0..5u32 {
            match self.store_private_bytes(&name, &content) {
                Ok(()) => return Ok(()),
                Err(error) if is_transient_windows_lock(&error) => {
                    tracing::warn!(attempt, error = %error, "private operation persist transiently locked; retrying");
                    last_error = Some(error);
                    std::thread::sleep(std::time::Duration::from_millis(
                        u64::from(attempt + 1) * 20,
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| anyhow::anyhow!("private operation persist retries exhausted")))
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
    /// RV01：`live` 集内的是本 owner 会话结束后新受理（已派发待消费）或
    /// 待交接收束的操作——派发状态已知，不是未知结果，扫描必须跳过。
    pub(crate) fn recover_unfinished_operations(
        &self,
        live: &std::collections::HashSet<String>,
    ) -> Result<RecoveryScan> {
        let mut scan = RecoveryScan {
            recovered: Vec::new(),
            quarantined: Vec::new(),
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
                    self.report_unreadable_record(&path, error, &mut scan);
                    continue;
                }
            };
            let operation =
                match serde_json::from_str::<StoredOperation>(content.trim_end_matches('\n')) {
                    Ok(operation) => operation,
                    Err(error) => {
                        self.report_unreadable_record(&path, error, &mut scan);
                        continue;
                    }
                };
            let mut operation = operation;
            if live.contains(&operation.view.operation_id) {
                continue;
            }
            if !operation.view.state.is_terminal() {
                // recovery v3：统一 owner 的管理 API 先于首个业务会话开放
                //（R3），Accepted 的 Stop 可能在"已受理、尚未有会话消费"的
                // 窗口被本扫描视为中断。停止已确认不存在的业务幂等成功
                //（spec §2.3）：此时没有任何会话在跑（首个会话尚未启动），
                // 业务必然未由本操作启动——Succeeded 如实。Executing 状态
                // 的 Stop 保守保持 RecoveryRequired（可能中断在物理停中）。
                let id = operation.view.operation_id.clone();
                if operation.view.kind == RuntimeOperationKind::Stop
                    && operation.view.state == RuntimeOperationState::Accepted
                {
                    operation.view.state = RuntimeOperationState::Succeeded;
                    operation.view.error_code = None;
                    operation.view.error_message = None;
                } else {
                    operation.view.state = RuntimeOperationState::RecoveryRequired;
                    operation.view.error_code = Some(ERR_RECOVERY_REQUIRED.into());
                    operation.view.error_message = Some(
                        "process restarted before the operation reached a terminal state".into(),
                    );
                }
                self.store_operation(&operation)?;
                scan.recovered.push(id);
            }
        }
        Ok(scan)
    }

    /// 损坏记录处置：隔离进 `operations-quarantine/`（同状态根 rename，原
    /// 文件保留可查）。隔离失败（存储层故障）才计入 `blocked` 保持保护。
    pub(super) fn report_unreadable_record(
        &self,
        path: &Path,
        error: impl std::fmt::Display,
        scan: &mut RecoveryScan,
    ) {
        match self.quarantine_record(path) {
            Ok(moved) => {
                tracing::error!(
                    "runtime state: unreadable operation record quarantined to {}: {error}",
                    moved.display()
                );
                scan.quarantined.push(moved.display().to_string());
            }
            Err(quarantine_error) => {
                tracing::error!(
                    "runtime state: operation record could not be read or quarantined {}: {error}; {quarantine_error:#}",
                    path.display()
                );
                scan.blocked.push(path.display().to_string());
            }
        }
    }

    /// End an interrupted request without claiming rollback of SQL or directory
    /// changes. Their receipts remain available to the corresponding recovery step.
    pub(super) fn settle_interrupted_operation(
        &self,
        operation: &mut StoredOperation,
    ) -> Result<()> {
        operation.view.state = RuntimeOperationState::Failed;
        operation.view.error_code = Some(ERR_INTERRUPTED_OWNER_EXIT.into());
        operation.view.error_message =
            Some("operation result was not committed before runtime management recovery".into());
        operation.view.failure_detail = Some(RuntimeFailureDetail {
            stage: "startup_reconciled".into(),
            exit_code: None,
            stderr_tail: None,
            cleanup_confirmed: false,
        });
        self.store_operation(operation)
    }

    /// 隔离损坏的操作记录（保留原件，业务不受阻断）。
    pub(super) fn quarantine_record(&self, path: &Path) -> Result<PathBuf> {
        let dir = self.root.join("operations-quarantine");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create quarantine dir {}", dir.display()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("record");
        let target = dir.join(format!(
            "{}-{name}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%3f")
        ));
        std::fs::rename(path, &target)
            .with_context(|| format!("quarantine {} -> {}", path.display(), target.display()))?;
        Ok(target)
    }
}

/// 受理快照 + 对外视图（持久化单位）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredOperation {
    pub view: RuntimeOperationView,
    /// 受理时的规范化请求（重放摘要比对的原始输入）。
    pub request: RuntimeOperationRequest,
}

/// Persisted runtime authority, including credentials and discovery records.
/// A directory or owner.lock alone is only bootstrap, never a second authority.
pub(super) const RUNTIME_STATE_ENTRIES: &[&str] = &[
    "desired.json",
    "identity.json",
    "operations",
    "events",
    "cancellations",
    "token",
    "endpoint.json",
];

fn legacy_desired_base(roots: &[PathBuf]) -> Result<Option<(DesiredState, u64)>> {
    let mut selected = None;
    for root in roots {
        let path = root.join("desired.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("read legacy desired state"),
        };
        let decoded = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| {
                Some((
                    serde_json::from_value::<DesiredState>(value["desired"].clone()).ok()?,
                    value["revision"].as_u64()?,
                ))
            });
        let Some(value) = decoded else {
            tracing::warn!(path = %path.display(), "damaged legacy desired input retained; explicit source selects new intent");
            return Ok(Some((DesiredState::Stopped, 0)));
        };
        if selected.is_some_and(|selected| selected != value) {
            tracing::warn!(
                "conflicting legacy desired inputs retained; waiting for explicit source input"
            );
            return Ok(Some((DesiredState::Stopped, 0)));
        }
        selected = Some(value);
    }
    Ok(selected)
}

fn legacy_belongs_to_project(root: &Path, project: &Path) -> Result<bool> {
    let path = root.join("identity.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).context("read legacy project identity"),
    };
    let value = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => value,
        Err(error) => {
            if !bytes.is_empty() {
                tracing::warn!(path = %path.display(), %error, "damaged legacy identity preserved for actual scope cleanup");
            }
            serde_json::Value::Null
        }
    };
    let requested_app = std::env::var("PROJECT_ID")
        .ok()
        .filter(|value| !value.is_empty());
    let recorded_app = value
        .get("application_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty());
    if matches!((requested_app.as_deref(), recorded_app), (Some(requested), Some(recorded)) if requested != recorded)
    {
        return Ok(false);
    }
    if value
        .get("service_family")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|family| family != "userapp-dev")
    {
        return Ok(false);
    }
    if requested_app
        .as_deref()
        .zip(recorded_app)
        .is_some_and(|(requested, recorded)| requested == recorded)
    {
        return Ok(true);
    }
    if let Some(source) = value
        .get("source_root")
        .and_then(serde_json::Value::as_str)
        .filter(|source| Path::new(source).is_absolute())
    {
        return Ok(runtime_state_layout::canonical_project_root(Path::new(source)) == project);
    }
    match std::fs::read(root.join("supervisor.json")) {
        Ok(bytes) => {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && let Some(binding) = value.pointer("/snapshot/binding")
                && let Ok(binding) =
                    serde_json::from_value::<runtime_supervisor::Binding>(binding.clone())
            {
                return Ok(binding.component == "app-cli"
                    && runtime_state_layout::canonical_project_root(&binding.resource) == project);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("read legacy native project binding"),
    }
    Ok(true)
}

fn legacy_native_binding(
    root: &Path,
    project: &Path,
) -> Result<Option<runtime_supervisor::Binding>> {
    let path = root.join("supervisor.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(runtime_supervisor::Binding {
                component: "app-cli".into(),
                resource: project.to_path_buf(),
            }));
        }
        Err(error) => return Err(error).context("read legacy native binding"),
    };
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(binding) = value.pointer("/snapshot/binding")
        && let Ok(binding) = serde_json::from_value::<runtime_supervisor::Binding>(binding.clone())
    {
        if !binding.component.trim().is_empty() && binding.component != "app-cli" {
            return Ok(None);
        }
        if !binding.component.trim().is_empty() && binding.resource.is_absolute() {
            return Ok(Some(binding));
        }
    }
    Ok(Some(runtime_supervisor::Binding {
        component: "app-cli".into(),
        resource: project.to_path_buf(),
    }))
}
pub(super) fn is_legacy_state_domain(path: &Path) -> Result<bool> {
    for name in RUNTIME_STATE_ENTRIES {
        if root_exists(&path.join(name))? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn root_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("stat runtime state root"),
    }
}

pub(super) fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
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
pub(super) fn write_json_once(temp: &Path, path: &Path, content: &[u8]) -> Result<()> {
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
pub(super) fn is_transient_windows_lock(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(|io_error| io_error.raw_os_error())
            .is_some_and(|code| code == 5 || code == 32)
    })
}

pub(super) fn read_json(path: &Path) -> Result<Option<serde_json::Value>> {
    // Read the actual record: only absence is a not-found observation. A
    // directory or unreadable file must retain its concrete I/O failure instead
    // of silently selecting the default business intent.
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read state {}", path.display()));
        }
    };
    Ok(Some(
        serde_json::from_str(&content).context("decode state record")?,
    ))
}
