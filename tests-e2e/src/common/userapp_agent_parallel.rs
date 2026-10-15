//! Real AI UserApp conversations sharing source files but not agent execution.
use std::{cell::RefCell, time::Duration};

use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::{Backend, Env, chat_reported, report::JsonlReporter, scenario::CollectSpec, sse};

struct GateStream<'a> {
    entry: &'a str,
    sid: &'a str,
    phase: &'a str,
    marker: &'a str,
    request_id: &'a str,
    started: watch::Sender<bool>,
    stop_on_cancel: bool,
}

async fn collect_signalled(
    env: &Env,
    report: &JsonlReporter,
    spec: GateStream<'_>,
) -> Vec<sse::SseEvent> {
    let GateStream {
        entry,
        sid,
        phase,
        marker,
        request_id,
        started,
        stop_on_cancel,
    } = spec;
    report.subscribe_begin(phase, entry, None);
    let captured = RefCell::new(Vec::new());
    let (terminal, mut terminal_rx) = watch::channel(false);
    let reason = {
        let stream = sse::collect(&env.sse_http, entry, sid, 450.0, None, true, |event| {
            report.sse_event(phase, event.seq, &event.event, &event.data, event.t_ms);
            captured.borrow_mut().push(event.clone());
            if matches!(event.event.as_str(), "tool_call" | "tool_call_update")
                && event.data.to_string().contains(marker)
            {
                started.send_replace(true);
            }
            if stop_on_cancel
                && event.event == "cancelled"
                && event.data["data"]["request_id"].as_str() == Some(request_id)
            {
                terminal.send_replace(true);
            }
        });
        tokio::pin!(stream);
        tokio::select! {
            (_, reason) = &mut stream => reason.as_str().to_owned(),
            _ = terminal_rx.changed() => {
                "cancelled_observed".to_owned()
            }
            _ = tokio::time::sleep(Duration::from_secs(460)) => "deadline".to_owned(),
        }
    };
    let events = captured.into_inner();
    report.subscribe_end(
        phase,
        &reason,
        events.len(),
        &sse::ids_of(&events),
        sse::type_counts(&events),
        &sse::chunks_text(&events),
    );
    events
}

async fn read_file(
    env: &Env,
    entry: &str,
    app: &str,
    user: &str,
    project: &str,
    file: &str,
) -> Result<String> {
    let response = env
        .http
        .post(format!("{entry}/api/git/file-content"))
        .header("X-Service-Type", "userapp")
        .header("X-App-Id", app)
        .json(
            &json!({"workspaceType":"userApp","serviceType":"userapp","appId":app,
            "userId":user,"cId":project,"projectId":project,"ref":"worktree","filePath":file}),
        )
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "read {file}: HTTP {}",
        response.status()
    );
    let value: Value = response.json().await?;
    ensure!(
        value["success"] == true,
        "read {file}: {}",
        value["message"]
    );
    value["content"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("missing content for {file}"))
}

struct GateFile<'a> {
    entry: &'a str,
    app: &'a str,
    user: &'a str,
    project: &'a str,
    file: &'a str,
}

async fn wait_for_gate(
    env: &Env,
    file: GateFile<'_>,
    receiver: &mut watch::Receiver<bool>,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            if read_file(
                env,
                file.entry,
                file.app,
                file.user,
                file.project,
                file.file,
            )
            .await?
                == "started"
                && *receiver.borrow()
            {
                return Ok::<_, anyhow::Error>(());
            }
            ensure!(
                receiver.has_changed().is_ok(),
                "agent stream ended before executing {}",
                file.file
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await??;
    Ok(())
}

async fn post(env: &Env, entry: &str, path: &str, body: Value) -> Result<Value> {
    let response = env
        .http
        .post(format!("{entry}{path}"))
        .json(&body)
        .timeout(Duration::from_secs(90))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "{path}: HTTP {}",
        response.status()
    );
    let value: Value = response.json().await?;
    ensure!(value["code"] == "0000", "{path}: {}", value["message"]);
    Ok(value)
}

