//! 容器运行支撑域：params（Deployment 参数装配）/pingora（代理 backend 注册）/db（容器内 PG）/metadata（业务元数据 store）。

mod db;
pub(crate) mod metadata;
pub(crate) mod params;
mod pingora;
