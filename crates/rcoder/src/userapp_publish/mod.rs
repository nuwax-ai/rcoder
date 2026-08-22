//! UserApp 自动化构建发布(rcoder 侧编排)。
//!
//! publish 编排 = 正向调 agent-runner file-server build(`:60000`,订阅进度 SSE)
//! → 同进程调 app_manager(`prepare`/`activate`/`create_app`/`confirm`,零 HTTP)。
//! agent-runner 只负责 build,**不反向调 rcoder**(无 `RCODER_API_BASE`/`FILE_SERVER_BASE_URL`)。
//!
//! 接口(对前端/Java):一键 `publish` + 独立 `build`,均带 agent-runner `project_id`(一个
//! agent-runner 可含多个 UserApp workspace)。
//!
//! 模块划分:`types`(对外契约)← `task`(单任务状态机)← `store`(任务表);
//! `client`(底层 HTTP)← `agent_runner`(build SSE 消费) + `app_lifecycle`(app_manager 生命周期),
//! `orchestrator`(流程编排入口)组合上述两者。对外契约统一从本模块父路径取。

pub mod agent_runner;
mod client; // 仅 userapp_publish 内部消费(handler 不直接触达)
pub mod handler;
pub mod models;
pub mod orchestrator;
pub mod store;
pub mod task;
pub mod types;

// 对外契约统一从父路径取;消费者不再 reach into `::task::` 拿类型。
// (`PublishTask`/`RemoteBuildTask` 只在本模块内经 `super::task::` 直接引用,无需父级再导出。)
pub use store::PublishTaskStore;
pub use types::*;
