//! JSONL 场景报告器：每场景一个独立文件，事件实时逐行追加 + flush。
//!
//! 设计目标（供 agent 追溯排查）：
//! - 连续的 `sse_event` 行 = 原样保留 SSE 消息时序流，可 grep/jq/分段读取
//! - 逐行实时落盘：测试中途挂掉或被 kill，已收事件全部保留
//! - `assert` 行分两级：`hard`（可穷举不变量，fail 即场景 fail）与
//!   `diagnostic`（程序算出的特征指标，不判死——缺失/重复的复杂形态由
//!   agent 看报告判定，新发现的异常模式沉淀为新 hard 断言）
//! - api_key 永不落盘（chat_request 行写入前由调用方脱敏）
//! - 终态兜底：正常路径 `finish()` 写 `scenario_end`；panic/早退由 Drop 补写
//!   `verdict=aborted`（进程被 kill -9 时 jsonl 已有内容仍然有效）

use std::cell::Cell;
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::{Value, json};

/// 本进程的 run 目录（`tests-e2e/reports/<run_tag>_<pid>/`），场景间共享。
/// cargo test 的每个测试二进制是独立进程，run_tag 带 pid 防多二进制混目录。
fn run_dir() -> &'static Path {
    static RUN_DIR: OnceLock<PathBuf> = OnceLock::new();
    RUN_DIR.get_or_init(|| {
        let tag = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("reports")
            .join(format!("{tag}_p{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create reports run dir");
        dir
    })
}

/// run 级 summary.json 跨场景合并（make 目标 --test-threads=1 串行；Mutex 兜底）。
static SUMMARY: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Skip,
    Aborted,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Skip => "skip",
            Verdict::Aborted => "aborted",
        }
    }
}

/// chat 留痕参数（chat_request 的结构化入参）。
pub struct ChatTrace<'a> {
    pub phase: &'a str,
    pub url: &'a str,
    pub ok: bool,
    pub request_sanitized: Value,
    pub response: Option<&'a Value>,
    pub error: Option<&'a str>,
    pub elapsed_ms: u128,
}

pub struct JsonlReporter {
    file: Mutex<File>,
    pub path: PathBuf,
    scenario: String,
    backend: String,
    started: Instant,
    hard_pass: Cell<u32>,
    hard_fail: Cell<u32>,
    failed_names: RefCell<Vec<String>>,
    end_written: Cell<bool>,
}

