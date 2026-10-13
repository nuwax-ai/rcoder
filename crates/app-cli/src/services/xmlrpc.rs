//! supervisord XML-RPC（unix socket）最小客户端。
//!
//! supervisord 的控制通道是 XML-RPC over HTTP（`[unix_http_server]` +
//! `[rpcinterface:supervisor]`，Debian 默认配置自带，见 supervisord.conf）。
//! 本模块只实现编排器需要的方法子集，响应统一解析为 [`serde_json::Value`]：
//! string/int/boolean → JSON 标量，array/struct → JSON 数组/对象。
//! 解析用 roxmltree（XML-RPC 协议 20 年稳定，方法集封闭）。

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use roxmltree::Node;

#[path = "xmlrpc_deadline.rs"]
mod deadline;
#[cfg(test)]
#[path = "xmlrpc_deadline_tests.rs"]
mod deadline_tests;

pub(crate) fn require_deadline_budget(method: &str, until: tokio::time::Instant) -> Result<()> {
    deadline::require_budget(method, until)
}

pub(crate) fn is_before_dispatch(error: &anyhow::Error) -> bool {
    error.is::<deadline::BeforeDispatch>()
}

/// A parsed RPC fault is a completed response, distinct from lost or malformed replies.
#[derive(Debug)]
struct RpcFault(serde_json::Value);

impl std::fmt::Display for RpcFault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "supervisord fault: {}", self.0)
    }
}

impl std::error::Error for RpcFault {}

pub(crate) fn is_confirmed_fault(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcFault>()
        .is_some_and(RpcFault::is_complete)
}

impl RpcFault {
    fn is_complete(&self) -> bool {
        self.0
            .get("faultCode")
            .and_then(serde_json::Value::as_i64)
            .is_some()
            && self
                .0
                .get("faultString")
                .and_then(serde_json::Value::as_str)
                .is_some()
    }

    /// 目标组/进程不存在（R11 结构化分类）。
    ///
    /// 判据**仅** `faultCode == 10`（supervisord `xmlrpc.Faults.BAD_NAME`——官方
    /// 稳定错误码，官方客户端同款分类）。faultString 文案不参与判定：
    /// 措辞随版本漂移，且 `SPAWN_ERROR` 等 fault 的文案可能恰含 "BAD_NAME"。
    /// **只在 RpcFault 已解析的前提下判定**——普通网络错误无从误判。
    fn is_no_such_process(&self) -> bool {
        self.0.get("faultCode").and_then(serde_json::Value::as_i64) == Some(10)
    }
}

/// supervisord 控制客户端（unix socket）。
/// supervisord `getAllProcessInfo` 行（XML-RPC struct → 类型化字段；
/// 未知字段忽略，缺失字段保持 None——supervisord 版本差异不因此失败）。
/// name/pid/description 保留为诊断锚点（日志/后续监控消费）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum SupervisorProcessState {
    Stopped,
    Starting,
    Running,
    Backoff,
    Stopping,
    Exited,
    Fatal,
    Unknown,
}

impl SupervisorProcessState {
    fn code(self) -> i64 {
        match self {
            Self::Stopped => 0,
            Self::Starting => 10,
            Self::Running => 20,
            Self::Backoff => 30,
            Self::Stopping => 40,
            Self::Exited => 100,
            Self::Fatal => 200,
            Self::Unknown => 1000,
        }
    }

