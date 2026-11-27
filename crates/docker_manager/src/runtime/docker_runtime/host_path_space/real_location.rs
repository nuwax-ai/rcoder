//! 一个宿主机路径的真实位置：以宿主机路径为准，一个成分一个成分往下走，路径上的符号链接
//! 经 RCoder 自己的挂载去解析。
//!
//! Docker 登记挂载源用的是词法路径；路径上的符号链接由内核在宿主机上、挂载那一刻解析。
//! 所以要知道一个挂载源到底指向哪里，必须把链接也走一遍。每走一步经 RCoder 自己的挂载看
//! 这一级是什么（[`look`]），是链接就顺着它指的地方接着走：相对目标从链接所在的目录算起，
//! 绝对目标是宿主机上的路径。只采纳能核验的视图——视图里的路径换算回宿主机必须还是这个
//! 路径，被另一条挂载遮蔽的不算。
//!
//! 走完之后（[`Location`]）分四种：
//!
//! - [`Location::Present`]：停在一个存在的对象上；
//! - [`Location::Absent`]：路径上有一级不存在，`path` 是它此后的位置——已经走过的成分照旧，
//!   后面还没看的成分按词法接上；
//! - [`Location::Outside`]：从头到尾没有读到过链接，整条路径都在 RCoder 的挂载之外；
//! - [`Location::Unconfirmed`]：读到过链接、之后却走进 RCoder 看不见的路径（看不见的那一段里
//!   可能还有链接绕回来），被另一条挂载遮蔽，`..` 回退到 RCoder 看不见的那一级，读取出错，
//!   链接层数过多：无法确认。

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;

use super::{HostPathSpace, Viewed, viewing};

/// 一条路径上最多跟随多少个符号链接，与内核的上限相同
const MAX_SYMLINKS: usize = 40;

/// 一个路径本身是什么，不跟随符号链接
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Entry {
    Directory,
    /// 普通文件，或其它既不是目录也不是链接的对象
    Other,
    Link(PathBuf),
    /// 不存在；路径中间有一级不是目录时也是它
    Missing,
}

/// 读取文件系统的来源
///
/// 生产环境读真实文件系统。做成可替换，是为了在没有特权做 bind mount 的测试里模拟"同一个
/// 宿主机目录挂进 RCoder 两次"。
#[async_trait]
pub(crate) trait FileSystemView: Send + Sync {
    /// 这个路径本身是什么，不跟随符号链接
    async fn entry(&self, path: &Path) -> std::io::Result<Entry>;
    /// 跟随符号链接后这个路径所指目录的身份；不是目录等错误原样返回
    async fn identity(&self, path: &Path) -> std::io::Result<DirectoryId>;
}

/// 目录的身份：设备号加 inode。同一个目录经不同名字、不同挂载点出现时身份相同
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct DirectoryId(u64, u64);

pub(crate) struct LocalFileSystem;

#[async_trait]
impl FileSystemView for LocalFileSystem {
    async fn entry(&self, path: &Path) -> std::io::Result<Entry> {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Err(error) if is_absent(&error) => return Ok(Entry::Missing),
            inspected => inspected?,
        };
        if metadata.file_type().is_symlink() {
            return tokio::fs::read_link(path).await.map(Entry::Link);
        }
        Ok(if metadata.is_dir() {
            Entry::Directory
        } else {
            Entry::Other
        })
    }

    async fn identity(&self, path: &Path) -> std::io::Result<DirectoryId> {
        let metadata = tokio::fs::metadata(path).await?;
        if !metadata.is_dir() {
            return Err(std::io::Error::from(ErrorKind::NotADirectory));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(DirectoryId(metadata.dev(), metadata.ino()))
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Err(std::io::Error::new(
                ErrorKind::Unsupported,
                "directory identity is only available on Unix",
            ))
        }
    }
}

/// 路径不存在，或者路径中间有一级不是目录
pub(crate) fn is_absent(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// 经 RCoder 的挂载看到的一个宿主机路径
#[derive(Clone, Debug, Eq, PartialEq)]
enum Sight {
    Seen(Entry),
    Unmounted,
    Hidden,
}

/// 看一个宿主机路径是什么；同一轮里同一个路径只看一次
async fn look(
    space: &HostPathSpace<'_>,
    fs: &dyn FileSystemView,
    sights: &mut HashMap<PathBuf, Sight>,
    host_path: &Path,
) -> std::io::Result<Sight> {
    if let Some(sight) = sights.get(host_path) {
        return Ok(sight.clone());
    }
    let sight = match viewing(space, host_path) {
        Viewed::Through(projection) => Sight::Seen(fs.entry(&projection.path).await?),
        Viewed::Unmounted => Sight::Unmounted,
        Viewed::Hidden => Sight::Hidden,
    };
    sights.insert(host_path.to_path_buf(), sight.clone());
    Ok(sight)
}

/// 一个宿主机路径在宿主机上的位置
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Location {
    /// 解析到了一个存在的对象
    Present { path: PathBuf, directory: bool },
    /// 路径上有一级不存在；`path` 是它此后的位置
    Absent { path: PathBuf },
    /// 整条路径都在 RCoder 的挂载之外
    Outside,
    /// 从 RCoder 这里无法确认它指向哪里
    Unconfirmed(String),
}

/// 一条路径里要逐级走过去的成分：去掉根和盘符，保留 `.` 和 `..`
fn components(path: &Path) -> impl Iterator<Item = OsString> {
    path.components()
        .filter(|component| !matches!(component, Component::RootDir | Component::Prefix(_)))
        .map(|component| component.as_os_str().to_os_string())
}

/// 从根开始把走过的成分接成绝对路径
fn from_root(resolved: &[OsString]) -> PathBuf {
    let mut path = PathBuf::from("/");
    for component in resolved {
        path.push(component);
    }
    path
}

/// 一轮判定里共用的观察结果：经 RCoder 的视图看一个宿主机路径是什么，同一个路径只看一次
pub(crate) struct RealLocations<'a> {
    space: &'a HostPathSpace<'a>,
    fs: &'a dyn FileSystemView,
    sights: HashMap<PathBuf, Sight>,
}

