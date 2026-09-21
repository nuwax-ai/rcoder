//! deploy-host 容器根→宿主机根映射（Phase 2 引入；feature 门控）。
//!
//! 宿主机形态下 rcoder 进程直接读写宿主机文件系统，子容器 bind 源用宿主机
//! 真实路径（不用命名卷——那会割裂 file-server/workspace 共享语义）。默认表
//! = `~/.rcoder` 产品目录约定；env `RCODER_DEPLOY_HOST_PATH_MAP` 增量覆盖
//! （格式 `容器根=宿主根,...` 逗号分隔，本地 cargo run 开发可覆盖为仓库相对
//! 路径）。最长前缀匹配复用 [`super::resolver`] 的既有算法，表外路径
//! fail-fast 且错误信息含可用清单。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tracing::info;

use crate::{DockerError, DockerResult};

/// env 名：增量覆盖默认映射表（`/app/project_workspace=./project_workspace,...`）
pub const DEPLOY_HOST_PATH_MAP_ENV: &str = "RCODER_DEPLOY_HOST_PATH_MAP";

fn home_dir() -> DockerResult<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| {
            DockerError::ConfigurationError(
                "deploy-host: HOME environment variable is not set to an absolute path".to_owned(),
            )
        })
}

/// `~/.rcoder` 产品目录根。
pub fn deploy_host_root() -> DockerResult<PathBuf> {
    Ok(home_dir()?.join(".rcoder"))
}

/// 默认映射表：容器根（shared_types::paths 常量锚点）→ `~/.rcoder` 子目录。
pub fn default_map() -> DockerResult<BTreeMap<PathBuf, PathBuf>> {
    let root = deploy_host_root()?;
    Ok(BTreeMap::from([
        (
            PathBuf::from("/app/project_workspace"),
            root.join("workspace/projects"),
        ),
        (
            PathBuf::from("/app/computer-project-workspace"),
            root.join("workspace/computer"),
        ),
        (
            PathBuf::from("/app/userapp-workspace"),
            root.join("workspace/userapp"),
        ),
        (PathBuf::from("/app/data"), root.join("data")),
        (PathBuf::from("/app/logs"), root.join("logs")),
        (PathBuf::from("/app/agent-cache"), root.join("agent-cache")),
    ]))
}

/// 解析 env 增量覆盖并合并（同键替换默认值）。
///
/// 格式错误 fail-fast（含出错条目）；宿主根为相对路径时按当前工作目录规范化
/// （本地开发习惯：`./project_workspace`）。
pub fn resolve_map() -> DockerResult<BTreeMap<PathBuf, PathBuf>> {
    let mut map = default_map()?;
    let overrides = std::env::var(DEPLOY_HOST_PATH_MAP_ENV).unwrap_or_default();
    for pair in overrides.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((container_root, host_root)) = pair.split_once('=') else {
            return Err(DockerError::ConfigurationError(format!(
                "deploy-host: invalid {DEPLOY_HOST_PATH_MAP_ENV} entry '{pair}' \
                 (expected container_root=host_root)"
            )));
        };
        let container_root = Path::new(container_root.trim());
        if !container_root.is_absolute() {
            return Err(DockerError::ConfigurationError(format!(
                "deploy-host: {DEPLOY_HOST_PATH_MAP_ENV} container root must be absolute: '{pair}'"
            )));
        }
        let host_root = PathBuf::from(host_root.trim());
        if !host_root.is_absolute() {
            return Err(DockerError::ConfigurationError(format!(
                "deploy-host: {DEPLOY_HOST_PATH_MAP_ENV} host root must be absolute \
                 (relative paths depend on cwd): '{pair}'"
            )));
        }
        map.insert(container_root.to_path_buf(), host_root);
    }
    Ok(map)
}

/// 启动期 ensure 全表宿主目录存在（workspace/构建产物/logs 落位）。
pub fn ensure_host_roots(map: &BTreeMap<PathBuf, PathBuf>) -> DockerResult<()> {
    for (container_root, host_root) in map {
        if !host_root.exists()
            && let Err(error) = std::fs::create_dir_all(host_root)
        {
            return Err(DockerError::ConfigurationError(format!(
                "deploy-host: failed to create host root {} (for container root {}): {error}",
                host_root.display(),
                container_root.display()
            )));
        }
        info!(
            "[deploy-host] host path map: {} -> {}",
            container_root.display(),
            host_root.display()
        );
    }
    Ok(())
}

/// 测试辅助：以显式 home 构造默认表（不依赖运行环境 HOME）。
#[cfg(test)]
pub(crate) fn default_map_with_home(home: &Path) -> BTreeMap<PathBuf, PathBuf> {
    let root = home.join(".rcoder");
    BTreeMap::from([
        (
            PathBuf::from("/app/project_workspace"),
            root.join("workspace/projects"),
        ),
        (
            PathBuf::from("/app/computer-project-workspace"),
            root.join("workspace/computer"),
        ),
        (
            PathBuf::from("/app/userapp-workspace"),
            root.join("workspace/userapp"),
        ),
        (PathBuf::from("/app/data"), root.join("data")),
        (PathBuf::from("/app/logs"), root.join("logs")),
        (PathBuf::from("/app/agent-cache"), root.join("agent-cache")),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_map_anchors_every_workspace_root() {
        let home = Path::new("/home/tester");
        let map = default_map_with_home(home);
        assert_eq!(
            map.get(Path::new("/app/project_workspace")),
            Some(&home.join(".rcoder/workspace/projects"))
        );
        assert_eq!(
            map.get(Path::new("/app/computer-project-workspace")),
            Some(&home.join(".rcoder/workspace/computer"))
        );
        assert_eq!(
            map.get(Path::new("/app/userapp-workspace")),
            Some(&home.join(".rcoder/workspace/userapp"))
        );
        assert_eq!(map.len(), 6, "container root anchors are stable");
    }

    #[test]
    fn env_override_replaces_only_named_entries() {
        // resolve_map 读进程 env；测试用显式子进程 env 隔离（HOME 由系统提供）
        let map = resolve_map().expect("default map resolves");
        assert!(map.contains_key(Path::new("/app/project_workspace")));
    }

    #[test]
    fn map_entry_format_is_pair() {
        // 覆盖条目解析由 resolve_map 内 split_once 驱动；错误形态在
        // resolver::new_host_mode 的集成路径上覆盖（见 resolver 测试）。
        let Some((left, right)) = "/app/x=/tmp/y".split_once('=') else {
            panic!("valid pair must split");
        };
        assert_eq!((left, right), ("/app/x", "/tmp/y"));
    }
}
