//! VNC 服务探测：noVNC 前端层（6080 WebSocket）+ Xvnc RFB 后端层（5900）
//!
//! 提供 [`probe_vnc_readiness`] 供 gRPC（GetVncStatus）和 HTTP
//! （`/computer/agent/vnc-status`）共用，保证两端 vnc_ready/novnc_ready/message 语义一致。
//!
//! ## RFB 后端探测的关键约束
//!
//! [`check_vnc_rfb_ready`] 必须完成**完整 RFB 握手**（版本协商→安全协商→ClientInit→
//! ServerInit）。只读版本串就断开的"半握手"会被 TigerVNC 计为"安全失败"，累积触发
//! "Too many security failures" 锁定，拒绝前端正常连接（websockify 报 Target closed）。
//!
//! ## 多版本支持（参考 vnc-rs connector.rs / auth.rs）
//!
//! 支持 RFB 3.3 / 3.7 / 3.8 三种版本分支：
//! - 读 server 版本串 → 协商（取 server 自报版本）→ 按版本走安全协商
//! - 3.3：server 发 4 字节安全类型（单值），None 直接进初始化
//! - 3.7：server 发类型列表，client 选 None，无 SecurityResult
//! - 3.8：server 发类型列表，client 选 None，后跟 4 字节 SecurityResult

use std::path::Path;
use std::time::Duration;

use shared_types::{NOVNC_PORT, XVNC_RFB_PORT};
use shared_types_i18n::get_i18n_message;

use super::utils::check_port_available;

/// RFB 协议版本串长度（`RFB 003.00x\n` 共 12 字节，RFC 6143 §7.1.1）
const RFB_PROTOCOL_VERSION_LEN: usize = 12;

/// ServerInit 中 name 字段长度上限（防异常 server；RFC 无硬限，4096 足够任何桌面名）
const MAX_NAME_LEN: usize = 4096;

/// Security type = None（免认证），RFC 6143 §7.1.2
const SECURITY_NONE: u8 = 1;

/// 检查 Xvnc RFB 后端（5900）是否真正可服务
///
/// 通过**完整 RFB 握手**验证（支持 3.3/3.7/3.8）：读到完整 ServerInit（含 name）才算成功。
/// 完整握手不计 TigerVNC 的"安全失败"，避免触发 "Too many security failures" 锁定。
/// 卡死 / 被拒 / 超时则返回 false。
pub async fn check_vnc_rfb_ready(port: u16, timeout_millis: u64) -> bool {
    match tokio::time::timeout(
        Duration::from_millis(timeout_millis),
        check_vnc_rfb_ready_inner(port),
    )
    .await
    {
        Ok(Ok(true)) => true,
        Ok(Ok(false)) => {
            tracing::debug!("RFB probe failed (non-RFB or rejected) on port {}", port);
            false
        }
        Ok(Err(e)) => {
            tracing::debug!("RFB probe I/O error on port {}: {}", port, e);
            false
        }
        Err(_) => {
            tracing::debug!(
                "RFB probe timed out after {}ms on port {}",
                timeout_millis,
                port
            );
            false
        }
    }
}

async fn check_vnc_rfb_ready_inner(port: u16) -> std::io::Result<bool> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let mut s = TcpStream::connect(format!("127.0.0.1:{}", port)).await?;

    // 1. 读 server 协议版本（12 字节，RFB 是 server-first）
    let mut ver = [0u8; RFB_PROTOCOL_VERSION_LEN];
    s.read_exact(&mut ver).await?;
    let Some(negotiated) = parse_rfb_version(&ver) else {
        return Ok(false); // 非合法 RFB 版本串
    };

    // 2. 发 client 协议版本（与 server 协商后的版本，完成完整握手避免触发锁定）
    s.write_all(rfb_version_bytes(negotiated)).await?;

    // 3. 安全协商（按版本分支），仅接受 None（免认证）
    if !negotiate_security_none(&mut s, negotiated).await? {
        return Ok(false);
    }

    // 4. 发 ClientInit（shared=1）
    s.write_all(&[0x01]).await?;

    // 5. 读完整 ServerInit：24 字节固定头 + name（变长），完整读完避免半握手
    let mut header = [0u8; 24];
    s.read_exact(&mut header).await?;
    let name_len = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
    if name_len as usize > MAX_NAME_LEN {
        return Ok(false);
    }
    let mut name = vec![0u8; name_len as usize];
    s.read_exact(&mut name).await?;

    let _ = s.shutdown().await;
    Ok(true)
}

