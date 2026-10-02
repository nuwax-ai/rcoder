use super::*;

#[test]
fn preparation_lease_releases_while_inherited_descriptor_survives() {
    let file = tempfile::NamedTempFile::new().expect("lease file");
    let owner = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file.path())
        .expect("open owner");
    owner.try_lock().expect("first owner");
    let inherited = owner.try_clone().expect("inherited description");
    let next = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file.path())
        .expect("next owner");
    assert!(
        next.try_lock().is_err(),
        "active operation remains exclusive"
    );
    drop(super::PreparationLease(owner));
    next.try_lock()
        .expect("completed operation releases even with inherited description");
    next.unlock().expect("release next");
    drop(inherited);
}

/// 与 workspace-manifest golden fixture 同形状的最小可解析 lock。
const MINIMAL_LOCK: &str = r#"
schema_version = 1
release_id = "test-release-0001"
workspace_name = "demo"
minimum_app_cli_version = "0.1.3"
runtime_image_digest = "registry.example/app-runtime:0.1.140"

[pingap]
mode = "managed"
version = "0.14.3"
commit = "abc123"

[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200

[services.run]
command = ["node", "server.js"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"

[[services.logs]]
id = "application"
glob = "web*.log*"
format = "jsonl"

[services.env]
"#;

#[tokio::test]
async fn preparation_lease_protects_active_staging_and_reclaims_stale_owned_files() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("code");
    let incoming = root.path().join(INCOMING_DIR);
    tokio::fs::create_dir_all(&incoming)
        .await
        .expect("incoming");
    tokio::fs::write(incoming.join("deploy-stale.part"), "stale")
        .await
        .expect("stale");
    tokio::fs::write(incoming.join("unrelated"), "keep")
        .await
        .expect("unrelated");
    let url = serve_once(build_zip(&[("release.lock.toml", MINIMAL_LOCK)])).await;
    let prepared = prepare(&workspace, &url, "prepared", None, None)
        .await
        .expect("prepare")
        .expect("new");
    let staging = prepared.staging.path().to_owned();
    assert!(!incoming.join("deploy-stale.part").exists());
    assert!(incoming.join("unrelated").exists());
    assert!(
        prepare(
            &workspace,
            "http://127.0.0.1:1/unused",
            "contender",
            None,
            None
        )
        .await
        .is_err()
    );
    assert!(
        cleanup_startup(&workspace).await.is_err(),
        "startup sweep must respect the live lease"
    );
    assert!(staging.exists(), "contender must not clean active staging");
    drop(prepared);
    assert!(!staging.exists());
}

#[tokio::test]
async fn preparation_does_not_replace_serving_code_and_drop_cleans_staging() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("code");
    tokio::fs::create_dir_all(&workspace).await.expect("mkdir");
    tokio::fs::write(workspace.join("old.txt"), "old")
        .await
        .expect("old");
    let url = serve_once(build_zip(&[
        ("release.lock.toml", MINIMAL_LOCK),
        ("new.txt", "new"),
    ]))
    .await;
    let prepared = prepare(&workspace, &url, "new-release", None, None)
        .await
        .expect("prepare")
        .expect("prepared");
    assert_eq!(
        tokio::fs::read_to_string(workspace.join("old.txt"))
            .await
            .expect("old"),
        "old"
    );
    assert!(!workspace.join("new.txt").exists());
    let staging = prepared.staging.path().to_path_buf();
    assert!(staging.exists());
    drop(prepared);
    assert!(!staging.exists());
    assert_eq!(
        std::fs::read_dir(incoming_dir(root.path()))
            .expect("incoming")
            .count(),
        0
    );
}

#[tokio::test]
async fn failed_manifest_cleans_staging_and_download() {
    let root = tempfile::tempdir().expect("root");
    let url = serve_once(build_zip(&[("untrusted.txt", "payload")])).await;
    assert!(
        prepare(&root.path().join("code"), &url, "bad", None, None)
            .await
            .is_err()
    );
    for name in [INCOMING_DIR, STAGING_DIR] {
        assert_eq!(
            std::fs::read_dir(root.path().join(name))
                .expect("directory")
                .count(),
            0
        );
    }
}

