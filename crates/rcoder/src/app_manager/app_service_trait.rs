//! 应用服务 trait 定义

use anyhow::Result;
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
    async fn create_app(&self, request: CreateAppRequest) -> Result<AppInfo>;

    /// 查询应用列表（实时查集群 + 过滤/分页；仅 status/app_ids 过滤生效，
    /// name/created_at 过滤需要业务元数据，由 Java 侧完成）
    async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> Result<PaginatedResponse<AppRuntimeInfo>>;

    /// 对账接口：列出集群中所有 rcoder 托管的应用运行时状态
    ///
    /// 供 Java 在 rcoder/自身重启后对账（rcoder 不持久化 app 元数据）。
    async fn list_app_runtimes(&self) -> Result<Vec<AppRuntimeInfo>>;

    /// 获取应用运行时详情（实时查集群）
    async fn get_app(&self, app_id: &str) -> Result<AppRuntimeInfo>;

    /// 更新应用（全量替换 desired state）。rcoder 无状态，调用方需发送完整新状态
    /// （`image` 必填）。K8s SSA re-apply 幂等；Docker 重建容器。详见 v2 设计 §5.2。
    async fn update_app(&self, app_id: &str, request: UpdateAppRequest) -> Result<AppRuntimeInfo>;

    /// 删除应用（删计算资源；持久存储默认保留，purge=true 才清空数据面。v2 §5.3）
    async fn delete_app(&self, app_id: &str, purge: bool) -> Result<()>;

    /// 查询单个应用持久存储状态（v2 §5.4，O(1) stat，不含 size_bytes）
    async fn get_app_storage(&self, app_id: &str) -> Result<StorageInfo>;

    /// 清空应用持久存储（仅当 app 已 delete 时允许，否则 INVALID_STATE）
    async fn delete_app_storage(&self, app_id: &str) -> Result<()>;

    /// 分页查询持久存储（强制分页，无全量模式）
    async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> Result<PaginatedResponse<StorageInfo>>;

    /// 启动应用（scale replicas = 1）
    async fn start_app(&self, app_id: &str) -> Result<AppRuntimeInfo>;

    /// 停止应用（scale replicas = 0）
    async fn stop_app(&self, app_id: &str) -> Result<AppRuntimeInfo>;

    /// 重启应用（rollout restart）
    async fn restart_app(&self, app_id: &str) -> Result<AppRuntimeInfo>;

    /// 获取应用日志（读取共享工作空间的 logs/app.log）
    async fn get_app_logs(&self, app_id: &str, params: LogParams) -> Result<Vec<LogEntry>>;

    /// 启动日志流（follow），返回 mpsc::Receiver 供 WS handler 桥接（v2 §11）
    async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> Result<container_runtime_api::mpsc::Receiver<container_runtime_api::ContainerLogEntry>>;

    /// 获取资源使用情况（best-effort：restart_count 来自运行时；CPU/内存需 metrics-server）
    async fn get_app_stats(&self, app_id: &str) -> Result<ResourceStats>;

    /// 获取应用事件（best-effort：当前返回空，TODO 接 K8s events）
    async fn get_app_events(&self, app_id: &str) -> Result<Vec<String>>;

    /// 上传文件（写入共享工作空间 code 目录）
    async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
    ) -> Result<UploadResult>;

    /// 列出文件（app 根或其子目录 code/data/logs；subpath=None 列 app 根）
    async fn list_files(&self, app_id: &str, subpath: Option<&str>) -> Result<Vec<FileInfo>>;

    /// 删除文件（app 根相对路径，可指向 code/data/logs 下任意文件）
    async fn delete_file(&self, app_id: &str, file_path: &str) -> Result<()>;
}
