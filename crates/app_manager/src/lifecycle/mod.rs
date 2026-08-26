//! 生命周期域：create（创建）/query（查询面）/update（变更面）/ops（启停）/start（统一部署+启动）/status（状态）/workspace（app 目录）。

mod create;
mod hot_deploy;
mod ops;
mod query;
mod start;
mod status;
mod update;
mod workspace;