#[tokio::test]
async fn invalid_startup_contract_preserves_serving_generation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("code");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("release.lock.toml"), MINIMAL_LOCK).unwrap();
    std::fs::write(workspace.join("serving"), "old generation").unwrap();
    // A process strategy on a web service used to be ignored. Reject the
    // candidate while it is still staged, before handing it to activation.
    let invalid = MINIMAL_LOCK.replace(
        "[services.health]",
        "[services.health]\nstartup_probe = \"process\"",
    );
    let url = serve_once(build_zip(&[("release.lock.toml", &invalid)])).await;
    assert!(
        prepare(&workspace, &url, "invalid-probe", None, None)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("release.lock.toml")).unwrap(),
        MINIMAL_LOCK
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("serving")).unwrap(),
        "old generation"
    );
    for name in [INCOMING_DIR, STAGING_DIR] {
        assert_eq!(
            std::fs::read_dir(root.path().join(name)).unwrap().count(),
            0
        );
    }
}

#[test]
fn extraction_preserves_content_without_quota_configuration() {
    let root = tempfile::tempdir().expect("root");
    let archive = root.path().join("input.zip");
    let content = "x".repeat(20000);
    let names: Vec<_> = (0..16).map(|i| format!("entry-{i}")).collect();
    let entries: Vec<_> = names
        .iter()
        .map(|name| (name.as_str(), content.as_str()))
        .collect();
    std::fs::write(&archive, build_zip(&entries)).expect("zip");
    let out = tempfile::tempdir().expect("out");
    extract_zip_sync(&archive, out.path()).expect("extract without quota configuration");
    for name in names {
        assert_eq!(
            std::fs::read_to_string(out.path().join(name)).expect("content"),
            content
        );
    }
}

fn build_zip(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut buf);
    for (name, content) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .expect("start file");
        std::io::Write::write_all(&mut zip, content.as_bytes()).expect("write entry");
    }
    zip.finish().expect("finish zip");
    buf.into_inner()
}

/// 上游先 502（builder 停机窗）后恢复：下载必须在预算内重试成功。
#[allow(unsafe_code)] // edition-2024 测试内 env 变异（项目既有豁免通道）
#[tokio::test]
async fn artifact_download_retries_5xx_within_budget() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let (_dir, workspace) = make_volume();
    let zip_bytes = build_zip(&[
        ("release.lock.toml", MINIMAL_LOCK),
        ("web/server.js", "retry-me"),
    ]);
    let sha = {
        let mut h = Sha256::new();
        h.update(&zip_bytes);
        to_hex(&h.finalize())
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = std::sync::Arc::new(AtomicU32::new(0));
    let body = zip_bytes.clone();
    let hits_server = hits.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let hits_server = hits_server.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 || buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let count = hits_server.fetch_add(1, Ordering::SeqCst);
                let response = if count < 2 {
                    "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                };
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write head");
                if count >= 2 {
                    socket.write_all(&body).await.expect("write body");
                }
            });
        }
    });

    // 5xx then success: deployment must succeed after retries.
    unsafe { std::env::set_var("APP_DEPLOY_UNAVAILABLE_RETRY_SECONDS", "30") };
    deploy(
        &workspace,
        &format!("http://{addr}/artifact.zip"),
        "rel-retry",
        Some(&sha),
    )
    .await
    .expect("deploy after upstream recovery");
    unsafe { std::env::remove_var("APP_DEPLOY_UNAVAILABLE_RETRY_SECONDS") };
    assert!(workspace.join("web/server.js").exists());
    assert!(
        hits.load(Ordering::SeqCst) >= 3,
        "expected retries before success"
    );
}

/// 极简本地 HTTP 服务：单次请求返回 body（Connection: close）。
async fn serve_once(body: Vec<u8>) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4096];
        // 读到请求头结束即可（GET 无 body）
        loop {
            let n = socket.read(&mut buf).await.unwrap_or(0);
            if n == 0 || buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(header.as_bytes())
            .await
            .expect("write head");
        socket.write_all(&body).await.expect("write body");
    });
    format!("http://{addr}/artifact.zip")
}

