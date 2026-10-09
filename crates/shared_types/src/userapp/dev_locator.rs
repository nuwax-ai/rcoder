//! Userapp 开发容器定位契约（跨 crate，与 [`super::dev_cleanup`] 同居）。

use async_trait::async_trait;

/// UserappBuilder 开发容器定位（幂等 ensure + 地址解析 + 存在性探测）。
///
/// app_manager 的 runtime 视图（`UserAppRuntime`）经 ISP 分层不含 agent 容器
/// 能力，但文件/存储接口的 `env=dev` 分支需要定位开发容器（转发其 file-server
/// / 判定 dev 卷孤儿）——经此契约回调到宿主（rcoder，持有注册表与全量
/// runtime 视图）执行。
///
/// 实现要求：`dev_file_server_addr` 幂等（容器在则复用，miss 创建注册）；
/// `dev_container_alive` 无副作用（只探测不 ensure——orphan 判定不能有创建
/// 副作用）。
///
/// 定位键 = 复合 identifier `{user_id}-{app_id}`（协作模型多实例）：`user_id`
/// 显式传即该用户的实例，缺失由宿主实现回落 metadata owner。
#[async_trait]
pub trait UserappDevLocator: Send + Sync {
    /// 幂等 ensure 开发容器并返回其 file-server 基址（`http://{host}:60000`）。
    /// 错误返回面向日志/响应的描述串（调用方各自映射错误码）。
    ///
    /// 应用共享：按 app_id 定位唯一 dev 容器（无用户维度）。
    async fn dev_file_server_addr(&self, app_id: &str) -> Result<String, String>;

    /// Locate an already running development container for read-only logs.
    /// Never create or wake compute, refresh activity, or recover its owner.
    /// Unsupported implementations report an error instead of calling ensure.
    async fn dev_logs_file_server_addr(&self, app_id: &str) -> Result<String, String> {
        Err(format!(
            "Read-only development log location is unavailable for app {app_id}"
        ))
    }

    /// 开发容器是否在（dev 卷 orphan 判定用；不 ensure）。`Err` = 探测失败
    /// （调用方保守判非 orphan，与 prod 侧 `is_storage_orphan` 的保守语义对齐）。
    async fn dev_container_alive(&self, app_id: &str) -> Result<bool, String>;
}

/// dev builder 权威实例视图（F1 地址绑定核验用）。
///
/// 来源是运行时权威观测（非内存注册表）：`address` 是当前实例真实地址，
/// 供代理与注册表候选比对或直接使用；`container_id` 为物理身份锚点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevBuilderInstance {
    /// 当前实例 IP（权威运行时观测值；Pending 等无地址场景为空串）。
    pub address: String,
    /// 物理容器 ID（诊断与日志对账锚点）。
    pub container_id: String,
}

/// dev 定位/ensure 的类型化失败（engine 产生，代理按族映射 HTTP——不按字符串猜原因）。
///
/// 语义约定：`ObserveFailed` 表示权威查询失败、不可据以判定存在性，
/// 调用方不得回退端口探测，也不得借 ensure 绕过保护。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevEnsureError {
    /// builder 确认不存在（未创建工作区/已删除）——调用方可走 ensure 创建。
    BuilderAbsent { app_id: String },
    /// 存在在途操作或围栏保护，ensure/接管被拒（detail 含 holder 操作身份）。
    OperationInFlight { app_id: String, detail: String },
    /// 权威运行时查询失败——存在性不可判定，不得当作不存在处理。
    ObserveFailed { app_id: String, detail: String },
    /// builder 存在但尚无可用地址/不在运行态（如 Pending、重启中）。
    NotReady { app_id: String, detail: String },
    /// ensure 执行失败（运行时/存储故障）。
    EnsureFailed { app_id: String, detail: String },
}

impl DevEnsureError {
    pub fn app_id(&self) -> &str {
        match self {
            Self::BuilderAbsent { app_id }
            | Self::OperationInFlight { app_id, .. }
            | Self::ObserveFailed { app_id, .. }
            | Self::NotReady { app_id, .. }
            | Self::EnsureFailed { app_id, .. } => app_id,
        }
    }

    /// 面向日志/诊断的一行描述（不含内部地址与凭据）。
    pub fn brief(&self) -> String {
        match self {
            Self::BuilderAbsent { .. } => "builder absent".into(),
            Self::OperationInFlight { detail, .. } => {
                format!("operation in flight: {detail}")
            }
            Self::ObserveFailed { detail, .. } => format!("observe failed: {detail}"),
            Self::NotReady { detail, .. } => format!("builder not ready: {detail}"),
            Self::EnsureFailed { detail, .. } => format!("ensure failed: {detail}"),
        }
    }
}

/// UserappBuilder 开发容器懒启动回调（rcoder-proxy 终端代理消费）。
///
/// 终端代理（ttyd/vnc/audio/ime/dbx 的 dev 族）是使用语义：开发容器不在时
/// 自动 ensure 创建，而非 404 要求先建工作区。应用共享：按 app_id 定位
/// （URL 的用户占位段不参与定位）。
#[async_trait]
pub trait UserappDevEnsure: Send + Sync {
    /// 权威定位当前 dev builder（只读、无副作用、不刷新活跃）。
    ///
    /// `Ok(None)` = 确认不存在（调用方可走 [`Self::ensure_dev_container`]）；
    /// `Err(ObserveFailed)` = 查询失败不可判定——不得当作不存在，不得以
    /// 端口探测替代身份核验；`Ok(Some)` 的地址为权威当前值，注册表候选
    /// 与其不一致时以本值为准。
    async fn locate_dev_builder(
        &self,
        app_id: &str,
    ) -> Result<Option<DevBuilderInstance>, DevEnsureError>;

    /// ensure（幂等，探活自愈）开发容器并返回容器信息（`container_ip`
    /// 供代理拨流）。失败为类型化 [`DevEnsureError`]，调用方按族映射。
    async fn ensure_dev_container(
        &self,
        app_id: &str,
    ) -> Result<crate::ContainerBasicInfo, DevEnsureError>;
}
