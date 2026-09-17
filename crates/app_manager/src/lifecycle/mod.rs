//! 生命周期域：create（创建）/query（查询面）/update（变更面）/ops（启停）/start（统一部署+启动）/status（状态）/workspace（app 目录）。

mod config_input;
mod create;
mod delete;
mod deploy_control;
mod deploy_signals;
mod deploy_wait;
mod hot_deploy;
mod ops;
mod policy;
mod query;
mod recovery;
mod start;
mod status;
mod update;
mod wake;
