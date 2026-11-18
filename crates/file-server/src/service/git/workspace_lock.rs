//! 工作区级 git 写互斥: 同一物理工作区的所有服务端 git 写入口
//! (commit/add/branch/tag/init/checkout/reset/...) 经本模块串行化。
//!
//! 仅做进程内串行化; 跨进程 (agent 启动的 git CLI 等) 由 index 事务的
//! index.lock 真锁 (有界退避等待, 超时明确报错) 兜底。锁键是**创建前后稳定
//! 的物理路径** (缺失路径解析最长存在祖先再拼后缀), symlink 别名在初始化
//! 前后都收敛到同一把锁。
//!
//! 每工作区一把 `&'static Mutex`(Box::leak, 数量=工作区数、条目不回收),
//! 使守卫可作 `MutexGuard<'static>` 直接返回, 调用方单行持有。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::error::{AppError, AppResult};

/// 取工作区 git 写锁并返回守卫, 阻塞至可用。
///
/// 路径规范化失败 (工作区在 exists 检查后被删除等) 直接报错——
/// 拿不到确定性锁键时继续执行会绕过串行化保证。
pub(crate) fn acquire(path: &Path) -> AppResult<MutexGuard<'static, ()>> {
    let lock = resolve(path)?;
    // 锁中毒 = 持锁线程 panic; 临界区无自身状态, 直接接管
    Ok(lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}

/// 仅测试用: 不阻塞地探测同锁互斥。
#[cfg(test)]
fn try_acquire(path: &Path) -> AppResult<Option<MutexGuard<'static, ()>>> {
    Ok(resolve(path)?.try_lock().ok())
}

fn resolve(path: &Path) -> AppResult<&'static Mutex<()>> {
    let canonical = stable_lock_key(path)?;
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    // 锁中毒 = 持锁线程 panic; 临界区无自身状态, 直接接管
    let mut map = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lock: &'static Mutex<()> = map
        .entry(canonical)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))));
    drop(map);
    Ok(lock)
}

/// 创建前后稳定的锁键 (codex v4 P2): 整路径存在 → canonicalize (解析沿途+叶子
/// symlink); 缺失 → 解析**最长存在祖先**的物理路径再拼回缺失后缀——父目录经
/// 符号链接时, 创建前后的两种拼写收敛到同一键。stat 的权限/IO 错误响亮失败,
/// 不当"不存在"走词法路径 (仅 NotFound 上溯)。
fn stable_lock_key(path: &Path) -> AppResult<PathBuf> {
    match path.canonicalize() {
        Ok(physical) => return Ok(physical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::system(format!(
                "git workspace canonicalize {}: {error}",
                path.display()
            )));
        }
    }
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name().map(|n| n.to_os_string()) else {
                    // 上溯到根仍不存在: 词法键兜底 (极端挂载形态)
                    return Ok(path.to_path_buf());
                };
                suffix.push(name);
                let Some(parent) = current.parent().map(Path::to_path_buf) else {
                    return Ok(path.to_path_buf());
                };
                current = parent;
            }
            Err(error) => {
                return Err(AppError::system(format!(
                    "git workspace stat {}: {error}",
                    current.display()
                )));
            }
        }
    }
    let mut key = current.canonicalize().map_err(|error| {
        AppError::system(format!(
            "git workspace canonicalize {}: {error}",
            current.display()
        ))
    })?;
    for name in suffix.iter().rev() {
        key.push(name);
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_alias_shares_one_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real-ws");
        std::fs::create_dir_all(&real).expect("mkdir");
        let _guard = acquire(&real).expect("lock real path");
        assert!(
            try_acquire(&real).expect("resolve same path").is_none(),
            "same path must be mutually exclusive"
        );
        #[cfg(unix)]
        {
            let alias = dir.path().join("link-ws");
            std::os::unix::fs::symlink(&real, &alias).expect("symlink");
            assert!(
                try_acquire(&alias).expect("resolve alias").is_none(),
                "symlink alias must share the same physical lock"
            );
        }
    }

    #[test]
    fn missing_workspace_falls_back_to_stable_ancestor_key() {
        // init 流程: 目标未创建时按"最长存在祖先物理路径 + 后缀"取键,
        // 且创建前后 (经父目录 symlink 的两种拼写) 必须收敛到同一把锁。
        let dir = tempfile::tempdir().expect("tempdir");
        let real_parent = dir.path().join("real-parent");
        std::fs::create_dir_all(&real_parent).expect("mkdir real parent");
        let target = real_parent.join("ws");
        #[cfg(unix)]
        {
            let alias_parent = dir.path().join("alias-parent");
            std::os::unix::fs::symlink(&real_parent, &alias_parent).expect("symlink parent");

            // 创建前: 经别名父目录的拼写 (缺失目标 → 祖先解析)
            let alias_ws = alias_parent.join("ws");
            let guard = acquire(&alias_ws).expect("lock via alias before create");
            assert!(
                try_acquire(&target)
                    .expect("resolve real spelling")
                    .is_none(),
                "missing target via real parent must share the lock"
            );
            drop(guard);

            // 创建后再经别名拼写 (整路径存在 → canonicalize) 仍收敛同锁
            std::fs::create_dir_all(&target).expect("create target");
            let _guard = acquire(&target).expect("lock real after create");
            let alias_ws = alias_parent.join("ws");
            assert!(
                try_acquire(&alias_ws)
                    .expect("resolve alias spelling")
                    .is_none(),
                "alias spelling after creation must share the lock"
            );
        }
        #[cfg(not(unix))]
        {
            let guard = acquire(&target).expect("lock missing path");
            drop(guard);
        }
    }
}