impl JsonlReporter {
    /// 开始一个场景：立即写 `scenario_begin` 行（文件建立即可见）。
    pub fn begin(scenario: &str, backend: &str, environment: Value) -> Self {
        let path = run_dir().join(format!("{scenario}__{backend}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .expect("open scenario jsonl");
        let reporter = Self {
            file: Mutex::new(file),
            path,
            scenario: scenario.to_owned(),
            backend: backend.to_owned(),
            started: Instant::now(),
            hard_pass: Cell::new(0),
            hard_fail: Cell::new(0),
            failed_names: RefCell::new(Vec::new()),
            end_written: Cell::new(false),
        };
        reporter.write_line(json!({
            "kind": "scenario_begin",
            "scenario": scenario,
            "backend": backend,
            "environment": environment,
        }));
        reporter
    }

    /// 跳过场景（环境门控未过等）：写终态行并消耗 self（测试函数随后 return）。
    pub fn skip(mut self, reason: &str) {
        self.write_end(Verdict::Skip, Some(reason));
    }

    /// chat 请求留痕（request 须已脱敏；失败时 ok=false + error）。
    pub fn chat_request(&self, trace: ChatTrace<'_>) {
        let mut line = json!({
            "kind": "chat_request",
            "phase": trace.phase,
            "url": trace.url,
            "ok": trace.ok,
            "request": trace.request_sanitized,
            "elapsed_ms": trace.elapsed_ms,
        });
        if let Some(resp) = trace.response {
            line["response"] = resp.clone();
        }
        if let Some(err) = trace.error {
            line["error"] = json!(err);
        }
        self.write_line(line);
    }

    pub fn subscribe_begin(&self, phase: &str, entry: &str, last_event_id: Option<u64>) {
        self.write_line(json!({
            "kind": "subscribe_begin",
            "phase": phase,
            "entry": entry,
            "last_event_id_sent": last_event_id,
        }));
    }

    /// SSE 事件实时落盘（t_ms = 相对 collect 开始的毫秒，由调用方传入）。
    pub fn sse_event(&self, phase: &str, seq: Option<u64>, event: &str, data: &Value, t_ms: u128) {
        self.write_line(json!({
            "kind": "sse_event",
            "phase": phase,
            "seq": seq,
            "event": event,
            "data": data,
            "t_ms": t_ms,
        }));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn subscribe_end(
        &self,
        phase: &str,
        ended_reason: &str,
        event_count: usize,
        seqs: &[u64],
        type_counts: Value,
        assembled_text: &str,
    ) {
        self.write_line(json!({
            "kind": "subscribe_end",
            "phase": phase,
            "ended_reason": ended_reason,
            "event_count": event_count,
            "seqs": seqs,
            "type_counts": type_counts,
            "assembled_text": assembled_text,
        }));
    }

    /// 硬断言：fail 计入场景 verdict。返回 ok 便于调用侧短路。
    pub fn assert_hard(&self, name: &str, ok: bool, detail: String) -> bool {
        if ok {
            self.hard_pass.set(self.hard_pass.get() + 1);
        } else {
            self.hard_fail.set(self.hard_fail.get() + 1);
            self.failed_names.borrow_mut().push(name.to_owned());
        }
        self.write_line(json!({
            "kind": "assert",
            "level": "hard",
            "name": name,
            "ok": ok,
            "detail": detail,
        }));
        ok
    }

    /// 诊断指标：不判死，供 agent 分析（如逐字重放窗口检测、首事件延迟、拼接全文）。
    pub fn diagnostic(&self, name: &str, value: &str, detail: &str) {
        self.write_line(json!({
            "kind": "assert",
            "level": "diagnostic",
            "name": name,
            "value": value,
            "detail": detail,
        }));
    }

    /// 正常收尾：写 `scenario_end` + 合并 summary.json。
    /// 返回 hard 断言是否全过（调用侧据此 assert! 让测试红）。
    pub fn finish(mut self) -> bool {
        let all_pass = self.hard_fail.get() == 0;
        self.write_end(
            if all_pass {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            None,
        );
        all_pass
    }

    fn write_end(&mut self, verdict: Verdict, error: Option<&str>) {
        if self.end_written.get() {
            return;
        }
        self.end_written.set(true);
        let mut line = json!({
            "kind": "scenario_end",
            "verdict": verdict.as_str(),
            "duration_s": self.started.elapsed().as_secs_f64(),
            "hard_pass": self.hard_pass.get(),
            "hard_fail": self.hard_fail.get(),
            "failed_asserts": self.failed_names.borrow().as_slice(),
        });
        if let Some(err) = error {
            line["error"] = json!(err);
        }
        self.write_line(line);
        self.merge_summary(verdict);
    }

    /// 合并写 run 级 summary.json（agent 的入口文件：verdict + jsonl 路径 + 失败断言）。
    fn merge_summary(&self, verdict: Verdict) {
        let _guard = SUMMARY.lock().expect("summary lock");
        let summary_path = run_dir().join("summary.json");
        let mut summary: Value = std::fs::read_to_string(&summary_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({"scenarios": []}));
        let entry = json!({
            "scenario": self.scenario,
            "backend": self.backend,
            "verdict": verdict.as_str(),
            "report": self.path.display().to_string(),
            "failed_asserts": self.failed_names.borrow().as_slice(),
        });
        if let Some(arr) = summary["scenarios"].as_array_mut() {
            // 同名场景同后端重跑时覆盖旧条目
            arr.retain(|e| e["scenario"] != entry["scenario"] || e["backend"] != entry["backend"]);
            arr.push(entry);
        }
        // 先写临时文件再 rename，避免 agent 半途读到截断 JSON
        let tmp = run_dir().join("summary.json.tmp");
        let body = serde_json::to_string_pretty(&summary).unwrap_or_default();
        let write_ok = File::create(&tmp)
            .and_then(|mut f| {
                f.write_all(body.as_bytes())?;
                f.flush()
            })
            .is_ok()
            && std::fs::rename(&tmp, &summary_path).is_ok();
        if !write_ok {
            std::fs::write(&summary_path, body).ok();
        }
    }

    fn write_line(&self, mut line: Value) {
        line["ts"] = json!(chrono::Local::now().to_rfc3339());
        if let Ok(mut file) = self.file.lock()
            && let Ok(body) = serde_json::to_string(&line)
        {
            writeln!(file, "{body}").ok();
            file.flush().ok();
        }
    }
}

impl Drop for JsonlReporter {
    /// 未正常 finish（panic/早退）→ 补写终态，保证 jsonl 可判定。
    fn drop(&mut self) {
        if !self.end_written.get() {
            self.write_end(Verdict::Aborted, Some("scenario ended without finish()"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_str_round() {
        assert_eq!(Verdict::Pass.as_str(), "pass");
        assert_eq!(Verdict::Aborted.as_str(), "aborted");
    }
}
