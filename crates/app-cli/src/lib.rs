//! `app-cli`：UserApp 容器运行时编排器。
//!
//! 装在 app-runtime 镜像 `/usr/local/bin/app-cli`，替代 workspace `start.sh`：
//! 读 workspace.manifest.toml + 各子项目 project.manifest.toml 的 `[run].command`，
//! 语言无关地编排所有子项目（按子项目分日志）+ 管理 pingap（类型安全配置 + spawn/托管），
//! 并暴露 HTTP 管理 API。编排逻辑版本化进镜像二进制，升级 = 升镜像，不动已部署用户的包。

pub mod api;
pub mod config;
pub mod manifest;
pub mod pingap_config;
pub mod supervisor;

pub use config::CliArgs;
