//! VNC 后端解析模块
//!
//! 提供 VNC 后端解析器的具体实现

mod docker_resolver;

pub use docker_resolver::CachedDockerResolver;
