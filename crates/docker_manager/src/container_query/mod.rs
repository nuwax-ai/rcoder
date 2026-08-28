//! Container query operations（目录化拆分）
//!
//! `impl DockerManager` 查询域按主题分布（extension-impl 模式，对齐 `manager/`
//! 目录先例；方法体原样搬迁，对外路径零变化——固有 impl 方法全局可见）：
//! - [`inspect`]：缓存查询（get_container_info / list_containers）、实时
//!   Docker inspect（find_container_realtime）、模式列表（list_containers_with_pattern）
//! - [`lookup`]：按 project_id / user_id 定位容器（find_project_container /
//!   find_user_container）与 Agent 连接信息（get_agent_info 等）
//! - [`timestamps`]：RFC3339 / Unix 时间戳解析 + `#[ignore]` 本地集成测试

mod inspect;
mod lookup;
mod timestamps;
