//! 多镜像配置（目录化：主体 + overrides 按域分组；对外走 crate 根 re-export）。

mod multi_image_inner;
mod overrides;

pub use multi_image_inner::*;
