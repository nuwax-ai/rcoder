//! Userapp 域 —— 用户应用（无状态 Pod 引擎 + 发布体系）的共享契约
//!
//! - `activity`：活动追踪 + 流量唤醒（`ActivityPersistence` / `AppAccessTracker`，闲置自动回收 / wake-on-traffic）
//! - `metadata`：应用业务元数据（集群不持有的字段，query 的 name/created_at 过滤数据源）
//! - `build_event`：build 进度事件（file-server 发送与 rcoder 接收共享的类型化 DTO）
//! - `dev_cleanup`：开发资源回收契约（app_manager purge 回调宿主回收 UserappBuilder 容器/PVC）
//! - `dev_locator`：开发容器定位契约（app_manager env=dev 分支回调宿主 ensure/定位 UserappBuilder）
//! - `env`：环境维度（dev/prod）路径段统一解析
//! - `db_align`：PG 凭据对齐契约（dev/prod 双环境容器内 PG 密码检查+重置，流程单头）
//! - `forward_contract`：转发分流契约常量（X-Service-Type/X-App-Id，rcoder 与 file-server 共用）
//!
//! 对外统一经 crate 根部 re-export 暴露（如 `shared_types::BuildProgressEvent`），
//! 下游不应依赖 `shared_types::userapp::` 路径。

pub mod activity;
pub mod app_stage;
pub mod build_event;
pub mod db_admin;
pub mod db_align;
pub mod dev_cleanup;
pub mod dev_locator;
pub mod forward_contract;
pub mod metadata;
