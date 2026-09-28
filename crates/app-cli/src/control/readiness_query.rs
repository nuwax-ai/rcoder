//! `app-cli readiness` —— 纯只读业务就绪查询子命令。
//!
//! 平台在无直接管理地址的通道（Docker exec / K8s pods/exec）里固定调用：
//! `app-cli readiness --json --admin-addr 127.0.0.1:3010`。
//!
//! 契约（Plan §5.3）：
//! - 只 GET `GET /v1/app/readiness`；在 main 的运行初始化/日志目录初始化/
//!   owner 获取之前分派——无人监听时**不启动 owner**、不生成配置、不附着。
//! - 查询成功（包括 `ready=false`）退出 0；**退出码不是业务 ready**。
//! - 传输/协议失败非零并给结构化原因；JSON 写 stdout、诊断写 stderr。
//! - 复用本 crate 的 reqwest 客户端，不依赖 curl/Node/Python，三平台可用。

use anyhow::Result;
use serde::Deserialize;
use shared_types::UserAppBusinessReadiness;

/// 查询预算（exec 通道内 loopback 单请求；覆盖连接 + 读响应）。
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 结构化查询失败（exit code + 原因码 + 人读信息）。
#[derive(Debug)]
pub struct ReadinessQueryError {
    /// 进程退出码（非零；调用方直接透传）。
    pub exit_code: i32,
    /// 结构化原因码（如 `ADMIN_UNREACHABLE`）。
    pub code: &'static str,
    /// 诊断信息（写 stderr）。
    pub message: String,
}

impl ReadinessQueryError {
    fn transport(message: String) -> Self {
        Self {
            exit_code: 2,
            code: "ADMIN_UNREACHABLE",
            message,
        }
    }

    fn unsupported(status: u16, message: String) -> Self {
        Self {
            exit_code: 3,
            code: "RUNTIME_UPGRADE_REQUIRED",
            message: format!("admin endpoint returned {status}: {message}"),
        }
    }

    fn protocol(message: String) -> Self {
        Self {
            exit_code: 4,
            code: "ADMIN_PROTOCOL_INVALID",
            message,
        }
    }
}

/// 管理 API 统一信封（`{code, message, data, tid, success}`；success 由 code
/// 推导——与 api/envelope.rs wire 锁一致，此处只取所需字段）。
#[derive(Debug, Deserialize)]
struct AdminEnvelope {
    code: String,
    #[serde(default)]
    message: String,
    data: Option<UserAppBusinessReadiness>,
}

/// 执行一次查询并输出结果。
///
/// 成功：业务快照 JSON 打印 stdout，返回 Ok（exit 0——即使 `ready=false`）。
/// 失败：结构化错误 JSON 打印 stdout（机器可读），诊断经 Err 返回给调用方
/// 写 stderr。
pub async fn run(admin_addr: &str) -> Result<(), ReadinessQueryError> {
    let snapshot = query(admin_addr).await?;
    let rendered = serde_json::to_string_pretty(&snapshot)
        .map_err(|error| ReadinessQueryError::protocol(format!("serialize result: {error}")))?;
    println!("{rendered}");
    Ok(())
}

/// 只查询不输出（组合测试复用）。
pub async fn query(admin_addr: &str) -> Result<UserAppBusinessReadiness, ReadinessQueryError> {
    let client = reqwest::Client::builder()
        .connect_timeout(QUERY_TIMEOUT)
        .timeout(QUERY_TIMEOUT)
        .build()
        .map_err(|error| ReadinessQueryError::transport(format!("build client: {error}")))?;
    let url = format!("http://{admin_addr}/v1/app/readiness");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| ReadinessQueryError::transport(format!("GET {url}: {error}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| ReadinessQueryError::transport(format!("read response body: {error}")))?;
    parse_response(status.as_u16(), &body)
}

/// 响应分类（纯函数；旧 runtime 404/405、非 2xx、信封错误、非法 JSON 分开）。
fn parse_response(
    status: u16,
    body: &str,
) -> Result<UserAppBusinessReadiness, ReadinessQueryError> {
    match status {
        200 | 201 => {}
        404 | 405 => {
            return Err(ReadinessQueryError::unsupported(
                status,
                "runtime does not implement /v1/app/readiness (business-readiness-v1)".into(),
            ));
        }
        _ => {
            return Err(ReadinessQueryError::transport(format!(
                "admin endpoint returned HTTP {status}"
            )));
        }
    }
    let envelope: AdminEnvelope = serde_json::from_str(body).map_err(|error| {
        ReadinessQueryError::protocol(format!("invalid JSON envelope: {error}"))
    })?;
    if envelope.code != "0000" {
        return Err(ReadinessQueryError::protocol(format!(
            "admin endpoint error envelope: code={} message={}",
            envelope.code, envelope.message
        )));
    }
    envelope.data.ok_or_else(|| {
        ReadinessQueryError::protocol("success envelope missing data payload".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_body() -> String {
        r#"{"code":"0000","message":"success","data":{"ready":true,"status":"ready","checked_at":"2026-09-26T08:00:00Z","observation_revision":3,"proxy":{"ready":true,"status":"ready","reason_code":null},"services":[]},"tid":null,"success":true}"#.into()
    }

    #[test]
    fn success_envelope_yields_snapshot_even_when_not_ready() {
        let body = ready_body().replace("\"ready\":true", "\"ready\":false");
        let snapshot = parse_response(200, &body).expect("observation succeeded");
        assert!(!snapshot.ready);
    }

    #[test]
    fn endpoint_404_maps_to_runtime_upgrade_required() {
        let error = parse_response(404, "not found").expect_err("404 is unsupported");
        assert_eq!(error.exit_code, 3);
        assert_eq!(error.code, "RUNTIME_UPGRADE_REQUIRED");
        let error = parse_response(405, "").expect_err("405 is unsupported");
        assert_eq!(error.code, "RUNTIME_UPGRADE_REQUIRED");
    }

    #[test]
    fn invalid_json_and_error_envelope_are_protocol_errors() {
        let error = parse_response(200, "not json").expect_err("invalid json");
        assert_eq!(error.exit_code, 4);
        assert_eq!(error.code, "ADMIN_PROTOCOL_INVALID");
        let error = parse_response(
            200,
            r#"{"code":"INTERNAL","message":"boom","data":null,"tid":null,"success":false}"#,
        )
        .expect_err("error envelope");
        assert_eq!(error.code, "ADMIN_PROTOCOL_INVALID");
        // success 信封但 data 缺失同样是协议错误（不能当 ready）
        let error = parse_response(
            200,
            r#"{"code":"0000","message":"success","data":null,"tid":null,"success":true}"#,
        )
        .expect_err("missing data");
        assert_eq!(error.code, "ADMIN_PROTOCOL_INVALID");
    }

    #[test]
    fn server_errors_map_to_transport_classification() {
        let error = parse_response(503, "busy").expect_err("5xx");
        assert_eq!(error.exit_code, 2);
        assert_eq!(error.code, "ADMIN_UNREACHABLE");
    }
}
