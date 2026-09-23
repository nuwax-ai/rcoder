//! Physical Docker identities and data retention, independent of LLM credentials.
use rcoder_e2e::common::scenario::assert_hard_all;
use rcoder_e2e::common::{Env, TestUserGuard};

#[tokio::test]
async fn docker_deletion_identity_contract() {
    let scenario = "docker_deletion_identity_contract";
    let Some((_env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let directory = std::env::var_os("E2E_REPORT_DIR").expect("run via strict E2E entry");
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/docker_lifecycle_contract.py"
        ))
        .status();
    report.assert_hard(
        "Docker lifecycle process completed",
        result.is_ok_and(|s| s.success()),
        "see docker-lifecycle artifacts".into(),
    );
    let path = std::path::PathBuf::from(directory).join("docker-lifecycle/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("Docker assertions file"))
            .expect("Docker assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "Docker lifecycle contract failed");
}

// ============================================================
// 场景：computer ttyd 终端 query 契约（?service_type= / ?cwd=）——
//       浏览器原生 WS 无法设自定义 header，query 是业务场景与初始目录的
//       唯一客户端载体。链路：ws → pingora(:8089) → ws_terminal(17681) →
//       ttyd(7681)，agent_runner percent-encode + wrapper urldecode 成对
//       传输（含空格路径的运行时实证）。容器经 pod/ensure 供给，无 LLM
//       依赖；compose 专属（ttyd 路由按容器 IP 定位）。
// =================================================-----------
#[tokio::test]
async fn computer_ttyd_cwd_query() {
    let scenario = "computer_ttyd_cwd_query";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    if !env.k8s_ssh.is_empty() {
        eprintln!("[{scenario}] K8s 模式跳过（compose 专属场景）");
        return;
    }
    let user = env.scoped_user("ttyd-cwd");
    let _guard = TestUserGuard::new(&env, &user);
    let project = format!("np-{}", env.run_tag);

    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use std::time::{Duration, Instant};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    // 1) 供给 computer 容器（幂等，无 LLM 依赖）
    let mut ensured = false;
    for _ in 0..5 {
        let resp = env
            .http
            .post(format!("{}/computer/pod/ensure", env.rcoder))
            .timeout(Duration::from_secs(300))
            .json(&json!({ "user_id": user, "project_id": project }))
            .send()
            .await;
        if let Ok(resp) = resp {
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            if status.is_success() && body["code"].as_str() == Some("0000") {
                ensured = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    report.assert_hard(
        "computer 容器 ensure（pod/ensure 无 LLM 供给）",
        ensured,
        "5 次重试后仍未 running".into(),
    );
    if !ensured {
        assert_hard_all(report).await;
        return;
    }

    let pingora = std::env::var("E2E_PINGORA_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8089".to_owned());

    // 单腿：握手（60s 重试窗覆盖容器冷启动 + ws_terminal 等 ttyd 就绪）
    // → 发首帧/命令 → 在 OUTPUT 帧里等期望串。
    async fn run_terminal_leg(
        ws_url: &str,
        command: &str,
        expect: &str,
        handshake_window_s: u64,
    ) -> (bool, bool, String) {
        let mut ws = None;
        let deadline = Instant::now() + Duration::from_secs(handshake_window_s);
        while Instant::now() < deadline {
            let Ok(mut req) = ws_url.into_client_request() else {
                break;
            };
            req.headers_mut()
                .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("tty"));
            match connect_async(req).await {
                Ok((stream, _)) => {
                    ws = Some(stream);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
        let Some(mut ws) = ws else {
            return (false, false, "握手窗口耗尽".into());
        };
        let mut sample = String::new();
        ws.send(Message::Text(r#"{"columns":80,"rows":24}"#.into()))
            .await
            .ok();
        tokio::time::sleep(Duration::from_secs(2)).await;
        ws.send(Message::Binary(format!("0{command}\n").into_bytes().into()))
            .await
            .ok();
        let read_deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < read_deadline {
            let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await
            else {
                continue;
            };
            let Message::Binary(data) = msg else {
                continue;
            };
            if data.first() != Some(&b'0') {
                continue;
            }
            let text = strip_ansi(&String::from_utf8_lossy(&data[1..]));
            if text.contains(expect) {
                ws.close(None).await.ok();
                return (true, true, text.chars().take(120).collect());
            }
            if sample.is_empty() {
                sample = text.chars().take(120).collect();
            }
        }
        ws.close(None).await.ok();
        (true, false, sample)
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    // 2) 默认链路开终端，预置目录：normalProject/{project} 与含空格目录。
    //    $((6*7)) 只在真实输出出现 42（命令回显是字面量），以它确认 mkdir 执行完成
    let base =
        format!("{pingora}/computer/ttyd/{user}/{project}/ws").replacen("http://", "ws://", 1);
    let (ok_hs, ok_mkdir, detail) = run_terminal_leg(
        &base,
        &format!(
            "mkdir -p /home/user/normalProject/{project} '/home/user/ttyd cwd dir' && echo TTYD_MKDIR_$((6*7))"
        ),
        "TTYD_MKDIR_42",
        60,
    )
    .await;
    report.assert_hard(
        "终端 ws 握手（默认链路，子协议 tty）",
        ok_hs,
        detail.clone(),
    );
    report.assert_hard(
        "mkdir 预置目录执行完成（TTYD_MKDIR_42 回显）",
        ok_hs && ok_mkdir,
        detail,
    );
    if !ok_mkdir {
        assert_hard_all(report).await;
        return;
    }

    // 3) ?service_type=computer-normal-project → cwd 落 normalProject 前缀
    let normal_url = format!("{base}?service_type=computer-normal-project");
    let (hs, hit, detail) = run_terminal_leg(
        &normal_url,
        "pwd",
        &format!("/home/user/normalProject/{project}"),
        30,
    )
    .await;
    report.assert_hard(
        "?service_type=computer-normal-project → cwd=/home/user/normalProject/{project}（pwd 回显）",
        hs && hit,
        detail,
    );

    // 4) ?cwd= 显式含空格目录（%20 传输；encode/decode 全链运行时实证）
    let explicit_url = format!("{base}?cwd=%2Fhome%2Fuser%2Fttyd%20cwd%20dir");
    let (hs, hit, detail) =
        run_terminal_leg(&explicit_url, "pwd", "/home/user/ttyd cwd dir", 30).await;
    report.assert_hard(
        "?cwd= 显式初始目录含空格（%20 传输，pwd 回显）",
        hs && hit,
        detail,
    );

    // 5) 反例：非法 service_type → 握手 400 拒绝（fail fast，不落默认目录）
    let bad_url = format!("{base}?service_type=not-a-type");
    let mut rejected = false;
    if let Ok(mut req) = bad_url.clone().into_client_request() {
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("tty"));
        rejected = connect_async(req).await.is_err();
    }
    report.assert_hard(
        "?service_type= 非法值 → 握手 400 拒绝",
        rejected,
        format!("url={bad_url}"),
    );

    assert_hard_all(report).await;
}