    pub(crate) fn is_transitional(self) -> bool {
        matches!(self, Self::Starting | Self::Backoff | Self::Stopping)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct SupervisordProcessInfo {
    pub group: String,
    #[serde(default)]
    pub name: Option<String>,
    /// 进程状态名（RUNNING/STOPPED/STARTING/BACKOFF/STOPPING/EXITED/FATAL/
    /// UNKNOWN/SHUTDOWN）；supervisord 文档固定大写。
    #[serde(default)]
    pub statename: Option<SupervisorProcessState>,
    #[serde(default)]
    pub state: Option<i64>,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub start: Option<i64>,
    #[serde(default)]
    pub description: Option<String>,
}

// 诊断字段（name/pid/description）当前无代码消费——保留供日志与后续
// 监控读取，显式豁免而非删除（wire 契约完整性优先）。
#[allow(dead_code)]
impl SupervisordProcessInfo {
    /// 诊断摘要（日志用）：`group/name pid=N state=S`。
    pub fn brief(&self) -> String {
        let base = format!(
            "{}/{} pid={} state={}",
            self.group,
            self.name.as_deref().unwrap_or("?"),
            self.pid.unwrap_or(0),
            self.statename
                .map(|state| format!("{state:?}"))
                .unwrap_or_else(|| "?".into())
        );
        match self.description.as_deref() {
            Some(desc) if !desc.is_empty() => format!("{base} ({desc})"),
            _ => base,
        }
    }
}

impl SupervisordProcessInfo {
    /// 组内是否有一个 RUNNING 的进程（组行 state=RUNNING）。
    pub fn checked_state(&self, name: &str) -> Result<SupervisorProcessState> {
        anyhow::ensure!(
            self.group == name && self.name.as_deref() == Some(name),
            "supervisord program identity mismatch for {name}"
        );
        let state = self
            .statename
            .context("supervisord process has no statename")?;
        anyhow::ensure!(
            self.state == Some(state.code()),
            "supervisord state fields disagree for {name}"
        );
        anyhow::ensure!(
            state != SupervisorProcessState::Unknown,
            "supervisord state UNKNOWN for {name}"
        );
        if state == SupervisorProcessState::Running {
            self.running_identity()?;
        }
        Ok(state)
    }

    pub fn running_identity(&self) -> Result<RunningProcess> {
        let pid = self
            .pid
            .filter(|pid| *pid > 0)
            .context("running supervisord program has no PID")?;
        let started = self
            .start
            .filter(|started| *started > 0)
            .context("running supervisord program has no start identity")?;
        Ok(RunningProcess { pid, started })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReloadChanges {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

fn decode_reload_changes(value: serde_json::Value) -> Result<ReloadChanges> {
    let outer = value
        .as_array()
        .context("reloadConfig returned non-array")?;
    anyhow::ensure!(outer.len() == 1, "reloadConfig must return one change set");
    let sets = outer[0]
        .as_array()
        .context("reloadConfig change set is not an array")?;
    anyhow::ensure!(
        sets.len() == 3,
        "reloadConfig must return added, changed and removed groups"
    );
    let decode = |index: usize| -> Result<Vec<String>> {
        sets[index]
            .as_array()
            .context("reloadConfig group list is not an array")?
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_string)
                    .context("reloadConfig group name is not a string")
            })
            .collect()
    };
    Ok(ReloadChanges {
        added: decode(0)?,
        changed: decode(1)?,
        removed: decode(2)?,
    })
}

#[derive(Clone)]
pub(crate) struct SupervisorClient {
    socket: std::path::PathBuf,
    #[cfg(test)]
    dispatch_hook: Option<std::sync::Arc<TestDispatchHook>>,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum DispatchHookPhase {
    BeforeAttempt,
    AfterAttempt,
}

#[cfg(test)]
struct TestDispatchHook {
    phase: DispatchHookPhase,
    reached: tokio::sync::Barrier,
    release: tokio::sync::Barrier,
}

/// Evidence for the instance launched into a newly added program group.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RunningProcess {
    pub pid: i64,
    pub started: i64,
}

#[derive(Clone, Copy)]
enum RpcAccess {
    Observation,
    Mutation,
    Cleanup,
}

impl SupervisorClient {
    pub(crate) fn new(socket: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            #[cfg(test)]
            dispatch_hook: None,
        }
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket
    }

    fn default_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(60)
    }

    pub(crate) async fn ping(&self) -> Result<String> {
        self.ping_until(Self::default_deadline()).await
    }

    pub(crate) async fn ping_until(&self, until: tokio::time::Instant) -> Result<String> {
        self.call_until("supervisor.getVersion", &[], until, RpcAccess::Observation)
            .await?
            .as_str()
            .map(str::to_string)
            .context("getVersion returned non-string")
    }

    pub(crate) async fn reload_config(&self) -> Result<ReloadChanges> {
        self.reload_config_until(Self::default_deadline()).await
    }

    pub(crate) async fn reload_config_until(
        &self,
        until: tokio::time::Instant,
    ) -> Result<ReloadChanges> {
        decode_reload_changes(
            self.call_until("supervisor.reloadConfig", &[], until, RpcAccess::Mutation)
                .await?,
        )
    }

    pub(crate) async fn add_process_group(&self, group: &str) -> Result<bool> {
        self.add_process_group_until(group, Self::default_deadline())
            .await
    }

