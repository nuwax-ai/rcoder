//! rcoder 桌面客户端骨架（Phase 6）。
//!
//! 形态 = 完整 rcoder 服务（引擎 + HTTP listener + Pingora，`rcoder::run()`
//! 全量组合）+ gpui-kit 原生窗口。服务跑在自建 tokio runtime 上，UI 主线程
//! 跑 gpui；UI 数据面经 HTTP `/health` 探活呈现引擎状态（后续接
//! `Arc<AppState>` 进程内直读）。结构复刻 gpui-kit examples/ai_recipes。
pub mod bootstrap;
pub mod engine;
