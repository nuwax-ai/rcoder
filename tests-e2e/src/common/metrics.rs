//! /metrics 快照：解析 /computer/chat 相关的 Prometheus 文本行，
//! 场景收口时做前后 diff 写 jsonl（性能观测：请求量/延迟增量）。
//!
//! 指标来源是 HttpMetricsLayer（rcoder 真实产出，非空挂的 gRPC/agent 指标）：
//! - http_requests_total{method="POST",path="/computer/chat",status="200"}
//! - http_request_duration_seconds_{sum,count}{...path="/computer/chat"...}

use serde_json::json;

#[derive(Debug, Clone, PartialEq)]
pub struct MetricsSnapshot {
    pub chat_requests_total: f64,
    pub chat_duration_sum: f64,
    pub chat_duration_count: f64,
}

impl MetricsSnapshot {
    /// 拉取并解析；失败（未启用 /metrics 等）返回 None。
    pub async fn fetch(http: &reqwest::Client, base_url: &str) -> Option<Self> {
        let text = http
            .get(format!("{base_url}/metrics"))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .text()
            .await
            .ok()?;
        Some(Self::parse(&text))
    }

    /// 从 Prometheus 文本解析目标行（缺行按 0）。
    pub fn parse(text: &str) -> Self {
        let mut snap = Self {
            chat_requests_total: 0.0,
            chat_duration_sum: 0.0,
            chat_duration_count: 0.0,
        };
        for line in text.lines() {
            let value = line.rsplit(' ').next().and_then(|v| v.parse::<f64>().ok());
            let Some(value) = value else { continue };
            if !line.contains("path=\"/computer/chat\"") {
                continue;
            }
            if line.starts_with("http_requests_total")
                && line.contains("method=\"POST\"")
                && line.contains("status=\"200\"")
            {
                snap.chat_requests_total += value;
            } else if line.starts_with("http_request_duration_seconds_sum") {
                snap.chat_duration_sum += value;
            } else if line.starts_with("http_request_duration_seconds_count") {
                snap.chat_duration_count += value;
            }
        }
        snap
    }

    /// 与前值的增量（jsonl 的 metrics_diff 行）。
    pub fn diff_json(&self, before: &Self) -> serde_json::Value {
        let d_req = self.chat_requests_total - before.chat_requests_total;
        let d_sum = self.chat_duration_sum - before.chat_duration_sum;
        let d_cnt = self.chat_duration_count - before.chat_duration_count;
        json!({
            "chat_requests_delta": d_req,
            "chat_duration_sum_delta_s": d_sum,
            "chat_duration_count_delta": d_cnt,
            "chat_avg_latency_s": if d_cnt > 0.0 { Some(d_sum / d_cnt) } else { None },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chat_metrics_lines() {
        let text = "\
# HELP http_requests_total help
http_requests_total{method=\"GET\",path=\"/health\",status=\"200\"} 42
http_requests_total{method=\"POST\",path=\"/computer/chat\",status=\"200\"} 10
http_request_duration_seconds_sum{method=\"POST\",path=\"/computer/chat\"} 5.5
http_request_duration_seconds_count{method=\"POST\",path=\"/computer/chat\"} 10
";
        let snap = MetricsSnapshot::parse(text);
        assert_eq!(snap.chat_requests_total, 10.0);
        assert_eq!(snap.chat_duration_sum, 5.5);
        assert_eq!(snap.chat_duration_count, 10.0);
    }

    #[test]
    fn diff_computes_avg_latency() {
        let before = MetricsSnapshot {
            chat_requests_total: 10.0,
            chat_duration_sum: 5.0,
            chat_duration_count: 10.0,
        };
        let after = MetricsSnapshot {
            chat_requests_total: 12.0,
            chat_duration_sum: 7.0,
            chat_duration_count: 12.0,
        };
        let d = after.diff_json(&before);
        assert_eq!(d["chat_requests_delta"], json!(2.0));
        assert_eq!(d["chat_avg_latency_s"], json!(1.0));
    }

    #[test]
    fn empty_text_is_zero_snapshot() {
        let snap = MetricsSnapshot::parse("");
        assert_eq!(snap.chat_requests_total, 0.0);
    }
}
