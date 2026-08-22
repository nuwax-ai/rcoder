//! 发布域：releases（prepare/activate/rollback 编排）/store（下载校验入库）/runtime（ensure_app）/identity（release lock 构建身份）。

pub(crate) mod identity;
mod releases;
pub(crate) mod runtime;
mod store;