fn make_volume() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("code");
    std::fs::create_dir_all(&workspace).expect("mkdir code");
    (dir, workspace)
}

#[tokio::test]
async fn fresh_deploy_extracts_and_writes_marker() {
    let (_dir, workspace) = make_volume();
    let zip_bytes = build_zip(&[
        ("release.lock.toml", MINIMAL_LOCK),
        ("web/server.js", "console.log('hi')"),
    ]);
    let url = serve_once(zip_bytes.clone()).await;
    let sha = {
        let mut h = Sha256::new();
        h.update(&zip_bytes);
        to_hex(&h.finalize())
    };

    deploy(&workspace, &url, "rel-001", Some(&sha))
        .await
        .expect("deploy");

    assert!(workspace.join("release.lock.toml").exists());
    assert!(workspace.join("web/server.js").exists());
    let state = read_state(&volume_root_of(&workspace))
        .await
        .expect("state written");
    assert_eq!(state.release_id, "rel-001");
    assert_eq!(state.sha256, sha);
    // .part 已清理
    assert!(
        !volume_root_of(&workspace)
            .join(INCOMING_DIR)
            .join("rel-001.zip.part")
            .exists()
    );
}

/// 注册制品缓存被清理后的显式恢复：激活目录仍声明该制品身份时，本地
/// 制品部署复用激活内容（no-op），不要求重新发布、不猜别的输入。
#[tokio::test]
async fn missing_registered_zip_reuses_activated_release_identity() {
    let (dir, workspace) = make_volume();
    let builds = dir.path().join("builds");
    std::fs::create_dir_all(&builds).expect("builds");
    let zip_path = builds.join("workspace-package-test-release-0001.zip");
    std::fs::write(
        &zip_path,
        build_zip(&[("release.lock.toml", MINIMAL_LOCK), ("web/server.js", "v1")]),
    )
    .expect("registered zip");
    let prepared = prepare_with_local(
        &workspace,
        "artifact://test-release-0001",
        Some(&zip_path),
        "runtime-op-1",
        None,
        None,
    )
    .await
    .expect("first local prepare")
    .expect("fresh preparation");
    activate(&workspace, prepared).await.expect("activate");
    std::fs::remove_file(&zip_path).expect("cache cleaned");

    // 新的显式恢复请求（不同操作标记）在缓存缺失下按激活身份复用。
    let reused = prepare_with_local(
        &workspace,
        "artifact://test-release-0001",
        Some(&zip_path),
        "runtime-op-2",
        None,
        None,
    )
    .await
    .expect("recovery prepare");
    assert!(
        reused.is_none(),
        "activated identity satisfies the deployment"
    );
    assert!(workspace.join("web/server.js").exists());
}

/// 缓存缺失但激活目录声明别的 release、或根本没有激活目录时，部署必须
/// 失败并指向注册 zip 路径——不静默复用身份未知的内容。
#[tokio::test]
async fn missing_registered_zip_with_unknown_identity_still_fails() {
    // 激活目录声明了另一个 release。
    let (dir, workspace) = make_volume();
    let builds = dir.path().join("builds");
    std::fs::create_dir_all(&builds).expect("builds");
    let zip_path = builds.join("workspace-package-rel-other.zip");
    std::fs::write(workspace.join("release.lock.toml"), MINIMAL_LOCK).expect("activated lock");
    let err = prepare_with_local(
        &workspace,
        "artifact://rel-other",
        Some(&zip_path),
        "runtime-op-1",
        None,
        None,
    )
    .await
    .err()
    .expect("identity mismatch must fail");
    assert!(
        format!("{err:#}").contains("registered local artifact is missing"),
        "unexpected error: {err:#}"
    );

    // 没有任何激活目录。
    let (dir2, workspace2) = make_volume();
    let zip2 = dir2
        .path()
        .join("builds")
        .join("workspace-package-rel-gone.zip");
    let err2 = prepare_with_local(
        &workspace2,
        "artifact://rel-gone",
        Some(&zip2),
        "runtime-op-1",
        None,
        None,
    )
    .await
    .err()
    .expect("missing activation must fail");
    assert!(
        format!("{err2:#}").contains("registered local artifact is missing"),
        "unexpected error: {err2:#}"
    );
}