impl<'a> RealLocations<'a> {
    pub(crate) fn new(space: &'a HostPathSpace<'a>, fs: &'a dyn FileSystemView) -> Self {
        Self {
            space,
            fs,
            sights: HashMap::new(),
        }
    }

    /// 一个宿主机路径的真实位置（宿主机路径）。链接解析不了时说明原因
    pub(crate) async fn real_location(&mut self, host_path: &Path) -> Result<PathBuf, String> {
        match self.locate(host_path).await {
            Ok(Location::Present { path, .. }) | Ok(Location::Absent { path }) => Ok(path),
            // RCoder 看不见、也没读到过链接：只能按词法路径算
            Ok(Location::Outside) => Ok(crate::normalize_path(host_path)),
            Ok(Location::Unconfirmed(reason)) => Err(reason),
            Err(error) => Err(error.to_string()),
        }
    }

    /// 以宿主机路径为准，一个成分一个成分往下走，求它的位置；做法见模块说明
    pub(crate) async fn locate(&mut self, source: &Path) -> std::io::Result<Location> {
        locate(self.space, self.fs, &mut self.sights, source).await
    }
}

/// 以宿主机路径为准，一个成分一个成分往下走，求它的真实位置；做法见模块说明
async fn locate(
    space: &HostPathSpace<'_>,
    fs: &dyn FileSystemView,
    sights: &mut HashMap<PathBuf, Sight>,
    source: &Path,
) -> std::io::Result<Location> {
    let unconfirmed = |detail: String| -> std::io::Result<Location> {
        Ok(Location::Unconfirmed(format!(
            "mount source {}: {detail}",
            source.display()
        )))
    };
    let mut resolved: Vec<OsString> = Vec::new();
    // `resolved` 里每一级是否经视图核验过
    let mut confirmed: Vec<bool> = Vec::new();
    let mut last = Entry::Missing;
    let mut queue: VecDeque<OsString> = components(source).collect();
    let mut links = 0usize;
    let mut followed_link = false;
    while let Some(component) = queue.pop_front() {
        if component == ".." {
            match confirmed.pop() {
                Some(true) => {
                    resolved.pop();
                }
                Some(false) => {
                    return unconfirmed(
                        "its `..` climbs above the mounts RCoder can verify, so where it leads is unknown"
                            .to_owned(),
                    );
                }
                None => {}
            }
            continue;
        }
        if component == "." {
            continue;
        }
        resolved.push(component);
        let path = from_root(&resolved);
        match look(space, fs, sights, &path).await? {
            Sight::Seen(entry @ (Entry::Directory | Entry::Other)) => {
                confirmed.push(true);
                last = entry;
            }
            Sight::Seen(Entry::Link(target)) => {
                links += 1;
                followed_link = true;
                if links > MAX_SYMLINKS {
                    return unconfirmed(format!(
                        "it passes through more than {MAX_SYMLINKS} symbolic links"
                    ));
                }
                // 链接自己被它的目标取代：相对目标从链接所在的目录接着走
                resolved.pop();
                let mut target_components = components(&target).peekable();
                if target.is_absolute() {
                    // Keep the already traversed common host prefix. Starting
                    // from scratch would treat even the shared, invisible
                    // parents of the configured mount as a visibility gap.
                    let mut common = 0;
                    for parent in &resolved {
                        if target_components.next_if_eq(parent).is_none() {
                            break;
                        }
                        common += 1;
                    }
                    resolved.truncate(common);
                    confirmed.truncate(common);
                }
                queue = target_components.chain(queue).collect();
            }
            Sight::Seen(Entry::Missing) => {
                // 这一级不存在：它此后的位置是"走过的成分 + 还没看的成分"，后者不可能再有链接
                let mut path = from_root(&resolved);
                for component in &queue {
                    path.push(component);
                }
                return Ok(Location::Absent {
                    path: crate::normalize_path(&path),
                });
            }
            Sight::Unmounted if followed_link => {
                return unconfirmed(
                    "it leads through a symbolic link into a path RCoder cannot verify, so where it ends is unknown"
                        .to_owned(),
                );
            }
            Sight::Unmounted => confirmed.push(false),
            Sight::Hidden => {
                return unconfirmed(
                    "it is hidden behind another mount, so it cannot be identified from inside RCoder"
                        .to_owned(),
                );
            }
        }
    }
    if followed_link && confirmed.last() != Some(&true) {
        // 读到过链接之后走进了看不见的路径：那一段里有没有链接绕回来，无从知道
        return unconfirmed(
            "it leads through a symbolic link into a path RCoder cannot verify, so where it ends is unknown"
                .to_owned(),
        );
    }
    Ok(if confirmed.last() == Some(&true) {
        Location::Present {
            path: from_root(&resolved),
            directory: last == Entry::Directory,
        }
    } else {
        Location::Outside
    })
}
