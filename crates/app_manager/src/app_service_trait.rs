//! 应用服务 trait 定义

use async_trait::async_trait;

use super::models::*;

/// 应用服务 trait
///
/// 定义应用生命周期管理的统一接口。rcoder 是无状态的应用 pod 引擎：
/// - 写操作（create/start/stop/restart/delete）转调 `ContainerRuntime` 的 Deployment 能力；
/// - 读操作（get/query/list_runtimes）实时查集群，返回 [`AppRuntimeInfo`]（运行时数据），
///   业务元数据由调用方（Java）持久化。
#[async_trait]
pub trait AppServiceTrait: Send + Sync {
    /// 创建应用（返回完整 [`AppInfo`]，rcoder 此时持有请求参数）
    async fn create_app(&self, request: CreateAppRequest) -> AppResult<AppInfo>;

    /// 查询应用列表（实时查集群 + 过滤/分页；仅 status/app_ids 过滤生效，
    /// name/created_at 过滤需要业务元数据，由 Java 侧完成）
    async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> AppResult<PaginatedResponse<AppRuntimeInfo>>;

    /// 对账接口：列出集群中所有 rcoder 托管的应用运行时状态
    ///
    /// 供 Java 在 rcoder/自身重启后对账（rcoder 不持久化 app 元数据）。
    async fn list_app_runtimes(&self) -> AppResult<Vec<AppRuntimeInfo>>;

    /// 获取应用运行时详情（实时查集群）
    async fn get_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 更新应用（全量替换 desired state）。rcoder 无状态，调用方需发送完整新状态
    /// （`image` 必填）。K8s SSA re-apply 幂等；Docker 重建容器。详见 v2 设计 §5.2。
    async fn update_app(
        &self,
        app_id: &str,
        request: UpdateAppRequest,
    ) -> AppResult<AppRuntimeInfo>;

    /// 删除应用（删计算资源；持久存储默认保留，purge=true 才清空数据面。v2 §5.3）
    async fn delete_app(
        &self,
        app_id: &str,
        purge: bool,
        expected_resource_version: Option<&str>,
    ) -> AppResult<()>;

    /// 查询单个应用持久存储状态（v2 §5.4，O(1) stat，不含 size_bytes）
    async fn get_app_storage(&self, app_id: &str) -> AppResult<StorageInfo>;

    /// 清空应用持久存储内容（留 PVC，可恢复；仅当 app 已 delete 时允许，否则 INVALID_STATE）
    async fn clear_app_storage(&self, app_id: &str) -> AppResult<()>;

    /// 销毁应用持久存储 PVC（高危·不可逆·释放配额；需 confirm==app_id，仅 app 已 delete 后允许）
    async fn destroy_app_storage(&self, app_id: &str, confirm: &str) -> AppResult<()>;

    /// 重置 app 容器内 PG 密码（exec psql ALTER USER，本地 trust 认证绕过当前密码）
    async fn reset_db_password(
        &self,
        app_id: &str,
        request: ResetDbPasswordRequest,
    ) -> AppResult<()>;

    /// 新建 PG 库（exec psql CREATE DATABASE）
    async fn create_database(&self, app_id: &str, request: CreateDatabaseRequest) -> AppResult<()>;

    /// 分页查询持久存储（强制分页，无全量模式）
    async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>>;

    /// 启动应用（scale replicas = 1）
    async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 停止应用（scale replicas = 0）
    async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 闲置回收专用 scale0：持久化允许流量唤醒的停止原因。
    async fn recycle_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 重启应用（rollout restart）
    async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 设置闲置回收策略（动态、免重启：只 patch Deployment 注解）。供免费↔付费 tier 变更调用。
    async fn set_recycle_policy(
        &self,
        app_id: &str,
        request: RecyclePolicyRequest,
    ) -> AppResult<AppRuntimeInfo>;

    async fn prepare_release(
        &self,
        app_id: &str,
        request: PrepareReleaseRequest,
    ) -> AppResult<ReleaseInfo>;

    /// 激活发布（单接口：切流 → ensure 运行容器 → 等就绪 → 提交/失败）。
    /// 就绪失败返回 `Ok(ReleaseInfo{status:Failed})` 且**保留现场**（不自动回滚）。
    /// `readiness_timeout` None=默认 300s（handler 层校验 5..=1800）。
    async fn activate_release(
        &self,
        app_id: &str,
        release_id: &str,
        readiness_timeout: Option<u64>,
    ) -> AppResult<ReleaseInfo>;

    /// 回滚到最近一次成功版本（`.rollback` 快照恢复，秒级）。无快照时幂等返回当前
    /// Active；首次发布失败（无旧版本）→ 409。
    async fn rollback_release(
        &self,
        app_id: &str,
        message: Option<String>,
    ) -> AppResult<ReleaseInfo>;

    async fn list_releases(&self, app_id: &str) -> AppResult<ReleaseListResponse>;

    async fn delete_release(&self, app_id: &str, release_id: &str) -> AppResult<()>;

    /// 获取资源使用情况（best-effort：restart_count 来自运行时；CPU/内存需 metrics-server）
    async fn get_app_stats(&self, app_id: &str) -> AppResult<ResourceStats>;

    /// 获取应用事件（K8s Events API：调度/拉取/启动/崩溃）
    async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>>;

    /// 上传文件 / 压缩包（魔数判断：zip/tar.gz → 解压到 target 目录；单文件存 target；flatten 剥 wrapper）
    async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult>;

    /// 从 HTTP(S) URL 下载文件/压缩包并上传；允许内网地址，复用 upload_file 解压和路径安全校验。
    async fn upload_from_url(
        &self,
        app_id: &str,
        url: &str,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult>;

    /// 列出文件（app 根或其子目录 code/data/logs；subpath=None 列 app 根）
    async fn list_files(&self, app_id: &str, subpath: Option<&str>) -> AppResult<Vec<FileInfo>>;

    /// 删除文件（app 根相对路径，可指向 code/data/logs 下任意文件）
    async fn delete_file(&self, app_id: &str, file_path: &str) -> AppResult<()>;
}