    pub(crate) async fn add_process_group_until(
        &self,
        group: &str,
        until: tokio::time::Instant,
    ) -> Result<bool> {
        let value = match self
            .call_until(
                "supervisor.addProcessGroup",
                &[group.into()],
                until,
                RpcAccess::Mutation,
            )
            .await
        {
            Ok(value) => value,
            Err(error)
                if error.downcast_ref::<RpcFault>().is_some_and(|fault| {
                    fault.is_complete()
                        && fault.0.get("faultCode").and_then(serde_json::Value::as_i64) == Some(90)
                }) =>
            {
                let info = self.process_info_until(group, until).await?;
                info.checked_state(group)?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        anyhow::ensure!(
            value.as_bool() == Some(true),
            "supervisord did not confirm adding {group}"
        );
        Ok(true)
    }

    pub(crate) async fn stop_remove_group(&self, group: &str) -> Result<()> {
        self.stop_remove_group_until(group, Self::default_deadline())
            .await
    }

    pub(crate) async fn stop_remove_group_until(
        &self,
        group: &str,
        until: tokio::time::Instant,
    ) -> Result<()> {
        match self
            .call_until(
                "supervisor.stopProcessGroup",
                &[group.into()],
                until,
                RpcAccess::Cleanup,
            )
            .await
        {
            Ok(_) => {}
            Err(error) if is_no_such_process(&error) => return Ok(()),
            Err(error) => return Err(error),
        }
        self.remove_process_group_until(group, until).await
    }

    /// An already stopped group needs only registration removal. Repeating a
    /// stop after its delayed RPC settled can fail with NOT_RUNNING instead.
    pub(crate) async fn remove_process_group_until(
        &self,
        group: &str,
        until: tokio::time::Instant,
    ) -> Result<()> {
        let value = match self
            .call_until(
                "supervisor.removeProcessGroup",
                &[group.into()],
                until,
                RpcAccess::Cleanup,
            )
            .await
        {
            Ok(value) => value,
            Err(error) if is_no_such_process(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        anyhow::ensure!(
            value.as_bool() == Some(true),
            "supervisord did not confirm removing {group}"
        );
        Ok(())
    }

    pub(crate) async fn start_process_wait(&self, name: &str) -> Result<()> {
        self.start_process_wait_until(name, Self::default_deadline())
            .await
    }

    pub(crate) async fn start_process_wait_until(
        &self,
        name: &str,
        until: tokio::time::Instant,
    ) -> Result<()> {
        let value = self
            .call_until(
                "supervisor.startProcess",
                &[name.into(), true.into()],
                until,
                RpcAccess::Mutation,
            )
            .await?;
        anyhow::ensure!(
            value.as_bool() == Some(true),
            "supervisord did not confirm start of {name}"
        );
        Ok(())
    }

    pub(crate) async fn running_process(&self, name: &str) -> Result<RunningProcess> {
        self.running_process_until(name, Self::default_deadline())
            .await
    }

    pub(crate) async fn running_process_until(
        &self,
        name: &str,
        until: tokio::time::Instant,
    ) -> Result<RunningProcess> {
        let info = self.process_info_until(name, until).await?;
        anyhow::ensure!(
            info.checked_state(name)? == SupervisorProcessState::Running,
            "supervisord program {name} is not RUNNING"
        );
        info.running_identity()
    }

    pub(crate) async fn process_info_until(
        &self,
        name: &str,
        until: tokio::time::Instant,
    ) -> Result<SupervisordProcessInfo> {
        serde_json::from_value(
            self.call_until(
                "supervisor.getProcessInfo",
                &[name.into()],
                until,
                RpcAccess::Observation,
            )
            .await?,
        )
        .context("decode supervisord process identity")
    }

    pub(crate) async fn get_all_process_info_until(
        &self,
        until: tokio::time::Instant,
    ) -> Result<Vec<SupervisordProcessInfo>> {
        let value = self
            .call_until(
                "supervisor.getAllProcessInfo",
                &[],
                until,
                RpcAccess::Observation,
            )
            .await?;
        serde_json::from_value(value).context("decode supervisord process info")
    }

    /// Exact shutdown may wait for a previously dispatched command; ordinary
    /// writes reject a busy fence instead of forming a request queue.
    pub(crate) async fn wait_mutations_until(&self, until: tokio::time::Instant) -> Result<()> {
        #[cfg(unix)]
        return deadline::wait(&self.socket, until).await;
        #[cfg(not(unix))]
        {
            let _ = until;
            Ok(())
        }
    }

    pub(crate) fn mutations_pending() -> Result<bool> {
        #[cfg(unix)]
        return deadline::pending(&default_socket_path());
        #[cfg(not(unix))]
        Ok(false)
    }

    async fn call_until(
        &self,
        method: &'static str,
        params: &[serde_json::Value],
        until: tokio::time::Instant,
        access: RpcAccess,
    ) -> Result<serde_json::Value> {
        deadline::require_budget(method, until)?;
        let body = build_request(method, params);
        let request = format!(
            "POST /RPC2 HTTP/1.1\r\nHost: supervisor\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ) + &body;
        match access {
            RpcAccess::Observation => {
                let response = tokio::time::timeout_at(until, self.transport(&request))
                    .await
                    .with_context(|| {
                        format!("supervisord {method} observation deadline exceeded")
                    })??;
                deadline::require_budget(method, until)?;
                decode_response(&response)
            }
            RpcAccess::Mutation | RpcAccess::Cleanup => {
                let fence = deadline::fence(&self.socket)?;
                let permit = match access {
                    RpcAccess::Observation => return Err(anyhow!("observation cannot acquire a mutation fence")),
                    RpcAccess::Mutation => deadline::MutationPermit::Exclusive(fence.try_write_owned().map_err(|_| deadline::BeforeDispatch(
                        format!("supervisord {method} not dispatched: prior mutation has not settled")))?),
                    RpcAccess::Cleanup => deadline::MutationPermit::Cleanup(tokio::time::timeout_at(until, fence.read_owned()).await
                        .map_err(|_| crate::supervisor::ShutdownUnconfirmed(
                            format!("supervisord {method} prior startup/config mutation has not settled before cleanup deadline")))?),
                };
                deadline::require_budget(method, until)?;
                let dispatched = std::sync::Arc::new(deadline::DispatchState::new());
                let _caller_dispatch = deadline::CallerDispatchGuard(dispatched.clone());
                let task_dispatched = dispatched.clone();
                let client = self.clone();
                // The actual owner capability remains retained while a delayed
                // supervisor response can still represent an executing write.
                let owner = crate::supervisor::resident::owner_scope()?;
                let (send, receive) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let result = client
                        .transport_mutation(&request, until, &task_dispatched)
                        .await
                        .and_then(|response| decode_response(&response));
                    // This only settles physical RPC ownership. It never commits
                    // a publication or changes the expired request's result.
                    if let Err(error) = &result {
                        tracing::warn!(method, dispatched = task_dispatched.attempting(), error = %shared_types::sanitize_error_text(&format!("{error:#}")), "supervisord retained mutation response settled with error");
                    }
                    // The next acknowledged phase may run on another executor
                    // immediately after send; settle its physical fence first.
                    permit.release();
                    drop(owner);
                    if send.send(result).is_err() {
                        tracing::debug!(
                            method,
                            "supervisord late response settled after request completion"
                        );
                    }
                });
                let mut result = match tokio::time::timeout_at(until, receive).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => {
                        Err(anyhow!("supervisord retained mutation task lost: {error}"))
                    }
                    Err(_) => Err(anyhow!("supervisord {method} response deadline exceeded")),
                };
                if tokio::time::Instant::now() >= until
                    && !result
                        .as_ref()
                        .err()
                        .is_some_and(|error| error.is::<deadline::BeforeDispatch>())
                {
                    let detail = match result {
                        Ok(_) => "completed response arrived after the deadline".to_string(),
                        Err(error) => format!("{error:#}"),
                    };
                    // Keep the actual reason as text, while an already-expired
                    // late RpcFault cannot erase the caller's deadline outcome.
                    result = Err(anyhow!(
                        "supervisord {method} response was not accepted before request deadline: {detail}"
                    ));
                }
                let sent = dispatched.expire_pending()?;
                result.map_err(|error| {
                    if is_confirmed_fault(&error) || error.is::<deadline::BeforeDispatch>() {
                        error
                    } else if sent {
                        crate::supervisor::ShutdownUnconfirmed(format!(
                            "supervisord {method} outcome unknown: {error:#}"
                        ))
                        .into()
                    } else {
                        deadline::BeforeDispatch(format!(
                            "supervisord {method} not dispatched: {error:#}"
                        ))
                        .into()
                    }
                })
            }
        }
    }

    #[cfg(unix)]
    async fn transport(&self, request: &str) -> Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect supervisord socket {}", self.socket.display()))?;
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok(response)
    }

    #[cfg(unix)]
    async fn transport_mutation(
        &self,
        request: &str,
        until: tokio::time::Instant,
        dispatched: &deadline::DispatchState,
    ) -> Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream =
            tokio::time::timeout_at(until, tokio::net::UnixStream::connect(&self.socket))
                .await
                .map_err(|_| deadline::before_dispatch("connect"))??;
        #[cfg(test)]
        self.dispatch_stage(DispatchHookPhase::BeforeAttempt).await;
        dispatched.begin_attempt()?;
        #[cfg(test)]
        self.dispatch_stage(DispatchHookPhase::AfterAttempt).await;
        deadline::require_budget("write", until)?;
        let write = tokio::time::timeout_at(until, stream.write_all(request.as_bytes()))
            .await
            .context("supervisord mutation request write deadline exceeded")
            .and_then(|result| result.context("supervisord mutation request write failed"));
        if let Err(write_error) = &write {
            // A partial Content-Length frame cannot be completed after admission
            // expires. End its write half so a healthy server observes EOF and
            // can reject it; keep reading the original reply, never resend.
            stream.shutdown().await.with_context(|| {
                format!("close partial supervisord request write half: {write_error:#}")
            })?;
        }
        // Do not add a cleanup budget or cancel a full/partial dispatched request
        // when its caller expires. A retry/Shutdown waits this exact socket task.
        let mut response = Vec::new();
        if let Err(read) = stream.read_to_end(&mut response).await {
            return Err(read).context(match write {
                Ok(()) => "supervisord dispatched mutation response read failed".to_string(),
                Err(write) => format!("supervisord potentially partial mutation: {write:#}"),
            });
        }
        if response.is_empty()
            && let Err(write) = write
        {
            return Err(write);
        }
        Ok(response)
    }