/// 注册 zip 仍在时，即使激活目录声明同一 release，也必须走完整校验/
/// 解压/换代链——内容以注册 zip 为准，不因激活命中跳过重校验。
#[tokio::test]
async fn present_registered_zip_still_reprepares_despite_matching_activation() {
    let (dir, workspace) = make_volume();
    let builds = dir.path().join("builds");
    std::fs::create_dir_all(&builds).expect("builds");
    let zip_path = builds.join("workspace-package-test-release-0001.zip");
    std::fs::write(&zip_path, build_zip(&[("release.lock.toml", MINIMAL_LOCK)]))
        .expect("registered zip");
    let prepared = prepare_with_local(
        &workspace,
        "artifact://test-release-0001",
        Some(&zip_path),
        "runtime-op-1",
        None,
        None,
    )
    .await
    .expect("first local prepare")
    .expect("fresh preparation");
    activate(&workspace, prepared).await.expect("activate");

    let reprepared = prepare_with_local(
        &workspace,
        "artifact://test-release-0001",
        Some(&zip_path),
        "runtime-op-2",
        None,
        None,
    )
    .await
    .expect("zip present must reprepare")
    .expect("full preparation");
    drop(reprepared);
}

#[tokio::test]
async fn marker_hit_skips_download() {
    let (_dir, workspace) = make_volume();
    let zip_bytes = build_zip(&[("release.lock.toml", MINIMAL_LOCK)]);
    let sha = hex::encode(Sha256::digest(&zip_bytes));
    let url = serve_once(zip_bytes).await;
    deploy(&workspace, &url, "rel-001", None)
        .await
        .expect("first deploy");

    // 第二次：URL 指向必然失败的地址（连接拒绝端口），marker 命中应跳过下载
    let bad_url = "http://127.0.0.1:1/nothing.zip";
    deploy(&workspace, bad_url, "rel-001", Some(&sha))
        .await
        .expect("marker skip");
    // code 仍是第一次部署的内容
    assert!(workspace.join("release.lock.toml").exists());
}

#[tokio::test]
async fn sha_mismatch_fails_and_preserves_code() {
    let (_dir, workspace) = make_volume();
    // 预置现场（旧 code）
    std::fs::write(workspace.join("sentinel.txt"), "old").expect("sentinel");

    let zip_bytes = build_zip(&[("release.lock.toml", MINIMAL_LOCK)]);
    let url = serve_once(zip_bytes).await;
    let err = deploy(&workspace, &url, "rel-001", Some(&"0".repeat(64)))
        .await
        .expect_err("mismatch must fail");
    assert!(err.to_string().contains("sha256 mismatch"));
    // 现场 code 未被破坏
    assert_eq!(
        std::fs::read_to_string(workspace.join("sentinel.txt")).unwrap(),
        "old"
    );
}

#[tokio::test]
async fn zip_slip_entry_rejected() {
    let (_dir, workspace) = make_volume();
    // enclosed_name 对 "../evil" 返回 None → 解压即拒
    let zip_bytes = build_zip(&[("../evil.txt", "boom")]);
    let url = serve_once(zip_bytes).await;
    let err = deploy(&workspace, &url, "rel-001", None)
        .await
        .expect_err("zip-slip must fail");
    // anyhow 链式上下文：{:#} 展开整链（to_string 只显最外层 context）
    let chain = format!("{err:#}");
    assert!(
        chain.contains("zip-slip") || chain.contains("escapes"),
        "unexpected error: {chain}"
    );
    assert!(!volume_root_of(&workspace).join("evil.txt").exists());
}

#[tokio::test]
async fn missing_lock_in_package_rejected() {
    let (_dir, workspace) = make_volume();
    let zip_bytes = build_zip(&[("random.txt", "not a platform artifact")]);
    let url = serve_once(zip_bytes).await;
    let err = deploy(&workspace, &url, "rel-001", None)
        .await
        .expect_err("package without lock must fail");
    assert!(err.to_string().contains("release.lock.toml"));
    // lock 闸门失败也清 .part（staged 块统一清理，不留残片）
    assert!(
        !volume_root_of(&workspace)
            .join(INCOMING_DIR)
            .join("rel-001.zip.part")
            .exists()
    );
}

