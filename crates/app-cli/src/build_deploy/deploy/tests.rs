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

#[test]
fn advisory_pending_and_corrupt_migrations_allow_previous_generation_restore() {
    for bytes in [
        br#"{"identity":"legacy","completed":false}"#.as_slice(),
        b"corrupt-receipt".as_slice(),
    ] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("code");
        let previous = root.path().join(PREVIOUS_DIR);
        std::fs::create_dir(&previous).unwrap();
        std::fs::write(previous.join("release.lock.toml"), MINIMAL_LOCK).unwrap();
        std::fs::write(previous.join("application"), "confirmed previous artifact").unwrap();
        let receipts = root.path().join("migration-receipts");
        std::fs::create_dir(&receipts).unwrap();
        let receipt = receipts.join("legacy.json");
        std::fs::write(&receipt, bytes).unwrap();
        restore_previous_generation(&workspace, "test-release-0001")
            .expect("application migration history must not prevent directory recovery");
        assert!(!previous.exists());
        assert_eq!(
            crate::manifest::read_release_lock(&workspace)
                .unwrap()
                .release_id,
            "test-release-0001"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("application")).unwrap(),
            "confirmed previous artifact"
        );
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        assert!(
            restore_previous_generation(&workspace, "test-release-0001").is_err(),
            "existing destination cannot be replaced"
        );
    }
}

/// 固定旧 zip 8.6.0 writer 制品；旧库不进入生产或开发依赖锁。
/// fixture SHA256: 7dc3bc12fbe9afc34f4239013d58f865a9f33a9c51c71fabeb712ebf22c4a3a3.
#[cfg(unix)]
#[test]
fn deploy_reads_retained_zip8_artifact_with_links_permissions_and_unicode_names() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("legacy.zip");
    std::fs::write(&package, include_bytes!("fixtures/legacy_zip8.zip")).unwrap();
    let output = root.path().join("extracted");
    extract_zip_sync(&package, &output).unwrap();
    assert_eq!(
        std::fs::read_to_string(output.join("web/index.html")).unwrap(),
        "legacy-zip8-content\n"
    );
    assert_eq!(
        std::fs::read_to_string(output.join("web/菜单.txt")).unwrap(),
        "旧制品保留\n"
    );
    let mode = std::fs::metadata(output.join("web/run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(mode & 0o6000, 0);
    let link = output.join("node_modules/entry");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::canonicalize(&link)
            .unwrap()
            .starts_with(std::fs::canonicalize(&output).unwrap())
    );
    assert_eq!(
        std::fs::read_to_string(link.join("index.html")).unwrap(),
        "legacy-zip8-content\n"
    );
}

/// Actual Linux Info-ZIP output passed through PackageWriter's raw-copy path.
/// Producer ownership is opaque ZIP metadata, never an extraction instruction.
#[cfg(unix)]
#[test]
fn deploy_extracts_assembled_infozip_without_applying_owner_or_special_bits() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn producer_owner(extra: &[u8]) -> (u64, u64) {
        let mut position = 0;
        while position + 4 <= extra.len() {
            let id = u16::from_le_bytes([extra[position], extra[position + 1]]);
            let length = u16::from_le_bytes([extra[position + 2], extra[position + 3]]) as usize;
            position += 4;
            assert!(
                position + length <= extra.len(),
                "complete producer metadata"
            );
            if id == 0x7875 {
                let payload = &extra[position..position + length];
                assert_eq!(payload[0], 1, "Info-ZIP Unix UID/GID version");
                let uid_bytes = payload[1] as usize;
                assert!((1..=8).contains(&uid_bytes));
                let gid_bytes = payload[2 + uid_bytes] as usize;
                assert!((1..=8).contains(&gid_bytes));
                assert_eq!(payload.len(), 3 + uid_bytes + gid_bytes);
                let id = |bytes: &[u8]| {
                    bytes.iter().enumerate().fold(0u64, |value, (index, byte)| {
                        value | (u64::from(*byte) << (8 * index))
                    })
                };
                return (
                    id(&payload[2..2 + uid_bytes]),
                    id(&payload[3 + uid_bytes..]),
                );
            }
            position += length;
        }
        panic!("retained producer UID/GID field missing");
    }

    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("assembled.zip");
    std::fs::write(&package, include_bytes!("fixtures/infozip-packed.zip")).unwrap();
    let output = root.path().join("output");
    std::fs::create_dir(&output).unwrap();
    let creator = std::fs::metadata(&output).unwrap();
    let creator_owner = (u64::from(creator.uid()), u64::from(creator.gid()));
    let mut source = zip::ZipArchive::new(std::fs::File::open(&package).unwrap()).unwrap();
    let mut expected = Vec::new();
    let mut distinct_owner = false;
    for (name, expected_owner) in [
        ("infozip/start-one.sh", (12345, 12346)),
        ("infozip/start-two.sh", (54321, 54322)),
    ] {
        let mut entry = source.by_name(name).unwrap();
        assert_eq!((entry.external_attributes() >> 16) & 0o6000, 0o6000);
        assert_eq!(entry.unix_mode().unwrap() & 0o6000, 0);
        let owner = producer_owner(&entry.extra_data().unwrap());
        assert_eq!(owner, expected_owner);
        distinct_owner |= owner != creator_owner;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        expected.push((name, bytes));
    }
    assert!(
        distinct_owner,
        "at least one producer differs from the actual current user"
    );
    extract_zip_sync(&package, &output).unwrap();
    for (name, payload) in expected {
        let path = output.join(name);
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o755);
        assert_eq!(metadata.permissions().mode() & 0o6000, 0);
        assert_eq!(
            (u64::from(metadata.uid()), u64::from(metadata.gid())),
            creator_owner
        );
    }
}

#[test]
fn deploy_extract_rejects_corrupt_crc_in_assembled_infozip() {
    let root = tempfile::tempdir().unwrap();
    let mut bytes = include_bytes!("fixtures/infozip-packed.zip").to_vec();
    let central = bytes
        .windows(4)
        .position(|value| value == b"PK\x01\x02")
        .expect("retained artifact central directory");
    // Preserve valid compression and the opaque producer metadata, but require
    // the ordinary extraction reader to detect a wrong recorded payload CRC.
    bytes[central + 16] ^= 1;
    let archive = root.path().join("corrupt-crc.zip");
    std::fs::write(&archive, bytes).unwrap();
    let error = extract_zip_sync(&archive, &root.path().join("output"))
        .expect_err("opaque producer fields must not bypass payload CRC validation");
    let diagnostic = format!("{error:#}").to_ascii_lowercase();
    assert!(
        diagnostic.contains("checksum") || diagnostic.contains("crc"),
        "{diagnostic}"
    );
}
