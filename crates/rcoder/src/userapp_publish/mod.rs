//! UserApp 自动化构建发布(rcoder 侧编排)。
//!
//! publish 编排 = 正向调 agent-runner file-server build(`:60000`,订阅进度 SSE)
//! → 同进程调 app_manager(`prepare`/`activate`/`create_app`/`confirm`,零 HTTP)。
//! agent-runner 只负责 build,**不反向调 rcoder**(无 `RCODER_API_BASE`/`FILE_SERVER_BASE_URL`)。
//!
//! 接口(对前端/Java):一键 `publish` + 独立 `build`,均带 agent-runner `project_id`(一个
//! agent-runner 可含多个 UserApp workspace)。

pub mod client;
pub mod handler;
pub mod orchestrator;
pub mod task;

// PublishTaskStore 注入 AppState(router.rs 用);其余类型经 `super::task::*` 在本模块内消费。
pub use task::PublishTaskStore;