    #[cfg(not(unix))]
    async fn transport(&self, _request: &str) -> Result<Vec<u8>> {
        anyhow::bail!(
            "supervisord unix-socket transport is unavailable on this platform: {}",
            self.socket.display()
        )
    }

    #[cfg(not(unix))]
    async fn transport_mutation(
        &self,
        _request: &str,
        _until: tokio::time::Instant,
        _dispatched: &deadline::DispatchState,
    ) -> Result<Vec<u8>> {
        self.transport("").await
    }

    #[cfg(test)]
    async fn dispatch_stage(&self, phase: DispatchHookPhase) {
        if let Some(hook) = &self.dispatch_hook
            && hook.phase == phase
        {
            hook.reached.wait().await;
            hook.release.wait().await;
        }
    }
}

fn decode_response(response: &[u8]) -> Result<serde_json::Value> {
    let text = String::from_utf8_lossy(response);
    split_body(&text)
        .ok_or_else(|| {
            anyhow!(
                "malformed XML-RPC response (no body split): {}",
                truncate(&text, 200)
            )
        })
        .and_then(parse_response)
}

/// 按标签名取首个子元素（roxmltree Children 无 find_element 便捷方法）。
fn child_elem<'a>(node: Node<'a, '_>, name: &str) -> Option<Node<'a, 'a>> {
    node.children()
        .find(|e| e.is_element() && e.has_tag_name(name))
}