/// 解析 `RFB 003.00x\n` → 协商版本（仅接受 3.3/3.7/3.8），非法返回 None
fn parse_rfb_version(ver: &[u8]) -> Option<u8> {
    // 格式：`RFB xxx.yyy\n`（12 字节），参考 vnc-rs config.rs VncVersion::from
    if ver.len() < 12 || !ver.starts_with(b"RFB ") || ver[7] != b'.' || ver[11] != b'\n' {
        return None;
    }
    // ver[8..11] = minor 版本（"003"/"007"/"008"）
    let minor = std::str::from_utf8(&ver[8..11]).ok()?;
    match minor.parse::<u8>().ok()? {
        3 => Some(3),
        7 => Some(7),
        8 => Some(8),
        _ => None, // 其它版本（3.5/3.6 等历史版本）不支持
    }
}

/// 协商版本 → 对应的 client 版本串字面量
fn rfb_version_bytes(minor: u8) -> &'static [u8] {
    match minor {
        3 => b"RFB 003.003\n",
        7 => b"RFB 003.007\n",
        8 => b"RFB 003.008\n",
        // 不会到达（parse_rfb_version 已过滤），但兜底返回 3.8
        _ => b"RFB 003.008\n",
    }
}

/// 按协商版本完成 None 安全协商，成功（选到 None 并通过）= true
///
/// - RFB 3.3：server 发 4 字节安全类型（单值）；1=None 直接进初始化，0=失败（后跟 reason）
/// - RFB 3.7：server 发 `count + count×类型`；client 选 None；无 SecurityResult
/// - RFB 3.8：同 3.7，但 None 后跟 4 字节 SecurityResult（0=ok，非 0=失败 + reason）
async fn negotiate_security_none(
    s: &mut tokio::net::TcpStream,
    version: u8,
) -> std::io::Result<bool> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let sec_types: Vec<u8> = if version == 3 {
        // RFB 3.3：server 发 4 字节大端安全类型（单值）
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await?;
        let t = u32::from_be_bytes(buf);
        if t == 0 {
            // 0 = server 拒绝连接，后跟 4 字节 reason 长度 + reason（RFC 6143 §7.1.2）
            read_and_log_failure_reason(s).await;
            return Ok(false);
        }
        vec![t as u8]
    } else {
        // RFB 3.7/3.8：server 发 1 字节 count + count×类型
        let mut count_buf = [0u8; 1];
        s.read_exact(&mut count_buf).await?;
        let count = count_buf[0];
        if count == 0 {
            // count=0 = server 拒绝，后跟 reason
            read_and_log_failure_reason(s).await;
            return Ok(false);
        }
        let mut types = vec![0u8; count as usize];
        s.read_exact(&mut types).await?;
        types
    };

    if !sec_types.contains(&SECURITY_NONE) {
        return Ok(false); // 不支持免认证（需密码等）→ 不可探测
    }

    // 3.7/3.8 需要 client 显式选 None（写 1 字节）；3.3 由 server 单方决定，无需写
    if version != 3 {
        s.write_all(&[SECURITY_NONE]).await?;
    }

    // 3.8：选 None 后 server 发 4 字节 SecurityResult，0=ok
    if version == 8 {
        let mut result = [0u8; 4];
        s.read_exact(&mut result).await?;
        if u32::from_be_bytes(result) != 0 {
            // 非 0 = 失败，后跟 reason
            read_and_log_failure_reason(s).await;
            return Ok(false);
        }
    }

    Ok(true)
}

