use super::*;

pub struct RuntimeStore {
    pub(super) root: PathBuf,
    // Keep the legacy lock domain fenced while the migrated owner is alive.
    _legacy_owner: Option<crate::platform::owner_guard::OwnerGuard>,
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
        let project = runtime_state_layout::canonical_project_root(workspace);
        let volume = project
            .parent()
            .context("workspace has no volume root for stable runtime state")?;
        let legacy_locations = [
            workspace.join(STATE_DIR_NAME),
            project.join(STATE_DIR_NAME),
            volume.join(STATE_DIR_NAME),
        ];
        let root_identity = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let mut checked = std::collections::BTreeSet::new();
        let mut existing_legacy: Option<PathBuf> = None;
        for legacy in legacy_locations {
            let physical = std::fs::canonicalize(&legacy).unwrap_or_else(|_| legacy.clone());
            if physical == root_identity
                || !checked.insert(physical)
                || !is_legacy_state_domain(&legacy)?
            {
                continue;
            }
            anyhow::ensure!(
                existing_legacy.is_none(),
                "multiple legacy runtime state locations exist; resolve explicitly before starting"
            );
            existing_legacy = Some(legacy);
        }
        let mut legacy_owner = None;
        if let Some(legacy) = existing_legacy {
            // OwnerGuard has normally already created root/owner.lock. Those
            // bootstrap files are not persisted runtime authority. Never ignore
            // existing desired/identity/operations/events/token/endpoint data.
            anyhow::ensure!(
                !is_legacy_state_domain(&root)?,
                "runtime state exists at stable and legacy roots (two authority domains); resolve explicitly before starting"
            );
            let guard = crate::platform::owner_guard::OwnerGuard::acquire(&legacy)
                .context("legacy runtime owner prevents state migration")?;
            std::fs::create_dir_all(&root).context("create migrated runtime root")?;
            // Preflight every destination before the first rename. Move only
            // runtime-owned entries, not sibling registries, journal locks or
            // another project's directory under a legacy bare root.
            let entries: Vec<_> = RUNTIME_STATE_ENTRIES
                .iter()
                .map(|name| (legacy.join(name), root.join(name)))
                .filter_map(|(source, target)| match root_exists(&source) {
                    Ok(true) => Some(Ok((source, target))),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<Result<_>>()?;
            for (_, target) in &entries {
                anyhow::ensure!(
                    !root_exists(target)?,
                    "runtime migration would overwrite existing persisted state"
                );
            }
            for (source, target) in entries {
                std::fs::rename(&source, &target).with_context(|| {
                    format!(
                        "migrate runtime state {} -> {}",
                        source.display(),
                        target.display()
                    )
                })?;
            }
            #[cfg(unix)]
            {
                std::fs::File::open(&root)?.sync_all()?;
                std::fs::File::open(&legacy)?.sync_all()?;
            }
            // Never unlink the old lockfile/directory. Partial moves fail closed
            // next time because both roots contain authority; no guessing or overwrite.
            legacy_owner = Some(guard);
            tracing::info!("runtime state migrated to stable root: {}", root.display());
        }
        std::fs::create_dir_all(root.join("operations")).context("create runtime state dir")?;
        std::fs::create_dir_all(root.join("events")).context("create runtime events dir")?;
        Ok(Self {
            root,
            _legacy_owner: legacy_owner,
        })
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
        std::fs::File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .context("sync private credential directory")?;
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

    /// Startup owns the runtime exclusively and has stopped old processes.
    /// Preserve malformed intent for diagnosis and start management in Stopped;
    /// an explicit request can then choose Running without deleting user data.
    pub(super) fn repair_desired_after_quiescence(&self) -> Result<()> {
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
    if !path.is_file() {
        return Ok(None);
    }
    // A record that vanishes between the is_file check and the read (accepted
    // operation whose file is not yet visible, replaced records) is an
    // observation of absence, not a corrupt store — surface it as not-found so
    // callers keep their retry semantics instead of a hard error.
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