/// 线上事故形态锁：URL 返回 HTTP 200 + JSON 错误信封（网关把 401 包成 200），
/// 下载器无感知落盘——必须在魔数校验层拦截并给出可诊断错误，而非深层的
/// "Could not find EOCD"。
#[tokio::test]
async fn deploy_rejects_non_zip_body() {
    let (_dir, workspace) = make_volume();
    std::fs::write(workspace.join("sentinel.txt"), "old").expect("sentinel");
    let envelope = br#"{"code":"4010","message":"not logged in","success":false}"#;
    let url = serve_once(envelope.to_vec()).await;
    let err = deploy(&workspace, &url, "rel-001", None)
        .await
        .expect_err("error envelope must be rejected");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("not a zip archive"),
        "unexpected error: {chain}"
    );
    // 首字节线索：7b22636f = `{"co`（JSON 信封指纹）
    assert!(chain.contains("7b22636f"), "missing head hex: {chain}");
    // .part 已清 + 现场 code 未破坏
    assert!(
        !volume_root_of(&workspace)
            .join(INCOMING_DIR)
            .join("rel-001.zip.part")
            .exists()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("sentinel.txt")).unwrap(),
        "old"
    );
}

/// 0 字节 body（某些网关错误页）：读不满 4 字节同样归入 not-a-zip 拒绝。
#[tokio::test]
async fn deploy_rejects_empty_body() {
    let (_dir, workspace) = make_volume();
    let url = serve_once(Vec::new()).await;
    let err = deploy(&workspace, &url, "rel-001", None)
        .await
        .expect_err("empty body must be rejected");
    assert!(
        format!("{err:#}").contains("not a zip archive"),
        "unexpected error: {err:#}"
    );
    assert!(
        !volume_root_of(&workspace)
            .join(INCOMING_DIR)
            .join("rel-001.zip.part")
            .exists()
    );
}

/// 启动 sweep：跨重启遗留的 *.part 残片在下一次 deploy 入口被清掉，
/// 非 .part 文件（.staging 中转等）不受影响。
#[tokio::test]
async fn sweep_clears_stale_parts_only() {
    let (_dir, workspace) = make_volume();
    let volume_root = volume_root_of(&workspace);
    let incoming = volume_root.join(INCOMING_DIR);
    std::fs::create_dir_all(&incoming).expect("mkdir incoming");
    std::fs::write(incoming.join("old-release.zip.part"), b"stale").expect("stale part");
    std::fs::write(incoming.join("keep.txt"), b"not a part").expect("keep file");

    sweep_incoming_parts(&incoming).await;

    assert!(!incoming.join("old-release.zip.part").exists());
    assert!(incoming.join("keep.txt").exists());
}

/// XP09（Unix）：激活失败无假成功——清理上一代残留失败时，部署如实
/// 报错，运行中内容原样保留（`.previous` 被占为普通文件 → remove_dir_all
/// ENOTDIR 确定性失败）。
#[cfg(unix)]
#[tokio::test]
async fn xp09_activation_failure_keeps_serving_content() {
    let (_dir, workspace) = make_volume();
    let zip_v1 = build_zip(&[("release.lock.toml", MINIMAL_LOCK), ("v.txt", "1")]);
    let url1 = serve_once(zip_v1).await;
    deploy(&workspace, &url1, "rel-001", None)
        .await
        .expect("v1");

    // 破坏上一代目录：占位为普通文件（清理路径确定性失败）
    let previous = volume_root_of(&workspace).join(PREVIOUS_DIR);
    std::fs::create_dir_all(&previous).expect("previous dir");
    std::fs::remove_dir_all(&previous).expect("clear");
    std::fs::write(&previous, "occupied").expect("occupy as file");

    let lock_v2 = MINIMAL_LOCK.replace("test-release-0001", "test-release-0002");
    let zip_v2 = build_zip(&[("release.lock.toml", &lock_v2), ("v.txt", "2")]);
    let url2 = serve_once(zip_v2).await;
    let second = deploy(&workspace, &url2, "rel-002", None).await;
    assert!(
        second.is_err(),
        "activation failure must surface, not fake success"
    );

    // 运行内容未被触碰（仍是 v1）
    assert_eq!(
        std::fs::read_to_string(workspace.join("v.txt")).unwrap(),
        "1",
        "serving generation must survive a failed activation"
    );
}