/// [`RpcFault::is_no_such_process`] 的错误链入口（R11：typed downcast 取代
/// 全链字符串匹配——只有已解析的 supervisord fault 才参与"不存在"分类）。
fn is_no_such_process(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcFault>()
        .is_some_and(RpcFault::is_no_such_process)
}

/// 拼 XML-RPC 请求（参数以 JSON 标量承载：string/bool/i64）。
fn build_request(method: &str, params: &[serde_json::Value]) -> String {
    let mut param_xml = String::new();
    for param in params {
        let value = match param {
            serde_json::Value::String(s) => {
                format!("<value><string>{}</string></value>", escape(s))
            }
            serde_json::Value::Bool(b) => format!(
                "<value><boolean>{}</boolean></value>",
                if *b { 1 } else { 0 }
            ),
            serde_json::Value::Number(n) => format!("<value><int>{n}</int></value>"),
            other => format!(
                "<value><string>{}</string></value>",
                escape(&other.to_string())
            ),
        };
        param_xml.push_str(&format!("<param>{value}</param>"));
    }
    format!(
        "<?xml version=\"1.0\"?><methodCall><methodName>{method}</methodName><params>{param_xml}</params></methodCall>"
    )
}

/// 响应按空行切 header/body（Connection: close 单响应，无 chunked）。
fn split_body(response: &str) -> Option<&str> {
    let idx = response.find("\r\n\r\n")?;
    let body = &response[idx + 4..];
    // 某些实现可能带 chunked 编码——supervisord 默认 Content-Length 直发；
    // 防御：body 若以 chunk 长度行开头则不可用（当前版本不会出现）
    Some(body)
}

