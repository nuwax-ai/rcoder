//! Codex ACP 安装模块
//!
//! 从 GitHub releases 自动下载和安装 codex-acp 二进制文件

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

// 使用自己的 fork 版本仓库
pub const CODEX_ACP_REPO: &str = "soddygo/codex-acp";
pub const CODEX_ACP_BINARY_NAME: &str = if cfg!(windows) {
    "codex-acp-agent.exe"
} else {
    "codex-acp-agent"
};

/// GitHub Release 信息
#[derive(Debug, serde::Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub assets: Vec<GitHubAsset>,
}

/// GitHub Asset 信息
#[derive(Debug, serde::Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// 获取安装目录路径
pub fn get_install_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("无法获取用户主目录")?;
    Ok(home.join(".rcoder").join("agents").join("codex-acp-agent"))
}

/// 获取指定版本的二进制文件路径
pub fn get_binary_path(version: &str) -> Result<PathBuf> {
    let install_dir = get_install_dir()?;
    Ok(install_dir.join(version).join(CODEX_ACP_BINARY_NAME))
}

/// 获取最新安装的二进制文件路径（如果存在）,zed 官方的 codex-acp
pub async fn get_installed_binary_path() -> Option<PathBuf> {
    let install_dir = get_install_dir().ok()?;
    
    // 遍历所有版本目录，找到最新的有效二进制文件
    let mut entries = tokio::fs::read_dir(&install_dir).await.ok()?;
    let mut latest_version: Option<(String, PathBuf)> = None;
    
    while let Some(entry) = entries.next_entry().await.ok().flatten() {
        let path = entry.path();
        if path.is_dir() {
            let version = path.file_name()?.to_string_lossy().to_string();
            let binary_path = path.join(CODEX_ACP_BINARY_NAME);
            
            if binary_path.exists() && binary_path.is_file() {
                // 简单版本比较（按字符串排序，适用于 v0.x.x 格式）
                if let Some((latest_ver, _)) = &latest_version {
                    if version > *latest_ver {
                        latest_version = Some((version, binary_path));
                    }
                } else {
                    latest_version = Some((version, binary_path));
                }
            }
        }
    }
    
    latest_version.map(|(_, path)| path)
}

/// 根据当前平台构建 asset 名称
///
/// 参考 Zed 的实现：
/// codex-acp-{version}-{arch}-{platform}.{ext}
fn asset_name(version: &str) -> Option<String> {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return None;
    };

    let platform = if cfg!(target_os = "macos") {
        "apple-darwin"
    } else if cfg!(target_os = "windows") {
        "pc-windows-msvc"
    } else if cfg!(target_os = "linux") {
        "unknown-linux-gnu"
    } else {
        return None;
    };

    // Windows x86_64 使用 .zip，其他使用 .tar.gz
    let ext = if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        "zip"
    } else {
        "tar.gz"
    };

    Some(format!("codex-acp-{version}-{arch}-{platform}.{ext}"))
}

/// 从 GitHub API 获取最新 release 信息
pub async fn fetch_latest_release() -> Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", CODEX_ACP_REPO);
    
    let client_builder = reqwest::Client::builder()
        .user_agent("rcoder-installer/1.0");
    
    let client = client_builder.build()?;
    
    let mut request = client.get(&url);
    
    // 如果设置了 GITHUB_TOKEN 环境变量，使用它来避免限流
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.header("Authorization", format!("Bearer {}", token));
        println!("🔑 使用 GITHUB_TOKEN 进行认证");
    }
    
    let response = request
        .send()
        .await
        .context("请求 GitHub API 失败")?;
    
    let status = response.status();
    if !status.is_success() {
        // 获取响应体以提供更详细的错误信息
        let error_body = response.text().await.unwrap_or_default();
        
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            bail!(
                "GitHub API 限流（403/429）- 请使用以下方法之一:\n\
                 1. 设置 GITHUB_TOKEN 环境变量: export GITHUB_TOKEN=github_pat_xxx\n\
                 2. 等待 1 小时后重试（未认证限制: 60次/小时）\n\
                 3. 手动下载: https://github.com/{}/releases/latest\n\
                 错误详情: {}", 
                CODEX_ACP_REPO, 
                if error_body.len() > 200 { &error_body[..200] } else { &error_body }
            );
        }
        
        bail!("GitHub API 请求失败: {} - {}", status, error_body);
    }
    
    let release: GitHubRelease = response
        .json()
        .await
        .context("解析 GitHub Release 信息失败")?;
    
    Ok(release)
}

