//! 路径空间和日志域判定的测试：真实的目录树加真实的符号链接，不依赖 Docker。
//!
//! 一次测试用的布局模拟一次挂载：`host` 是一个真实的临时目录，挂载表把它也登记成 RCoder
//! 进程里的路径（没有特权做真的 bind mount，进程路径和宿主机路径只能用同一个真实目录）。
//! 符号链接、`..` 都是文件系统上的真实效果。日志根是里面的 `container`。

use super::*;

/// 挂载表只有一条：`host` 同时也是它在 RCoder 进程里的路径
struct Layout {
    root: tempfile::TempDir,
    resolver: HostPathResolver,
}

impl Layout {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("host")).unwrap();
        let host = root.path().join("host");
        let resolver = HostPathResolver::from_mount_table(vec![(host.clone(), host)]).unwrap();
        Self { root, resolver }
    }

    /// 日志根在 RCoder 进程里的路径：和宿主机路径是同一个目录
    fn process_log_root(&self) -> PathBuf {
        self.host().join("container")
    }

    fn host(&self) -> PathBuf {
        self.root.path().join("host")
    }

    fn space(&self) -> HostPathSpace<'_> {
        HostPathSpace::Container(&self.resolver)
    }

    /// 日志根（RCoder 进程里的路径）和它的日志域：挂载表覆盖了它，真实位置也求得出
    async fn domain(&self) -> LogDomain {
        let domain = LogDomain::locate(Some(&self.space()), &self.process_log_root()).await;
        assert_eq!(
            domain.limitation(),
            None,
            "the root is covered by the mount table"
        );
        domain
    }

    async fn verdict(&self, domain: &LogDomain, host_path: &str) -> LogDomainVerdict {
        domain.classify(Some(&self.space()), host_path).await
    }
}

/// 建一个符号链接
fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) {
    let link = link.as_ref();
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target.as_ref(), link).unwrap();
}

#[tokio::test]
async fn only_what_is_strictly_below_the_root_is_inside() {
    let layout = Layout::new();
    let domain = layout.domain().await;
    let host = layout.host().display().to_string();

    assert_eq!(
        layout
            .verdict(&domain, &format!("{host}/container/runner-a"))
            .await,
        LogDomainVerdict::Inside
    );
    assert_eq!(
        layout
            .verdict(&domain, &format!("{host}/container/tenant/runner-a"))
            .await,
        LogDomainVerdict::Inside
    );
    // 日志根本身不算：清理器回收的是它里面的目录
    assert_eq!(
        layout.verdict(&domain, &format!("{host}/container")).await,
        LogDomainVerdict::Outside
    );
    assert_eq!(
        layout
            .verdict(&domain, &format!("{host}/container-other/runner-a"))
            .await,
        LogDomainVerdict::Outside
    );
    assert_eq!(
        layout.verdict(&domain, &format!("{host}/elsewhere")).await,
        LogDomainVerdict::Outside
    );
}

/// Docker 登记挂载源时做词法归一化：先归一化再比，`..` 绕不出去。
#[tokio::test]
async fn a_path_with_dot_dot_is_judged_after_normalization() {
    let layout = Layout::new();
    let domain = layout.domain().await;
    let host = layout.host().display().to_string();

    assert_eq!(
        layout
            .verdict(&domain, &format!("{host}/elsewhere/../container/old"))
            .await,
        LogDomainVerdict::Inside
    );
}

/// 词法上在外面、经符号链接却指进日志根：Docker 挂载那一刻解析的是链接指向的位置，算在里面。
#[tokio::test]
async fn a_symlink_into_the_root_is_inside() {
    let layout = Layout::new();
    symlink("../container", layout.host().join("elsewhere/alias"));
    let domain = layout.domain().await;
    let host = layout.host().display().to_string();

    assert_eq!(
        layout
            .verdict(&domain, &format!("{host}/elsewhere/alias/old"))
            .await,
        LogDomainVerdict::Inside
    );
}

/// 词法上在日志根里、经符号链接指到外面：按词法算在里面。宁可多要求一次新建目录，也不放过。
#[tokio::test]
async fn a_symlink_out_of_the_root_does_not_let_a_path_escape() {
    let layout = Layout::new();
    symlink("../elsewhere", layout.host().join("container/escape"));
    let domain = layout.domain().await;
    let host = layout.host().display().to_string();

    assert_eq!(
        layout
            .verdict(&domain, &format!("{host}/container/escape/old"))
            .await,
        LogDomainVerdict::Inside
    );
}

/// 链接指进 RCoder 看不见的地方：判不出来。调用方必须拒绝，不能当成"不在"。
#[tokio::test]
async fn a_link_into_an_unseen_path_cannot_be_classified() {
    let layout = Layout::new();
    let unseen = tempfile::tempdir().unwrap();
    symlink(unseen.path(), layout.host().join("elsewhere/away"));
    let domain = layout.domain().await;
    let host = layout.host().display().to_string();

    let verdict = layout
        .verdict(&domain, &format!("{host}/elsewhere/away/old"))
        .await;

    assert!(
        matches!(verdict, LogDomainVerdict::Unknown(ref reason) if reason.contains("symbolic link")),
        "{verdict:?}"
    );
}

