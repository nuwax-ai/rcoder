use super::*;

/// serve 主入口：api 常驻 + 状态机主循环（阻塞至 SIGTERM）。
///
/// `--attach` 标志启用附着模式：已有实例占用端口时核验身份并等待退出，
/// 然后重新执行本二进制成为新 owner。无 `--attach` 时端口冲突立即 fail-fast。
pub async fn serve(args: &RuntimeArgs) -> Result<()> {
    let normalized = args.for_management()?;
    let args = &normalized;
    if args.attach && std::env::var_os(runtime_supervisor::WORKER_ENV).is_none() {
        return attach_to_existing_owner(args).await;
    }
    serve_without_attach(args).await
}

/// 附着模式：核验已有实例身份并等待退出，然后重新执行为 owner。
///
/// 流程：
/// 1. 尝试 TCP connect 管理端口——不可达 = 无实例，直接走正常 serve 流程
/// 2. GET /v1/runtime/identity 核验 application_id + workspace_id
/// 3. 身份匹配 → 等待端口释放（轮询 connect）→ 重新执行本二进制（无 --attach）
/// 4. 初始化中的 owner 有界等待；身份不匹配或其他拒绝响应退出（exit 1）
pub(super) async fn attach_to_existing_owner(args: &RuntimeArgs) -> Result<()> {
    use std::process::exit;

    let mut addr = args.admin_addr.clone();

    // endpoint 发现线索（cross-platform.md §3）：记录存在时优先探测其地址，
    // 连接失败回退 CLI 给定地址——发现记录只是线索，身份核验才是权威。
    let application_id = std::env::var("PROJECT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".to_string());
    if let Ok(state_root) =
        crate::runtime_kernel::RuntimeStore::resolve_root(&args.workspace, &application_id)
        && let Some(record) = crate::runtime_kernel::RuntimeStore::read_endpoint(&state_root)
    {
        tracing::info!(
            "attach mode: endpoint discovery record points to {} (instance {})",
            record.address,
            record.runtime_instance_id
        );
        addr = record.address.clone();
    }

    // 核验已有实例身份。no_proxy：本机 owner 探测不得经系统 HTTP 代理
    // 转发（XP10——代理不劫持本地请求，也不把认证请求带给未知地址）。
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
        .context("build identity probe client")?;

    tracing::info!("attach mode: checking existing instance at {addr}");

    // 带重试的连接检测（新实例启动需要时间完成 API bind；回退换地址时
    // URL 随 addr 重建）
    let mut record_used = addr != args.admin_addr;
    for attempt in 0..15 {
        let url = format!("http://{addr}/v1/runtime/identity");
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                // 解析身份并核验
                let body: serde_json::Value =
                    resp.json().await.context("parse identity response")?;
                return verify_identity_and_attach(args, &body, &addr).await;
            }
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                    let body = resp
                        .json::<serde_json::Value>()
                        .await
                        .context("decode owner initialization response")?;
                    if body.get("code").and_then(serde_json::Value::as_str)
                        == Some("ERR_INITIALIZING")
                    {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
                // 发现记录指向的地址应答异常——可能是旧记录：回退 CLI 地址重试
                if record_used {
                    tracing::warn!(
                        "attach mode: endpoint record target {addr} returned {}; \
                         falling back to {}",
                        status,
                        args.admin_addr
                    );
                    addr = args.admin_addr.clone();
                    record_used = false;
                    continue;
                }
                tracing::error!(
                    "attach mode: existing instance at {addr} returned {} (identity unavailable); \
                     cannot verify ownership, exiting",
                    status
                );
                exit(1);
            }
            Err(_) if attempt < 14 => {
                if std::net::TcpListener::bind(addr.as_str()).is_ok() {
                    // 当前探测地址空闲：发现记录是旧的——回退 CLI 地址再判断
                    if record_used {
                        tracing::warn!(
                            "attach mode: endpoint record target {addr} is stale; \
                             falling back to {}",
                            args.admin_addr
                        );
                        addr = args.admin_addr.clone();
                        record_used = false;
                        continue;
                    }
                    // 端口空闲 = 无运行实例
                    tracing::info!(
                        "attach mode: no existing instance at {addr}, proceeding as owner"
                    );
                    // Re-enter normal launch so an empty attach cannot create
                    // an unsupervised owner alongside the supervised protocol.
                    reexec_without_attach();
                    exit(1);
                }
                // 端口被占但 API 不可达——实例正在启动中，等待
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(e) => {
                if record_used {
                    addr = args.admin_addr.clone();
                    record_used = false;
                    continue;
                }
                tracing::error!("attach mode: failed to connect to {addr}: {e}");
                exit(1);
            }
        }
    }

    tracing::error!("attach mode: could not reach existing instance at {addr} after retries");
    exit(1);
}

