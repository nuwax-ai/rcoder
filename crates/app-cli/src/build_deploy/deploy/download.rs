use super::*;

/// 制品 zip 魔数校验（下载完整落盘后读文件头，非流式截取）。
///
/// 拦截形态：下载 URL 返回 **HTTP 200 + 错误信封 body**（如网关把 401/404 包成
/// 200 + JSON code 信封返回）——下载器在 HTTP 层无感知，错误 body 被当 zip 落盘，
/// 直到解压层才报深层 "Could not find EOCD"（线上事故形态）。此处前置拦截并给
/// 出带首字节线索的可诊断错误。
///
/// 判据：ZIP 规范中含 ≥1 条目的 zip 偏移 0 必是第一个条目的 Local File Header
/// （PK\x03\x04），与生成工具无关；`infer::archive::is_zip` 还额外放行空 zip /
/// 跨卷等边缘形态（平台制品必含 release.lock.toml，空包由 lock 闸门拒绝，跨卷
/// zip crate 本就不支持——宽松方向只会放行"真 zip"，无误伤）。body 不足 4 字节
/// （0 字导体/极短错误页）is_zip 恒 false，天然归入同一错误。
pub(super) async fn verify_zip_magic(part: &Path) -> Result<()> {
    let mut file = tokio::fs::File::open(part)
        .await
        .with_context(|| format!("open downloaded part {}", part.display()))?;
    let mut head = [0u8; 4];
    let n = file
        .read(&mut head)
        .await
        .with_context(|| format!("read head of {}", part.display()))?;
    let head = &head[..n];
    if !infer::archive::is_zip(head) {
        bail!(
            "downloaded artifact is not a zip archive (first bytes: {}) — \
             the url likely returned an error envelope with HTTP 200; \
             verify the deploy url and its auth (release_id in path)",
            head.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
    }
    Ok(())
}

/// 流式下载到文件，返回内容 sha256。无整体超时与容量限制；保留连接和读取空闲超时。
pub(super) async fn download_to_file(url: &str, dest: &Path) -> Result<[u8; 32]> {
    let idle = Duration::from_secs(positive_setting("APP_DEPLOY_READ_IDLE_SECONDS", 60)?);
    let client = reqwest::Client::builder()
        .read_timeout(idle)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("build deploy http client")?;
    // The artifact URL is served through the workspace builder (idle recycling,
    // dev restarts, rollout windows make the upstream briefly unavailable with
    // 5xx or connection errors). Those windows are bounded and the GET is
    // idempotent, so retry within an explicit budget instead of failing the
    // whole deployment. 4xx answers are permanent and never retried.
    let budget = Duration::from_secs(positive_setting(
        "APP_DEPLOY_UNAVAILABLE_RETRY_SECONDS",
        180,
    )?);
    let deadline = tokio::time::Instant::now() + budget;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match download_once(&client, idle, url, dest).await {
            Ok(digest) => return Ok(digest),
            Err(error)
                if retryable_download_failure(&error) && tokio::time::Instant::now() < deadline =>
            {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                tracing::warn!(attempt, %error, remaining = ?remaining,
                    "artifact upstream unavailable; retrying within budget");
                tokio::time::sleep(Duration::from_secs(3).min(remaining)).await;
                if tokio::time::Instant::now() >= deadline {
                    return Err(error.context("artifact upstream retry budget exhausted"));
                }
            }
            Err(error) => {
                if attempt > 1 {
                    tracing::warn!(attempt, %error, "artifact download gave up after retries");
                }
                return Err(error);
            }
        }
    }
}

/// A retryable failure is a transport error, a mid-stream body break, or a 5xx
/// from the artifact upstream (builder idle recycling / dev restart / rollout
/// windows all cut the proxied stream mid-chunk).
fn retryable_download_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if cause.is::<tokio::time::error::Elapsed>() {
            return true;
        }
        if let Some(http) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(status) = http.status() {
                return status.is_server_error();
            }
            // is_body/is_decode: the response already started (200 + chunked)
            // and the connection was cut mid-transfer when the upstream
            // builder went away. The staged file is recreated on retry.
            return http.is_connect() || http.is_request() || http.is_body() || http.is_decode();
        }
        false
    })
}

async fn download_once(
    client: &reqwest::Client,
    idle: Duration,
    url: &str,
    dest: &Path,
) -> Result<[u8; 32]> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))
        .and_then(|r| r.error_for_status().with_context(|| format!("GET {url}")))?;
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    use futures::StreamExt;
    while let Some(chunk) = tokio::time::timeout(idle, stream.next())
        .await
        .context("download read idle timeout")?
    {
        let chunk = chunk.context("download stream")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    Ok(out)
}

fn positive_setting(key: &str, default: u64) -> Result<u64> {
    match std::env::var(key) {
        Ok(value) => {
            let n: u64 = value.parse().with_context(|| format!("invalid {key}"))?;
            if n == 0 {
                bail!("{key} must be positive");
            }
            Ok(n)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(e).with_context(|| format!("read {key}")),
    }
}
