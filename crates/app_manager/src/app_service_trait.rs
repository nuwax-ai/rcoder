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
    /// 记录开发注册（userApp create-workspace 时 owner user_id 落库；
    /// name 为空 = 开发期，部署 create_app 后 upsert 补全业务字段）
    async fn record_dev_registration(&self, app_id: &str, user_id: &str) -> AppResult<()>;

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
    /// 按 metadata owner 过滤（无归属记录的应用不返回——分区归属口径）。
    async fn list_app_runtimes(&self, user_id: &str) -> AppResult<Vec<AppRuntimeInfo>>;

    /// 无过滤全量版（系统内部扫描面：闲置回收器覆盖所有 app，与归属无关）。
    async fn list_all_app_runtimes(&self) -> AppResult<Vec<AppRuntimeInfo>>;

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

    /// 查询单个应用持久存储状态（prod=运行卷 / dev=开发卷；O(1) stat，不含 size_bytes）
    async fn get_app_storage(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<StorageInfo>;

    /// 清空应用持久存储内容（prod：留 PVC 可恢复，仅当 app 已 delete；dev：清空
    /// workspace 内容留容器留卷——"重置开发工作区"）
    async fn clear_app_storage(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<()>;

    /// 销毁应用持久存储（prod：删 PVC，需 confirm==app_id 且 app 已 delete；
    /// dev：销毁整个开发环境=builder 容器+dev 卷+目录，不动 metadata）
    async fn destroy_app_storage(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        confirm: &str,
    ) -> AppResult<()>;

    /// PG 凭据对齐（UserApp 运行容器内）：验证传入密码，不一致则重置
    /// （流程单头 `shared_types::align_pg_credentials`，exec 通道实现）
    async fn align_db_credentials(
        &self,
        app_id: &str,
        request: shared_types::AlignCredentialsRequest,
    ) -> AppResult<shared_types::AlignCredentialsOutcome>;

    /// database 目录 SQL 自动执行（发布 activate 后；失败收集进 report 不阻断）
    async fn execute_database_sql(&self, app_id: &str) -> AppResult<DatabaseSqlReport>;

    /// 查 app 的 owner user_id（userapp_metadata；create-workspace/publish 注册）。
    /// 未注册返回 None（调用方自行兜底）。
    async fn get_app_owner(&self, app_id: &str) -> Option<String>;

    /// 分页查询持久存储（强制分页，无全量模式；prod=运行卷清单，dev=开发卷清单）
    async fn query_storage(
        &self,
        app_stage: shared_types::UserappStage,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>>;

    /// 启动应用（scale replicas = 1；内部传统语义，发布链/编排用）
    async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 统一部署+启动（无参数 = 传统 start；带 url = 轻量部署 prepare→activate→启动；
    /// 可选 app_stage/idle/pg 对齐）——REST 面删除 create 后的统一入口
    async fn start_app_enhanced(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult>;

    /// restart 变体（同款可选参数；无 url 时走传统 restart）
    async fn restart_app_enhanced(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult>;

    /// 停止应用（scale replicas = 0）
    async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 闲置回收专用 scale0：持久化允许流量唤醒的停止原因。
    async fn recycle_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 重启应用（rollout restart）
    async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo>;

    /// 设置闲置回收策略（动态、免重启：只 patch Deployment 注解）。供管理/运营面策略调整调用。
    async fn set_recycle_policy(
        &self,
        app_id: &str,
        request: RecyclePolicyRequest,
    ) -> AppResult<AppRuntimeInfo>;

    /// 获取资源使用情况（best-effort：restart_count 来自运行时；CPU/内存需 metrics-server）。
    /// app_stage=prod 查运行容器（label app-id）；app_stage=dev 查开发容器（双键 instance+service-type），
    /// Docker/compose 形态无 metrics → 用量 0（降级语义与 prod 一致）
    async fn get_app_stats(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<ResourceStats>;

    /// 获取应用健康状态：prod=运行时状态派生；dev=探活开发容器内 file-server
    /// `/health`（常驻自愈容器，phase=Running 即达）。不支持 dev 语义的能力接口
    /// （events/recycle-policy 等）由 handler 层拦截，不进 trait。
    async fn get_app_health(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<HealthInfo>;

    /// 日志/管理面转发基址（app-cli :3010）：prod=运行实例 IP；dev=开发容器 host
    /// 解析重拼。logs 三接口的透明转发源
    async fn log_api_base(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<String>;

    /// 获取应用事件（K8s Events API：调度/拉取/启动/崩溃；仅 prod 运行容器——
    /// 调用方 handler 层负责环境校验）
    async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>>;

    /// 上传文件 / 压缩包（魔数判断：zip/tar.gz → 解压到 target 目录；单文件存 target；flatten 剥 wrapper）。
    /// app_stage=prod 转发运行容器（target 相对 /app 根）；app_stage=dev 转发开发容器（target 相对 workspace 根）
    async fn upload_file(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        file_data: Vec<u8>,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult>;

    /// 从 HTTP(S) URL 下载文件/压缩包并上传；允许内网地址，复用 upload_file 解压和路径安全校验。
    async fn upload_from_url(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        url: &str,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult>;

    /// 列出文件（app 根或其子目录 code/data/logs；subpath=None 列 app 根。
    /// dev 环境根基准为开发容器 workspace 根）
    async fn list_files(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        subpath: Option<&str>,
    ) -> AppResult<Vec<FileInfo>>;

    /// 删除文件（app 根相对路径，可指向 code/data/logs 下任意文件）
    async fn delete_file(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        file_path: &str,
    ) -> AppResult<()>;
}