/// 日志根还没建出来（全新部署）：位置照样定得出来，它下面的路径照样算在里面。
#[tokio::test]
async fn an_absent_root_is_located_where_it_will_be() {
    let layout = Layout::new();
    assert!(!layout.host().join("container").exists());

    let domain = layout.domain().await;

    let host = layout.host().display().to_string();
    assert_eq!(
        layout
            .verdict(&domain, &format!("{host}/container/runner-a"))
            .await,
        LogDomainVerdict::Inside
    );
}

/// 挂载表里没有一条覆盖日志根：换算不出来的只是词法路径。清理器照样能从这个进程路径读到
/// 目录，所以不能当成"没有日志域"放行——什么都判不出来
#[tokio::test]
async fn a_root_no_mount_covers_tells_nothing() {
    let layout = Layout::new();

    let domain =
        LogDomain::locate(Some(&layout.space()), Path::new("/not/mounted/container")).await;

    assert!(matches!(domain, LogDomain::Blind(_)));
    assert!(
        domain
            .limitation()
            .is_some_and(|reason| reason.contains("no mount of RCoder's own container covers")),
        "{domain:?}"
    );
    assert!(matches!(
        layout
            .verdict(&domain, "/not/mounted/container/runner-a")
            .await,
        LogDomainVerdict::Unknown(_)
    ));
}

/// 日志根是相对路径：清理器按工作目录读它，这里对不上。什么都判不出来，不是"没有日志域"
#[tokio::test]
async fn a_relative_root_tells_nothing() {
    let layout = Layout::new();

    let domain = LogDomain::locate(Some(&layout.space()), Path::new("logs/container")).await;

    assert!(
        domain
            .limitation()
            .is_some_and(|reason| reason.contains("not an absolute path"))
    );
    assert!(matches!(
        layout
            .verdict(&domain, "/srv/logs/container/runner-a")
            .await,
        LogDomainVerdict::Unknown(_)
    ));
}

/// 取不到路径空间：什么都判不出来。调用方照常挂载并登记，不能当成"不在"
#[tokio::test]
async fn without_a_path_space_nothing_can_be_told() {
    let domain = LogDomain::locate(None, Path::new("/app/logs/container")).await;

    assert!(
        domain
            .limitation()
            .is_some_and(|reason| reason.contains("mount table is unavailable"))
    );
    assert!(matches!(
        domain.classify(None, "/srv/logs/container/runner-a").await,
        LogDomainVerdict::Unknown(_)
    ));
}

/// 日志根的真实位置求不出来：词法还判得出，词法之外的判不出来。不报错
#[tokio::test]
async fn a_root_whose_real_location_is_unknown_still_judges_lexically() {
    let layout = Layout::new();
    let unseen = tempfile::tempdir().unwrap();
    symlink(unseen.path(), layout.host().join("rootlink"));
    let root = layout.host().join("rootlink").display().to_string();

    let domain = LogDomain::locate(Some(&layout.space()), Path::new(&root)).await;

    assert!(domain.limitation().is_some(), "{domain:?}");
    // 词法上在它下面：在
    assert_eq!(
        layout.verdict(&domain, &format!("{root}/runner-a")).await,
        LogDomainVerdict::Inside
    );
    // 词法上不在：真实位置比不了，判不出来（不是"不在"）
    let elsewhere = layout.host().join("elsewhere").display().to_string();
    assert!(
        matches!(
            layout.verdict(&domain, &elsewhere).await,
            LogDomainVerdict::Unknown(ref reason)
                if reason.contains("real location of the reclaimable log root is unknown")
        ),
        "{:?}",
        layout.verdict(&domain, &elsewhere).await
    );
}

/// 一个还不存在的路径，它的位置是"已有的那一段 + 后面按词法接上的成分"。
#[tokio::test]
async fn the_location_of_an_absent_path_includes_its_unseen_tail() {
    let layout = Layout::new();
    symlink("../container", layout.host().join("elsewhere/alias"));
    let space = layout.space();
    let mut locations = RealLocations::new(&space, &LocalFileSystem);

    let located = locations
        .locate(&layout.host().join("elsewhere/alias/missing/deeper"))
        .await
        .unwrap();

    assert_eq!(
        located,
        Location::Absent {
            path: layout.host().join("container/missing/deeper")
        }
    );
}

#[test]
fn a_bind_source_is_everything_before_the_first_colon() {
    assert_eq!(
        bind_source("/srv/logs/runner-a:/app/container-logs"),
        Some("/srv/logs/runner-a")
    );
    assert_eq!(
        bind_source("/srv/logs/runner-a:/app/container-logs:rw,z"),
        Some("/srv/logs/runner-a")
    );
    // 命名卷、Windows 路径不是宿主机路径
    assert_eq!(bind_source("myvolume:/data"), None);
    assert_eq!(bind_source("C:\\data:/data"), None);
}

/// deploy-host 形态进程路径就是宿主机路径，判定照样成立。
#[tokio::test]
async fn in_the_deploy_host_form_the_process_path_is_the_host_path() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("container")).unwrap();
    symlink("container", root.path().join("alias"));
    let space = HostPathSpace::Host;

    let domain = LogDomain::locate(Some(&space), &root.path().join("container")).await;
    assert_eq!(domain.limitation(), None);
    let alias = root.path().join("alias/runner-a").display().to_string();

    assert_eq!(
        domain.classify(Some(&space), &alias).await,
        LogDomainVerdict::Inside
    );
}
