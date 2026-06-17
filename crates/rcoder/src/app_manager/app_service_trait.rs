//! 应用服务 trait 定义

use anyhow::Result;
use async_trait::async_trait;

use super::models::*;

/// 应用服务 trait
///
/// 定义应用生命周期管理的统一接口，支持 Docker 和 K8s 两种实现
#[async_trait]
pub trait AppServiceTrait: Send + Sync {
    /// 创建应用
    async fn create_app(&self, request: CreateAppRequest) -> Result<AppInfo>;

    /// 查询应用列表
    async fn query_apps(&self, request: QueryAppsRequest) -> Result<PaginatedResponse<AppInfo>>;

    /// 获取应用详情
    async fn get_app(&self, app_id: &str) -> Result<AppInfo>;

    /// 更新应用配置
    async fn update_app(&self, app_id: &str, request: UpdateAppRequest) -> Result<AppInfo>;

    /// 删除应用
    async fn delete_app(&self, app_id: &str) -> Result<()>;

    /// 启动应用
    async fn start_app(&self, app_id: &str) -> Result<AppInfo>;

    /// 停止应用
    async fn stop_app(&self, app_id: &str) -> Result<AppInfo>;

    /// 重启应用
    async fn restart_app(&self, app_id: &str) -> Result<AppInfo>;

    /// 获取应用日志
    async fn get_app_logs(&self, app_id: &str, params: LogParams) -> Result<Vec<LogEntry>>;

    /// 获取资源使用情况
    async fn get_app_stats(&self, app_id: &str) -> Result<ResourceStats>;

    /// 获取应用事件
    async fn get_app_events(&self, app_id: &str) -> Result<Vec<String>>;

    /// 上传文件
    async fn upload_file(&self, app_id: &str, file_data: Vec<u8>, target: &str) -> Result<UploadResult>;

    /// 列出文件
    async fn list_files(&self, app_id: &str) -> Result<Vec<FileInfo>>;
}