/// 下载并安装 codex-acp-agent
pub async fn install_codex_acp(_version: Option<String>) -> Result<PathBuf> {
    println!("🔍 正在查询 codex-acp-agent 最新版本...");
    
    // 获取 release 信息
    let release = fetch_latest_release().await?;
    let version_tag = release.tag_name.clone();
    let version_number = version_tag.trim_start_matches('v');
    
    println!("📦 找到版本: {}", version_tag);
    
    // 检查是否已安装
    let binary_path = get_binary_path(&version_tag)?;
    if binary_path.exists() {
        println!("✅ 版本 {} 已安装: {}", version_tag, binary_path.display());
        return Ok(binary_path);
    }
    
    // 构建 asset 名称
    let asset_name = asset_name(version_number)
        .context("当前平台不支持 codex-acp-agent")?;
    
    println!("🎯 目标文件: {}", asset_name);
    
    // 查找匹配的 asset
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == asset_name)
        .with_context(|| format!("未找到匹配的 release asset: {}", asset_name))?;
    
    println!("⬇️  下载中: {}", asset.browser_download_url);
    
    // 创建安装目录
    let version_dir = get_install_dir()?.join(&version_tag);
    tokio::fs::create_dir_all(&version_dir)
        .await
        .context("创建安装目录失败")?;
    
    // 下载文件
    let client = reqwest::Client::builder()
        .user_agent("rcoder-installer/1.0")
        .build()?;
    
    let response = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("下载失败")?;
    
    if !response.status().is_success() {
        bail!("下载失败: {}", response.status());
    }
    
    let total_size = response.content_length().unwrap_or(0);
    println!("📊 文件大小: {} MB", total_size / 1024 / 1024);
    
    let bytes = response
        .bytes()
        .await
        .context("读取下载内容失败")?;
    
    // 解压文件
    println!("📂 解压中...");
    if asset_name.ends_with(".tar.gz") {
        extract_tar_gz(&bytes, &version_dir).await?;
    } else if asset_name.ends_with(".zip") {
        extract_zip(&bytes, &version_dir).await?;
    } else {
        bail!("不支持的文件格式");
    }
    
    // 验证二进制文件
    if !binary_path.exists() {
        bail!(
            "安装后未找到二进制文件: {}",
            binary_path.display()
        );
    }
    
    // 设置可执行权限（Unix系统）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&binary_path).await?.permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&binary_path, perms).await?;
        println!("🔐 已设置可执行权限");
    }
    
    println!("✅ 安装成功: {}", binary_path.display());
    println!("💡 提示: codex-acp-agent 已安装到 ~/.rcoder/agents/codex-acp-agent/{}/", version_tag);
    
    Ok(binary_path)
}

/// 解压 .tar.gz 文件
async fn extract_tar_gz(bytes: &[u8], target_dir: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;
    use std::io::Cursor;
    
    // 复制 bytes 以获得所有权（spawn_blocking 需要 'static）
    let bytes_owned = bytes.to_vec();
    let target_dir = target_dir.to_owned();
    
    tokio::task::spawn_blocking(move || {
        let cursor = Cursor::new(bytes_owned);
        let decoder = GzDecoder::new(cursor);
        let mut archive = Archive::new(decoder);
        archive.unpack(&target_dir)
            .context("解压 tar.gz 失败")
    })
    .await??;
    
    Ok(())
}

/// 解压 .zip 文件
async fn extract_zip(bytes: &[u8], target_dir: &Path) -> Result<()> {
    use std::io::Cursor;
    use zip::ZipArchive;
    
    // 复制 bytes 以获得所有权（spawn_blocking 需要 'static）
    let bytes_owned = bytes.to_vec();
    let target_dir = target_dir.to_owned();
    
    // 在阻塞任务中执行解压（zip 库是同步的）
    tokio::task::spawn_blocking(move || {
        let cursor = Cursor::new(bytes_owned);
        let mut archive = ZipArchive::new(cursor)
            .context("打开 ZIP 文件失败")?;
        
        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let outpath = target_dir.join(file.name());
            
            if file.is_dir() {
                std::fs::create_dir_all(&outpath)?;
            } else {
                if let Some(p) = outpath.parent()
                    && !p.exists() {
                        std::fs::create_dir_all(p)?;
                    }
                let mut outfile = std::fs::File::create(&outpath)?;
                std::io::copy(&mut file, &mut outfile)?;
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    
    Ok(())
}

/// 卸载 codex-acp
pub async fn uninstall_codex_acp(version: Option<String>) -> Result<()> {
    let install_dir = get_install_dir()?;
    
    if let Some(version) = version {
        // 卸载指定版本
        let version_dir = install_dir.join(&version);
        if version_dir.exists() {
            tokio::fs::remove_dir_all(&version_dir).await?;
            println!("✅ 已卸载版本: {}", version);
        } else {
            println!("⚠️  版本 {} 未安装", version);
        }
    } else {
        // 卸载所有版本
        if install_dir.exists() {
            tokio::fs::remove_dir_all(&install_dir).await?;
            println!("✅ 已卸载所有 codex-acp-agent 版本");
        } else {
            println!("⚠️  未找到已安装的 codex-acp-agent");
        }
    }
    
    Ok(())
}

/// 列出已安装的版本
pub async fn list_installed_versions() -> Result<Vec<String>> {
    let install_dir = get_install_dir()?;
    
    if !install_dir.exists() {
        return Ok(Vec::new());
    }
    
    let mut versions = Vec::new();
    let mut entries = tokio::fs::read_dir(&install_dir).await?;
    
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir()
            && let Some(version) = path.file_name() {
                versions.push(version.to_string_lossy().to_string());
            }
    }
    
    versions.sort();
    Ok(versions)
}