/// 核验已有实例身份，匹配则等待并 re-exec，不匹配则退出。
pub(super) async fn verify_identity_and_attach(
    args: &RuntimeArgs,
    body: &serde_json::Value,
    addr: &str,
) -> Result<()> {
    use std::process::exit;

    let identity: shared_types::RuntimeIdentityView = serde_json::from_value(
        body.get("data")
            .cloned()
            .context("attach mode: identity mismatch; refusing to attach")?,
    )
    .context("attach mode: incomplete identity; refusing to attach")?;
    let local_app = std::env::var("PROJECT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".to_string());
    validate_attach_identity(&args.workspace, &local_app, &identity)?;
    tracing::info!(
        application_id = %local_app,
        workspace_id = %identity.workspace_id,
        "attach mode: existing instance matches project identity; waiting for it to exit"
    );

    // 等待端口释放（已有实例退出后端口变为可绑定）
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        if tokio::time::Instant::now() >= deadline {
            tracing::error!("attach mode: timed out waiting for existing instance to exit");
            exit(1);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        if std::net::TcpListener::bind(addr).is_ok() {
            tracing::info!("attach mode: port {addr} released, re-executing as owner");
            break;
        }
    }

    // 重新执行本二进制（无 --attach）成为新 owner
    reexec_without_attach();
    exit(1);
}

/// A workspace leaf name is not an identity: two sibling trees may use it.
pub(super) fn validate_attach_identity(
    workspace: &std::path::Path,
    application_id: &str,
    identity: &shared_types::RuntimeIdentityView,
) -> Result<()> {
    let local = workspace
        .canonicalize()
        .context("resolve attach workspace")?;
    let remote = std::path::Path::new(&identity.source_root)
        .canonicalize()
        .context("resolve attached owner workspace")?;
    anyhow::ensure!(
        identity.application_id == application_id
            && !identity.workspace_id.is_empty()
            && identity.protocol_version == shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION
            && runtime_state_layout::canonical_project_root(&local)
                == runtime_state_layout::canonical_project_root(&remote),
        "attach mode: identity mismatch; refusing to attach"
    );
    Ok(())
}

/// 跨平台重新执行当前二进制（去掉 --attach 参数）。
///
/// Unix：exec 替换当前进程映像（成功不返回）。
/// Windows：spawn 子进程并等待退出（无法原地替换进程映像；supervisord
/// 不在 Windows 上运行，等待语义可接受）。
pub(super) fn reexec_without_attach() {
    let current_exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::error!("attach mode: get current executable path failed: {error}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--attach")
        .collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = std::process::Command::new(current_exe).args(&args).exec();
        tracing::error!("attach mode: re-exec failed: {error}");
        std::process::exit(1);
    }

    #[cfg(not(unix))]
    {
        match std::process::Command::new(current_exe).args(&args).spawn() {
            Ok(mut child) => match child.wait() {
                Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                Err(error) => {
                    tracing::error!("attach mode: wait for replacement failed: {error}");
                    std::process::exit(1);
                }
            },
            Err(error) => {
                tracing::error!("attach mode: spawn failed: {error}");
                std::process::exit(1);
            }
        }
    }
}
