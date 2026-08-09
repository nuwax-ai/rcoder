//! 安全、有界的 skill URL 下载器。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::LOCATION;
use reqwest::{Client, StatusCode, Url};

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::service::temp_file::{TemporaryFile, TemporaryFileWriter};

pub struct SkillDownloader {
    client: Client,
    temp_dir: PathBuf,
    max_bytes: u64,
    max_redirects: usize,
    max_url_count: usize,
    allow_http: bool,
    allow_private_networks: bool,
    allowed_hosts: Vec<String>,
}

impl SkillDownloader {
    pub fn new(config: &Config) -> AppResult<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(SafeResolver {
                allow_private_networks: config.skill_url_allow_private_networks,
            }))
            .connect_timeout(Duration::from_secs(
                config.skill_download_connect_timeout_secs,
            ))
            .timeout(Duration::from_secs(config.skill_download_timeout_secs))
            .build()
            .map_err(|error| AppError::system(format!("build skill HTTP client: {error}")))?;
        Ok(Self {
            client,
            temp_dir: config.upload_project_dir.join("temp"),
            max_bytes: config.skill_download_max_bytes,
            max_redirects: config.skill_download_max_redirects,
            max_url_count: config.skill_url_max_count,
            allow_http: config.skill_url_allow_http,
            allow_private_networks: config.skill_url_allow_private_networks,
            allowed_hosts: config
                .skill_url_allowed_hosts
                .iter()
                .map(|host| host.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
        })
    }

    pub fn validate_url_count(&self, count: usize) -> AppResult<()> {
        if count > self.max_url_count {
            return Err(AppError::validation(format!(
                "too many skill URLs (max {})",
                self.max_url_count
            )));
        }
        Ok(())
    }

    pub async fn download(&self, raw_url: &str) -> AppResult<TemporaryFile> {
        let mut url = Url::parse(raw_url)
            .map_err(|error| AppError::validation(format!("invalid skill URL: {error}")))?;
        for redirect_count in 0..=self.max_redirects {
            self.validate_target(&url)?;
            let response =
                self.client.get(url.clone()).send().await.map_err(|error| {
                    AppError::network(format!("fetch skill URL failed: {error}"))
                })?;

            if response.status().is_redirection() {
                if redirect_count == self.max_redirects {
                    return Err(AppError::network("skill URL redirect limit exceeded"));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| AppError::network("skill URL redirect has no Location"))?
                    .to_str()
                    .map_err(|error| {
                        AppError::network(format!("invalid redirect Location: {error}"))
                    })?;
                url = url
                    .join(location)
                    .map_err(|error| AppError::network(format!("invalid redirect URL: {error}")))?;
                continue;
            }
            if response.status() != StatusCode::OK {
                return Err(AppError::network(format!(
                    "fetch {url} returned status {}",
                    response.status()
                )));
            }
            if response
                .content_length()
                .is_some_and(|length| length > self.max_bytes)
            {
                return Err(AppError::validation(format!(
                    "skill download exceeds limit (max {} bytes)",
                    self.max_bytes
                )));
            }

            let mut writer =
                TemporaryFileWriter::create(&self.temp_dir, "skill-download-", self.max_bytes)
                    .await?;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| {
                    AppError::network(format!("read skill URL response failed: {error}"))
                })?;
                writer.write(&chunk).await?;
            }
            return writer.finish().await;
        }
        Err(AppError::network("skill URL redirect limit exceeded"))
    }

    fn validate_target(&self, url: &Url) -> AppResult<()> {
        match url.scheme() {
            "https" => {}
            "http" if self.allow_http => {}
            _ => return Err(AppError::validation("skill URL must use HTTPS")),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(AppError::validation(
                "skill URL must not contain credentials",
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| AppError::validation("skill URL host is required"))?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if !self.allowed_hosts.is_empty()
            && !self
                .allowed_hosts
                .iter()
                .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
        {
            return Err(AppError::permission(format!(
                "skill URL host is not allowed: {host}"
            )));
        }
        if !self.allow_private_networks
            && let Ok(ip) = host.parse::<IpAddr>()
            && !is_public_ip(ip)
        {
            return Err(AppError::permission(format!(
                "skill URL uses a private or reserved address: {ip}"
            )));
        }
        Ok(())
    }
}

/// 在 reqwest 实际连接时完成 DNS 解析和地址校验，防止 DNS rebinding。
struct SafeResolver {
    allow_private_networks: bool,
}

impl reqwest::dns::Resolve for SafeResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let allow_private_networks = self.allow_private_networks;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(boxed_io_error)?
                .collect::<Vec<_>>();
            if addresses.is_empty() {
                return Err(boxed_message(format!(
                    "skill URL host resolved to no address: {host}"
                )));
            }
            if !allow_private_networks
                && let Some(blocked) = addresses
                    .iter()
                    .map(std::net::SocketAddr::ip)
                    .find(|ip| !is_public_ip(*ip))
            {
                return Err(boxed_message(format!(
                    "skill URL resolves to a private or reserved address: {blocked}"
                )));
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn boxed_io_error(error: std::io::Error) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(error)
}

fn boxed_message(message: String) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::other(message))
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(ipv4);
    }
    let first = ip.segments()[0];
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first & 0xffc0) == 0xfec0
        || (first == 0x2001 && ip.segments()[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_private_and_reserved_addresses() {
        for ip in ["127.0.0.1", "10.0.0.1", "169.254.1.1", "::1", "fd00::1"] {
            let parsed = ip.parse::<IpAddr>().expect("parse fixture IP");
            assert!(!is_public_ip(parsed), "{ip} must be rejected");
        }
        assert!(is_public_ip(
            "8.8.8.8".parse::<IpAddr>().expect("parse public IP")
        ));
    }
}