/// 解析 methodResponse：fault → Err；params[0] 的 value → JSON。
fn parse_response(xml: &str) -> Result<serde_json::Value> {
    let doc = roxmltree::Document::parse(xml).context("parse XML-RPC response XML")?;
    let root = doc.root_element();
    if root.tag_name().name() != "methodResponse" {
        return Err(anyhow!(
            "unexpected XML-RPC root: {}",
            root.tag_name().name()
        ));
    }
    if let Some(fault) = child_elem(root, "fault") {
        let detail = child_elem(fault, "value")
            .map(parse_value)
            .transpose()?
            .unwrap_or(serde_json::Value::Null);
        return Err(RpcFault(detail).into());
    }
    let params =
        child_elem(root, "params").ok_or_else(|| anyhow!("methodResponse without params"))?;
    let first =
        child_elem(params, "param").ok_or_else(|| anyhow!("methodResponse without param"))?;
    let value = child_elem(first, "value").ok_or_else(|| anyhow!("param without value"))?;
    parse_value(value)
}

/// XML-RPC value → JSON（string/int/i4/boolean/double/array/struct 递归）。
fn parse_value(value: Node<'_, '_>) -> Result<serde_json::Value> {
    // 无类型标签的裸文本 = string
    let type_node = value.children().find(|n| n.is_element());
    let Some(type_node) = type_node else {
        let text = value.text().unwrap_or_default();
        return Ok(serde_json::Value::String(text.trim().to_string()));
    };
    match type_node.tag_name().name() {
        "string" => Ok(serde_json::Value::String(
            type_node.text().unwrap_or_default().to_string(),
        )),
        "int" | "i4" => {
            let text = type_node.text().unwrap_or_default();
            text.trim()
                .parse::<i64>()
                .map(|n| serde_json::Value::Number(n.into()))
                .with_context(|| format!("parse int '{text}'"))
        }
        "boolean" => {
            let flag = type_node.text().unwrap_or("0").trim() == "1";
            Ok(serde_json::Value::Bool(flag))
        }
        "double" => {
            let text = type_node.text().unwrap_or_default();
            text.trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(serde_json::Value::Number)
                .ok_or_else(|| anyhow!("parse double '{text}'"))
        }
        "array" => {
            let data = child_elem(type_node, "data");
            let mut items = Vec::new();
            if let Some(data) = data {
                for item in data.children().filter(|n| n.is_element()) {
                    if item.tag_name().name() == "value" {
                        items.push(parse_value(item)?);
                    }
                }
            }
            Ok(serde_json::Value::Array(items))
        }
        "struct" => {
            let mut map = serde_json::Map::new();
            for member in type_node.children().filter(|n| n.is_element()) {
                if member.tag_name().name() != "member" {
                    continue;
                }
                let name = child_elem(member, "name")
                    .and_then(|n| n.text())
                    .unwrap_or_default()
                    .to_string();
                let value_node = child_elem(member, "value");
                if let Some(value_node) = value_node {
                    map.insert(name, parse_value(value_node)?);
                }
            }
            Ok(serde_json::Value::Object(map))
        }
        other => Err(anyhow!("unsupported XML-RPC value type: {other}")),
    }
}

fn escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// supervisord socket 默认路径（Debian 包主配置）。
pub(crate) fn default_socket_path() -> std::path::PathBuf {
    std::env::var_os("APP_CLI_SUPERVISOR_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/var/run/supervisor.sock".into())
}

/// socket 是否存在（serve 启动时的引擎自动探测第一关；第二关 ping）。
pub(crate) fn socket_exists() -> bool {
    Path::new(&default_socket_path()).exists()
}

#[cfg(test)]
include!("xmlrpc_tests.rs");
