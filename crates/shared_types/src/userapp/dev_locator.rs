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
#[async_trait]
pub trait UserappDevLocator: Send + Sync {
    /// 幂等 ensure 开发容器并返回其 file-server 基址（`http://{host}:60000`）。
    /// 错误返回面向日志/响应的描述串（调用方各自映射错误码）。
    ///
    /// `user_id`：请求入参显式携带的 owner（懒创建容器宿主树
    /// `dev/{user_id}/{app_id}` 分区的显式档；`None`/空白走 metadata 注册值）。
    async fn dev_file_server_addr(
        &self,
        app_id: &str,
        user_id: Option<&str>,
    ) -> Result<String, String>;

    /// 开发容器是否在（dev 卷 orphan 判定用；不 ensure）。`Err` = 探测失败
    /// （调用方保守判非 orphan，与 prod 侧 `is_storage_orphan` 的保守语义对齐）。
    async fn dev_container_alive(&self, app_id: &str) -> Result<bool, String>;
}

/// UserappBuilder 开发容器懒启动回调（rcoder-proxy 终端代理消费）。
///
/// 终端代理（ttyd/vnc/audio/ime/dbx 的 dev 族）是使用语义：开发容器不在时
/// 自动 ensure 创建（owner 走 metadata 链——浏览器终端 URL 无入参携带
/// 能力），而非 404 要求先建工作区。
#[async_trait]
pub trait UserappDevEnsure: Send + Sync {
    /// ensure（幂等，探活自愈）开发容器并返回容器信息（`container_ip`
    /// 供代理拨流）。错误返回面向日志的描述串（调用方映射 404/502）。
    async fn ensure_dev_container(
        &self,
        app_id: &str,
        user_id: Option<&str>,
    ) -> Result<crate::ContainerBasicInfo, String>;
}
