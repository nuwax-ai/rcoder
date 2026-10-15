//! 部署域：runtime（ensure_app/wait_ready 编排）/identity（env 保留变量治理）。
//! 卷上 releases/ 编排（prepare/activate/rollback）已随 RBD 卷形态删除——部署走
//! start {url} 经 env 注入由 app-cli 容器内完成，回滚 = 用旧制品 URL 重新 start。

pub(crate) mod identity;
pub(crate) mod runtime;