fn no_foreign_request(events: &[sse::SseEvent], foreign_request: &str) -> bool {
    events.iter().all(|event| {
        event.data["requestId"].as_str() != Some(foreign_request)
            && event.data["data"]["request_id"].as_str() != Some(foreign_request)
    })
}

fn has_identity(events: &[sse::SseEvent], sid: &str, request: &str, terminal: &str) -> bool {
    events.iter().all(|event| {
        event.data["sessionId"]
            .as_str()
            .is_none_or(|value| value == sid)
    }) && events
        .iter()
        .any(|event| event.event == terminal && event.data["data"]["request_id"] == request)
}

/// The caller supplies an owned test app and entries; Compose and K8s run the
/// same behavioral assertions. The model must actually execute the shell gate.
pub async fn verify(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    backend: Backend,
    entries: &[String],
) -> Result<()> {
    ensure!(
        !env.api_key.is_empty() && !env.model.is_empty() && !env.base_url.is_empty(),
        "real LLM configuration is required"
    );
    let entry_a = entries.first().ok_or_else(|| anyhow!("no test entry"))?;
    let entry_b = &entries[1 % entries.len()];
    let project_a = format!("{app}-conversation-a");
    let project_b = format!("{app}-conversation-b");
    let token_a = format!("ISOLATED_A_{}", env.run_tag);
    let token_b = format!("ISOLATED_B_{}", env.run_tag);
    let release_file = format!("parallel-release-{app}.txt");
    let file_a = format!("parallel-a-{app}.txt");
    let file_b = format!("parallel-b-{app}.txt");
    let gate_a = format!("parallel-a-started-{app}.txt");
    let gate_b = format!("parallel-b-started-{app}.txt");
    let gate_b_stop = format!("parallel-b-stop-started-{app}.txt");
    let request_id_a = format!("{}-parallel-a", env.run_tag);
    let request_id_b = format!("{}-parallel-b", env.run_tag);
    let request_id_a2 = format!("{}-parallel-a-resume", env.run_tag);
    let request_id_b2 = format!("{}-parallel-b-resume", env.run_tag);
    let request_id_b3 = format!("{}-parallel-b-context", env.run_tag);
    let request_id_b4 = format!("{}-parallel-b-stop", env.run_tag);
    let make_request = |project: &str, prompt: &str, tag: &str| {
        let mut request =
            env.base_payload(backend, prompt, &format!("{}-{tag}", env.run_tag), user);
        request.service_type = Some(shared_types::ServiceType::Userapp);
        request.app_id = Some(app.to_owned());
        request.project_id = Some(project.to_owned());
        request
    };
    let command_a = format!(
        "printf started > {gate_a}; i=0; while [ ! -f {release_file} ] && [ $i -lt 420 ]; do sleep 1; i=$((i+1)); done; test -f {release_file} && printf '{token_a}' > {file_a}"
    );
    let request_a = make_request(
        &project_a,
        &format!(
            "必须立即调用 shell 工具原样执行这一条命令，等待它结束，不要把命令改成后台任务：\n{command_a}\n成功后只输出 {token_a}。"
        ),
        "parallel-a",
    );
    let response_a = chat_reported(env, report, "chat_a", entry_a, &request_a).await?;
    report.assert_hard(
        "A 回显原始 project_id",
        response_a.project_id == project_a,
        response_a.project_id.clone(),
    );
    let (started_a, mut wait_a) = watch::channel(false);
    let stream_a = collect_signalled(
        env,
        report,
        GateStream {
            entry: entry_b,
            sid: &response_a.session_id,
            phase: "stream_a",
            marker: &gate_a,
            request_id: &request_id_a,
            started: started_a,
            stop_on_cancel: false,
        },
    );
    let orchestrate = async {
        wait_for_gate(
            env,
            GateFile {
                entry: entry_b,
                app,
                user,
                project: &project_a,
                file: &gate_a,
            },
            &mut wait_a,
        )
        .await?;
        report.assert_hard("A 阻塞工具实际开始执行", true, gate_a.clone());
        let command_b = format!("printf started > {gate_b}; sleep 180");
        let mut request_b = make_request(
            &project_b,
            &format!("立即调用 shell 工具执行以下命令，保持前台运行，不要自行取消：\n{command_b}"),
            "parallel-b",
        );
        // Deliberately carry A's SID: B must start a new session, not cancel A.
        request_b.session_id = Some(response_a.session_id.clone());
        let response_b =
            chat_reported(env, report, "chat_b_foreign_hint", entry_b, &request_b).await?;
        report.assert_hard(
            "B 回显原始 project_id 且 session 与 A 不同",
            response_b.project_id == project_b && response_b.session_id != response_a.session_id,
            format!(
                "A={}, B={}, project={}",
                response_a.session_id, response_b.session_id, response_b.project_id
            ),
        );
        let (started_b, mut wait_b) = watch::channel(false);
        let stream_b = collect_signalled(
            env,
            report,
            GateStream {
                entry: entry_a,
                sid: &response_b.session_id,
                phase: "stream_b",
                marker: &gate_b,
                request_id: &request_id_b,
                started: started_b,
                stop_on_cancel: true,
            },
        );
        let cancel_b = async {
            wait_for_gate(
                env,
                GateFile {
                    entry: entry_a,
                    app,
                    user,
                    project: &project_b,
                    file: &gate_b,
                },
                &mut wait_b,
            )
            .await?;
            report.assert_hard("B 阻塞工具实际开始执行", true, gate_b.clone());
            let status_a = post(
                env,
                entry_b,
                "/computer/agent/status",
                json!({"user_id":user,"project_id":project_a}),
            )
            .await?;
            report.assert_hard(
                "B 启动后 A 仍 busy",
                status_a["data"]["is_alive"] == true && status_a["data"]["status"] == "busy",
                status_a["data"].to_string(),
            );
            let cancelled = post(
                env,
                entry_a,
                "/computer/agent/session/cancel",
                json!({"user_id":user,"project_id":project_b,"session_id":response_b.session_id}),
            )
            .await?;
            report.assert_hard(
                "无 app_id 请求仅取消 B",
                cancelled["data"]["success"] == true,
                cancelled["data"].to_string(),
            );
            Ok::<_, anyhow::Error>(())
        };
        let (events_b, ()) =
            tokio::try_join!(async { Ok::<_, anyhow::Error>(stream_b.await) }, cancel_b)?;
        report.assert_hard(
            "B 收到 cancelled 且没有 A 的 request_id",
            has_identity(
                &events_b,
                &response_b.session_id,
                &request_id_b,
                "cancelled",
            ) && no_foreign_request(&events_b, &request_id_a),
            format!("events={}", events_b.len()),
        );

        let mut continuation = make_request(
            &project_b,
            &format!(
                "必须调用 shell 工具原样执行下面这一条命令，在当前工作目录精确写入标记，不得添加换行或其它字节：\nprintf '%s' '{token_b}' > '{file_b}'\n命令成功后只输出 {token_b}。"
            ),
            "parallel-b-resume",
        );
        continuation.session_id = Some(response_b.session_id.clone());
        let resumed_b = chat_reported(env, report, "chat_b_resume", entry_a, &continuation).await?;
        let (events_b2, _) = super::scenario::collect_reported(
            env,
            report,
            CollectSpec {
                phase: "stream_b_resume",
                entry: entry_b,
                sid: &resumed_b.session_id,
                duration_s: 90.0,
                last_event_id: events_b.iter().filter_map(|event| event.seq).max(),
                idle_stop: true,
            },
        )
        .await;
        report.assert_hard(
            "B 独立续聊完成",
            resumed_b.session_id == response_b.session_id
                && has_identity(
                    &events_b2,
                    &resumed_b.session_id,
                    &request_id_b2,
                    "end_turn",
                )
                && no_foreign_request(&events_b2, &request_id_a)
                && sse::chunks_text(&events_b2).contains(&token_b),
            sse::chunks_text(&events_b2),
        );
        let mut context_request = make_request(
            &project_b,
            "不要调用工具或读取文件。检查本会话已有的历史消息：如果以前出现过 ISOLATED_A_ 开头的完整标记，只输出这个标记；否则只输出 NO_A_HISTORY。",
            "parallel-b-context",
        );
        context_request.session_id = Some(resumed_b.session_id.clone());
        let context_b =
            chat_reported(env, report, "chat_b_context", entry_a, &context_request).await?;
        let (events_b3, _) = super::scenario::collect_reported(
            env,
            report,
            CollectSpec {
                phase: "stream_b_context",
                entry: entry_b,
                sid: &context_b.session_id,
                duration_s: 90.0,
                last_event_id: events_b2.iter().filter_map(|event| event.seq).max(),
                idle_stop: true,
            },
        )
        .await;
        report.assert_hard(
            "B 未继承 A 会话上下文",
            context_b.session_id == response_b.session_id
                && has_identity(
                    &events_b3,
                    &context_b.session_id,
                    &request_id_b3,
                    "end_turn",
                )
                && sse::chunks_text(&events_b3).trim() == "NO_A_HISTORY"
                && !events_b3.iter().any(|event| event.event == "tool_call"),
            sse::chunks_text(&events_b3),
        );
        let mut stop_request = make_request(
            &project_b,
            &format!(
                "立即调用shell工具原样执行，保持前台运行：printf started > {gate_b_stop}; sleep 180"
            ),
            "parallel-b-stop",
        );
        stop_request.session_id = Some(resumed_b.session_id.clone());
        let stop_b =
            chat_reported(env, report, "chat_b_stop_running", entry_b, &stop_request).await?;
        ensure!(
            stop_b.session_id == response_b.session_id,
            "B stop turn changed the session"
        );
        let (started_stop, mut wait_stop) = watch::channel(false);
        let stop_stream = collect_signalled(
            env,
            report,
            GateStream {
                entry: entry_a,
                sid: &stop_b.session_id,
                phase: "stream_b_stop_running",
                marker: &gate_b_stop,
                request_id: &request_id_b4,
                started: started_stop,
                stop_on_cancel: true,
            },
        );
        let stop_control = async {
            wait_for_gate(
                env,
                GateFile {
                    entry: entry_a,
                    app,
                    user,
                    project: &project_b,
                    file: &gate_b_stop,
                },
                &mut wait_stop,
            )
            .await?;
            report.assert_hard("停止 B 前阻塞工具实际在执行", true, gate_b_stop.clone());
            post(
                env,
                entry_b,
                "/computer/agent/stop",
                json!({"user_id":user,"project_id":project_b,"session_id":stop_b.session_id}),
            )
            .await
        };
        let (events_b4, stopped) = tokio::try_join!(
            async { Ok::<_, anyhow::Error>(stop_stream.await) },
            stop_control
        )?;
        report.assert_hard(
            "无 app_id 请求仅停止 B",
            stopped["data"]["success"] == true,
            stopped["data"].to_string(),
        );
        let status_b = post(
            env,
            entry_a,
            "/computer/agent/status",
            json!({"user_id":user,"project_id":project_b}),
        )
        .await?;
        report.assert_hard(
            "B 停止后不再存活且停止轮事件归属正确",
            status_b["data"]["is_alive"] == false
                && has_identity(&events_b4, &stop_b.session_id, &request_id_b4, "cancelled")
                && no_foreign_request(&events_b4, &request_id_a),
            status_b["data"].to_string(),
        );
        let status_a = post(
            env,
            entry_a,
            "/computer/agent/status",
            json!({"user_id":user,"project_id":project_a}),
        )
        .await?;
        report.assert_hard(
            "B 停止后 A 仍 busy",
            status_a["data"]["is_alive"] == true && status_a["data"]["status"] == "busy",
            status_a["data"].to_string(),
        );
        Ok::<_, anyhow::Error>(())
    };
    // Always release A, including when B's control request fails, so failure
    // cleanup does not leave the real agent's shell gate running indefinitely.
    let release = async {
        let outcome = tokio::time::timeout(Duration::from_secs(360), orchestrate)
            .await
            .map_err(|_| anyhow!("parallel orchestration deadline"))
            .and_then(|result| result);
        let response = env
            .http
            .post(format!("{entry_a}/api/v1/userapp/generate-file"))
            .header("X-App-Id", app)
            .json(
                &json!({"app_id":app,"user_id":user,"file_name":release_file,"content":"release"}),
            )
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "release A HTTP {}",
            response.status()
        );
        let body: Value = response.json().await?;
        ensure!(
            body["success"] == true,
            "release A failed: {}",
            body["message"]
        );
        outcome
    };
    let (events_a, outcome) = tokio::join!(stream_a, release);
    outcome?;
    report.assert_hard(
        "A 未 cancelled 且完整执行结束",
        !events_a
            .iter()
            .any(|event| event.event == "cancelled" || event.event == "error")
            && has_identity(&events_a, &response_a.session_id, &request_id_a, "end_turn")
            && sse::chunks_text(&events_a).contains(&token_a),
        sse::chunks_text(&events_a),
    );
    report.assert_hard(
        "A SSE 没有 B 的 request_id 或回答",
        no_foreign_request(&events_a, &request_id_b)
            && no_foreign_request(&events_a, &request_id_b2)
            && no_foreign_request(&events_a, &request_id_b3)
            && no_foreign_request(&events_a, &request_id_b4)
            && !sse::chunks_text(&events_a).contains(&token_b),
        format!("events={}", events_a.len()),
    );
    let listing: Value = env
        .http
        .get(format!("{entry_b}/api/computer/get-file-list"))
        .header("X-Service-Type", "userapp")
        .header("X-App-Id", app)
        .query(&[
            ("userId", user),
            ("cId", project_a.as_str()),
            ("serviceType", "userapp"),
            ("appId", app),
            ("recursive", "true"),
        ])
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .json()
        .await?;
    let listing_text = listing.to_string();
    report.assert_hard(
        "共享 workspace 同时保留 A 和 B 产物",
        listing_text.contains(&file_a) && listing_text.contains(&file_b),
        format!("A={file_a}, B={file_b}"),
    );
    let content_a = read_file(env, entry_a, app, user, &project_b, &file_a).await?;
    let content_b = read_file(env, entry_b, app, user, &project_a, &file_b).await?;
    report.assert_hard(
        "共享 workspace 的 A 和 B 产物内容正确",
        content_a == token_a && content_b == token_b,
        format!("A={content_a}, B={content_b}"),
    );

    // Omit SID on purpose: lookup must use A's binding, never app.latest_session.
    let continuation_a = make_request(
        &project_a,
        "不要调用工具。只输出本会话上一轮最终回答的 ISOLATED_A 标记，不要输出其他标记。",
        "parallel-a-resume",
    );
    let resumed_a = chat_reported(env, report, "chat_a_resume", entry_b, &continuation_a).await?;
    let (events_a2, _) = super::scenario::collect_reported(
        env,
        report,
        CollectSpec {
            phase: "stream_a_resume",
            entry: entry_a,
            sid: &resumed_a.session_id,
            duration_s: 90.0,
            last_event_id: events_a.iter().filter_map(|event| event.seq).max(),
            idle_stop: true,
        },
    )
    .await;
    report.assert_hard(
        "A 无 sid 续聊使用自身 session 和上下文",
        resumed_a.session_id == response_a.session_id
            && has_identity(
                &events_a2,
                &resumed_a.session_id,
                &request_id_a2,
                "end_turn",
            )
            && no_foreign_request(&events_a2, &request_id_b2)
            && sse::chunks_text(&events_a2).contains(&token_a)
            && !sse::chunks_text(&events_a2).contains(&token_b),
        sse::chunks_text(&events_a2),
    );
    Ok(())
}