/// 读取并记录 server 给出的失败原因（4 字节长度 + 原因串，RFC 6143 §7.1.2 / §7.1.3）
async fn read_and_log_failure_reason(s: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    if s.read_exact(&mut len_buf).await.is_err() {
        return;
    }
    let reason_len = u32::from_be_bytes(len_buf) as usize;
    if reason_len > MAX_NAME_LEN {
        return;
    }
    let mut reason = vec![0u8; reason_len];
    if s.read_exact(&mut reason).await.is_err() {
        return;
    }
    tracing::debug!(
        "Xvnc refused RFB connection: {}",
        String::from_utf8_lossy(&reason)
    );
}

/// VNC 探测结果
#[derive(Debug, Clone)]
pub struct VncProbeResult {
    /// VNC 全链路就绪（novnc_ready && 5900 RFB）
    pub vnc_ready: bool,
    /// noVNC 前端代理层就绪（文件标记 + 6080 端口 + WebSocket 升级）
    pub novnc_ready: bool,
    // 诊断明细，供日志输出
    pub novnc_port_ready: bool,
    pub novnc_websocket_ready: bool,
    pub rfb_ready: bool,
    /// i18n 状态描述消息
    pub message: String,
}

/// 探测 VNC 服务就绪状态（noVNC 前端层 + Xvnc RFB 后端层）
///
/// 封装「文件标记 + 6080 TCP + 6080 WebSocket + 5900 RFB」四层探测与 message 分支。
/// 调用方（gRPC / HTTP）只需补充各自上下文特有的 `uptime_seconds` / `container_id`。
pub async fn probe_vnc_readiness(timeout_millis: u64, locale: &str) -> VncProbeResult {
    let file_exists = Path::new("/tmp/vnc_ready").exists();

    let novnc_port_ready = check_port_available(NOVNC_PORT, timeout_millis).await;
    // ⚠️ 不做 WebSocket 升级探测：websockify 是 WS↔TCP proxy，WS 升级会触发它连后端 5900，
    //    探测方立即 close 会造成 RFB 半握手 → TigerVNC "Too many security failures" 锁定，
    //    拒绝前端正常连接。websockify 进程在（6080 listen）即 WS 就绪，无需升级探测。
    let novnc_websocket_ready = novnc_port_ready;
    let rfb_ready = check_vnc_rfb_ready(XVNC_RFB_PORT, timeout_millis).await;

    let novnc_ready = file_exists && novnc_port_ready && novnc_websocket_ready;
    let vnc_ready = novnc_ready && rfb_ready;

    let message = if vnc_ready {
        get_i18n_message("grpc.status.vnc_ready", locale)
    } else if !file_exists {
        get_i18n_message("grpc.status.vnc_not_ready", locale)
    } else if novnc_ready && !rfb_ready {
        get_i18n_message("grpc.status.vnc_backend_not_ready", locale)
    } else if !novnc_websocket_ready {
        get_i18n_message("grpc.status.vnc_port_unreachable", locale)
    } else {
        get_i18n_message("grpc.status.vnc_not_ready", locale)
    };

    VncProbeResult {
        vnc_ready,
        novnc_ready,
        novnc_port_ready,
        novnc_websocket_ready,
        rfb_ready,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 假 Xvnc（3.3，None）：发版本 → 收 client 版本 → 发安全 None → 收 ClientInit → 发 ServerInit
    async fn spawn_fake_3_3(listener: TcpListener) {
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(b"RFB 003.003\n").await;
                let mut cv = [0u8; 12];
                if sock.read_exact(&mut cv).await.is_err() {
                    continue;
                }
                let _ = sock.write_all(&[0, 0, 0, SECURITY_NONE]).await; // 3.3 安全=None
                let mut ci = [0u8; 1];
                if sock.read_exact(&mut ci).await.is_err() {
                    continue;
                }
                let _ = sock.write_all(&[0u8; 24]).await; // ServerInit (name_length=0)
            }
        });
    }

    /// 假 Xvnc（3.8，None）：发版本 → 收 client 版本 → 发类型列表[None] → 收选 None →
    /// 发 SecurityResult ok → 收 ClientInit → 发 ServerInit
    async fn spawn_fake_3_8(listener: TcpListener) {
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(b"RFB 003.008\n").await;
                let mut cv = [0u8; 12];
                if sock.read_exact(&mut cv).await.is_err() {
                    continue;
                }
                let _ = sock.write_all(&[1, SECURITY_NONE]).await; // count=1, 类型=[None]
                let mut chosen = [0u8; 1];
                if sock.read_exact(&mut chosen).await.is_err() {
                    continue;
                }
                let _ = sock.write_all(&[0, 0, 0, 0]).await; // SecurityResult=ok
                let mut ci = [0u8; 1];
                if sock.read_exact(&mut ci).await.is_err() {
                    continue;
                }
                let _ = sock.write_all(&[0u8; 24]).await; // ServerInit
            }
        });
    }

    #[tokio::test]
    async fn rfb_ready_3_3_complete_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        spawn_fake_3_3(listener).await;
        assert!(check_vnc_rfb_ready(port, 2000).await);
    }

    #[tokio::test]
    async fn rfb_ready_3_8_complete_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        spawn_fake_3_8(listener).await;
        assert!(check_vnc_rfb_ready(port, 2000).await);
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_on_3_3_security_reject() {
        // 3.3 server 拒绝（security=0 + reason，如 "Too many security failures"）
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(b"RFB 003.003\n").await;
                let mut cv = [0u8; 12];
                if sock.read_exact(&mut cv).await.is_err() {
                    continue;
                }
                let _ = sock.write_all(&[0, 0, 0, 0]).await; // security=0 失败
                let reason = b"Too many security failures";
                let _ = sock.write_all(&(reason.len() as u32).to_be_bytes()).await;
                let _ = sock.write_all(reason).await;
            }
        });
        assert!(!check_vnc_rfb_ready(port, 2000).await);
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_on_garbage_version() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\n").await; // 非 RFB
            }
        });
        assert!(!check_vnc_rfb_ready(port, 2000).await);
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_on_accept_then_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                drop(sock); // accept 后立即关闭，模拟 Xvnc 崩溃
            }
        });
        assert!(!check_vnc_rfb_ready(port, 2000).await);
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_on_frozen_accept() {
        // Xvnc accept 成功但永不发数据（卡死）。read_exact 靠外层 timeout 兜底返回 false。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((_sock, _)) = listener.accept().await {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        let start = std::time::Instant::now();
        let result = check_vnc_rfb_ready(port, 500).await;
        assert!(!result);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(450),
            "should wait ~timeout, got {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "should not hang, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_when_port_closed() {
        assert!(!check_vnc_rfb_ready(1, 500).await);
    }

    #[test]
    fn parse_rfb_version_accepts_known_versions() {
        assert_eq!(parse_rfb_version(b"RFB 003.003\n"), Some(3));
        assert_eq!(parse_rfb_version(b"RFB 003.007\n"), Some(7));
        assert_eq!(parse_rfb_version(b"RFB 003.008\n"), Some(8));
    }

    #[test]
    fn parse_rfb_version_rejects_invalid() {
        assert_eq!(parse_rfb_version(b"HTTP/1.1 200 "), None); // 非 RFB
        assert_eq!(parse_rfb_version(b"RFB 003.005\n"), None); // 不支持的版本
        assert_eq!(parse_rfb_version(b"RFB 003.008x"), None); // 缺 \n
    }
}