/// XP09（Windows）：文件占用阻断激活——workspace 内被独占句柄打开的
/// 文件使目录 rename 失败，部署如实报错，原内容保留。
#[cfg(windows)]
#[tokio::test]
async fn xp09_activation_failure_keeps_serving_content() {
    use std::os::windows::fs::OpenOptionsExt;
    let (_dir, workspace) = make_volume();
    let zip_v1 = build_zip(&[("release.lock.toml", MINIMAL_LOCK), ("v.txt", "1")]);
    let url1 = serve_once(zip_v1).await;
    deploy(&workspace, &url1, "rel-001", None)
        .await
        .expect("v1");

    // 独占句柄（FILE_SHARE_NONE）钉住 workspace 内文件 → 目录 rename 失败
    let _pinned = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(workspace.join("v.txt"))
        .expect("pin running file");

    let lock_v2 = MINIMAL_LOCK.replace("test-release-0001", "test-release-0002");
    let zip_v2 = build_zip(&[("release.lock.toml", &lock_v2), ("v.txt", "2")]);
    let url2 = serve_once(zip_v2).await;
    let second = deploy(&workspace, &url2, "rel-002", None).await;
    assert!(
        second.is_err(),
        "occupied-file activation must surface, not fake success"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("v.txt")).unwrap_or_else(|_| "1".into()),
        "1",
        "serving generation must survive a failed activation"
    );
}

#[tokio::test]
async fn second_deploy_preserves_previous_generation() {
    let (_dir, workspace) = make_volume();
    let zip_v1 = build_zip(&[("release.lock.toml", MINIMAL_LOCK), ("v.txt", "1")]);
    let url1 = serve_once(zip_v1).await;
    deploy(&workspace, &url1, "rel-001", None)
        .await
        .expect("v1");

    let lock_v2 = MINIMAL_LOCK.replace("test-release-0001", "test-release-0002");
    let zip_v2 = build_zip(&[("release.lock.toml", &lock_v2), ("v.txt", "2")]);
    let url2 = serve_once(zip_v2).await;
    deploy(&workspace, &url2, "rel-002", None)
        .await
        .expect("v2");

    assert_eq!(
        std::fs::read_to_string(workspace.join("v.txt")).unwrap(),
        "2"
    );
    let previous = volume_root_of(&workspace).join(PREVIOUS_DIR);
    assert_eq!(
        std::fs::read_to_string(previous.join("v.txt")).unwrap(),
        "1"
    );
}

#[test]
fn release_id_fs_safe_validation() {
    assert!(validate_release_id_fs_safe("rel-abc_1.2").is_ok());
    assert!(validate_release_id_fs_safe("../evil").is_err());
    assert!(validate_release_id_fs_safe(".hidden").is_err());
    assert!(validate_release_id_fs_safe("a/b").is_err());
    assert!(validate_release_id_fs_safe("").is_err());
}
#[cfg(unix)]
#[test]
fn deploy_preserves_internal_dependency_symlink() {
    use std::io::Write;
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("artifact.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).expect("zip"));
    zip.start_file("lib/index.js", zip::write::SimpleFileOptions::default())
        .expect("file");
    zip.write_all(b"module.exports = 42").expect("content");
    zip.add_symlink(
        "node_modules/next",
        "../lib",
        zip::write::SimpleFileOptions::default(),
    )
    .expect("link");
    zip.finish().expect("finish");
    let out = root.path().join("out");
    std::fs::create_dir_all(&out).expect("out");
    extract_zip_sync(&path, &out).expect("extract");
    assert!(out.join("node_modules/next").is_symlink());
    assert_eq!(
        std::fs::read_to_string(out.join("node_modules/next/index.js"))
            .expect("read linked dependency"),
        "module.exports = 42"
    );
}
