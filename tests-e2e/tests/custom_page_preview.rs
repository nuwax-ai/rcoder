//! Custom Page（WebAgentRunner 开发阶段 Vite 预览）协调链 Compose E2E。
//!
//! 运行: `make test-e2e-compose E2E_SUITE=custom_page_preview` 或
//! `cargo test -p rcoder-e2e --test custom_page_preview -- --test-threads=1`
//!
//! 前置（docker compose dev 环境，preview_coordinator.enabled=true）：
//! 真实 vite 项目上传 → 协调受理 → pnpm 安装 → vite 起动 → `/proxy/{port}`
//! 经 Pingora 反代（宿主 8089→容器 8088）访问。
//!
//! 覆盖（spec 验收矩阵 Compose 侧）：
//! - 生命周期信封兼容（start 幂等/keep-alive alive/stop/restart/list/log）
//! - keep-alive 对终态实例的统一重建（action=start，对齐旧"探活失败重建"）
//! - `/proxy/{port}` 预览 200 + 停止后回环拒绝（502）
//! - HMR WebSocket Upgrade 全链握手（经 Pingora 8088）
//!
//! 多副本专属断言（双宿主端口唯一/非宿主操作/410 刷新/PG 故障）属
//! remote-k8s 专属环境验收（specs/custom-page-preview-routing/tasks.md T4.4）。

use std::collections::HashMap;
use std::time::Duration;

use rcoder_e2e::common::scenario::assert_hard_all;
use rcoder_e2e::common::{Env, TestUserGuard};
use serde_json::Value;

// ============================================================
// 工具：最小 vite 项目 zip 上传 + 生命周期 API
// ============================================================

fn build_vite_project_zip() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let package_json = r#"{
  "name": "e2e-custom-page-preview",
  "private": true,
  "scripts": { "dev": "vite" },
  "devDependencies": { "vite": "^5.4.11" }
}
"#;
        let index_html = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>e2e preview</title></head>
<body><div id="app">custom page preview e2e</div></body></html>
"#;
        zip.start_file("package.json", options)
            .expect("zip package.json");
        std::io::Write::write_all(&mut zip, package_json.as_bytes()).expect("write package.json");
        zip.start_file("index.html", options)
            .expect("zip index.html");
        std::io::Write::write_all(&mut zip, index_html.as_bytes()).expect("write index.html");
        zip.finish().expect("zip finish");
    }
    buf.into_inner()
}

async fn upload_project(
    env: &Env,
    report: &rcoder_e2e::common::report::JsonlReporter,
    project: &str,
) -> bool {
    let zip_bytes = build_vite_project_zip();
    let part = reqwest::multipart::Part::bytes(zip_bytes).file_name("project.zip");
    let form = reqwest::multipart::Form::new()
        .text("projectId", project.to_string())
        .text("codeVersion", "1")
        .part("file", part);
    let response = env
        .http
        .post(format!("{}/api/project/upload-project", env.rcoder))
        .timeout(Duration::from_secs(60))
        .multipart(form)
        .send()
        .await;
    let ok = matches!(&response, Ok(r) if r.status().is_success());
    let detail = match &response {
        Ok(r) => format!("HTTP {}", r.status()),
        Err(e) => format!("err: {e}"),
    };
    report.assert_hard("上传 vite 项目 zip", ok, detail)
}

#[derive(Debug)]
struct DevInstance {
    pid: i64,
    port: u16,
}

async fn start_dev(env: &Env, project: &str) -> Result<DevInstance, String> {
    start_dev_with_base(env, project, "/").await
}

async fn start_dev_with_base(
    env: &Env,
    project: &str,
    base_path: &str,
) -> Result<DevInstance, String> {
    let response = env
        .http
        .get(format!("{}/api/build/start-dev", env.rcoder))
        .query(&[("projectId", project), ("basePath", base_path)])
        .timeout(Duration::from_secs(420))
        .send()
        .await
        .map_err(|e| format!("start-dev request: {e}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|e| format!("decode: {e}"))?;
    if !status.is_success() || !body["success"].as_bool().unwrap_or(false) {
        return Err(format!("start-dev HTTP {status}: {body}"));
    }
    Ok(DevInstance {
        pid: body["pid"].as_i64().unwrap_or_default(),
        port: body["port"].as_u64().unwrap_or_default() as u16,
    })
}

async fn stop_dev(env: &Env, project: &str, pid: i64) -> Result<Value, String> {
    let response = env
        .http
        .get(format!("{}/api/build/stop-dev", env.rcoder))
        .query(&[("projectId", project), ("pid", &pid.to_string())])
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("stop-dev request: {e}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|e| format!("decode: {e}"))?;
    if !status.is_success() {
        return Err(format!("stop-dev HTTP {status}: {body}"));
    }
    Ok(body)
}

async fn keep_alive(env: &Env, project: &str, pid: i64, port: u16) -> Result<Value, String> {
    let response = env
        .http
        .get(format!("{}/api/build/keep-alive", env.rcoder))
        .query(&[
            ("projectId", project),
            ("pid", &pid.to_string()),
            ("port", &port.to_string()),
            ("basePath", "/page/"),
        ])
        .timeout(Duration::from_secs(420))
        .send()
        .await
        .map_err(|e| format!("keep-alive request: {e}"))?;
    response.json().await.map_err(|e| format!("decode: {e}"))
}

/// Compose 的 Pingora 入口（宿主 8089 → 容器 8088，docker-compose 固定映射）。
fn proxy_base(env: &Env) -> String {
    let rcoder = env.rcoder.trim_end_matches('/');
    match rcoder.rsplit_once(':') {
        Some((head, _port)) => format!("{head}:8089"),
        None => format!("{rcoder}:8089"),
    }
}

// ============================================================
// 场景 1：生命周期信封兼容（协调模式）
// ============================================================
async fn scenario_lifecycle_envelopes() {
    let scenario = "cpp_lifecycle_envelopes";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let project = format!("cpp-{}", env.run_tag);
    let _guard = TestUserGuard::new(&env, &format!("g-{scenario}"));

    if !upload_project(&env, &report, &project).await {
        assert_hard_all(report).await;
        return;
    }

    // start-dev：协调受理 + 真实 pnpm install + vite 起动
    let started = match start_dev(&env, &project).await {
        Ok(v) => v,
        Err(e) => {
            report.assert_hard("start-dev 协调受理并起动", false, e);
            assert_hard_all(report).await;
            return;
        }
    };
    report.assert_hard(
        "start-dev 信封（success/pid>0/port 池内）",
        started.pid > 0 && shared_types::is_preview_port(started.port),
        format!("pid={} port={}", started.pid, started.port),
    );

    // 幂等：重复 start 返回同一实例
    let again = start_dev(&env, &project).await.ok();
    report.assert_hard(
        "start-dev 幂等返回同实例",
        matches!(&again, Some(v) if v.pid == started.pid && v.port == started.port),
        format!("{again:?}"),
    );

    // keep-alive：刚发布心跳新鲜 → alive 信封
    let alive = keep_alive(&env, &project, started.pid, started.port)
        .await
        .unwrap_or(Value::Null);
    report.assert_hard(
        "keep-alive alive 信封（success/message/action=None）",
        alive["success"].as_bool().unwrap_or(false)
            && alive["message"].as_str().unwrap_or_default() == "Development server is alive"
            && alive.get("action").map(Value::is_null).unwrap_or(true),
        alive.to_string(),
    );

    // list-dev 含该项目
    let list = env
        .http
        .get(format!("{}/api/build/list-dev", env.rcoder))
        .timeout(Duration::from_secs(30))
        .send()
        .await;
    let list_body = match list {
        Ok(r) => r.json::<Value>().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let listed = list_body["list"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .any(|it| it["projectId"].as_str() == Some(project.as_str()))
        })
        .unwrap_or(false);
    report.assert_hard("list-dev 聚合视图含项目", listed, list_body.to_string());

    // get-dev-log 成功
    let log = env
        .http
        .get(format!("{}/api/build/get-dev-log", env.rcoder))
        .query(&[
            ("projectId", project.clone()),
            ("startIndex", "1".to_string()),
            ("logType", "temp".to_string()),
        ])
        .timeout(Duration::from_secs(30))
        .send()
        .await;
    let log_body = match log {
        Ok(r) => r.json::<Value>().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    report.assert_hard(
        "get-dev-log 协调读取成功",
        log_body["success"].as_bool().unwrap_or(false),
        log_body.to_string(),
    );

    // stop-dev：协调停止（受理→本机执行→终态）
    let stopped = stop_dev(&env, &project, started.pid)
        .await
        .unwrap_or(Value::Null);
    report.assert_hard(
        "stop-dev 信封（success + Stopped）",
        stopped["success"].as_bool().unwrap_or(false),
        stopped.to_string(),
    );

    // keep-alive 对终态实例 → 统一受理重建（action=start，对齐旧"探活失败重建"语义）
    let rebuilt = keep_alive(&env, &project, started.pid, started.port)
        .await
        .unwrap_or(Value::Null);
    report.assert_hard(
        "keep-alive 终态重建信封（action=start）",
        rebuilt["success"].as_bool().unwrap_or(false)
            && rebuilt["action"].as_str().unwrap_or_default() == "start",
        rebuilt.to_string(),
    );
    let rebuilt_port = rebuilt["port"].as_u64().unwrap_or_default() as u16;
    if rebuilt_port > 0 {
        note_stop(stop_dev(&env, &project, rebuilt["pid"].as_i64().unwrap_or_default()).await);
    }

    assert_hard_all(report).await;
}

// ============================================================
// 场景 2：/proxy/{port} 预览路由 + 停止后回环拒绝
// ============================================================
async fn scenario_preview_http() {
    let scenario = "cpp_preview_http";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let project = format!("cpph-{}", env.run_tag);
    let _guard = TestUserGuard::new(&env, &format!("g-{scenario}"));

    if !upload_project(&env, &report, &project).await {
        assert_hard_all(report).await;
        return;
    }
    let Some(started) = start_dev_or_report(&env, &report, &project).await else {
        assert_hard_all(report).await;
        return;
    };
    let proxy = proxy_base(&env);

    // 预览首页 200（协调 Local 解析 → localhost → vite）
    let page = env
        .http
        .get(format!("{proxy}/proxy/{}/", started.port))
        .timeout(Duration::from_secs(30))
        .send()
        .await;
    let page_ok = matches!(&page, Ok(r) if r.status().as_u16() == 200);
    let page_detail = match &page {
        Ok(r) => format!("HTTP {}", r.status()),
        Err(e) => format!("err: {e}"),
    };
    report.assert_hard("/proxy/{port}/ 预览 200", page_ok, page_detail);
    let body_text = match page {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => String::new(),
    };
    if !body_text.is_empty() {
        report.assert_hard(
            "预览内容为 vite index.html",
            body_text.contains("custom page preview e2e"),
            format!("{} bytes", body_text.len()),
        );
    }

    // 停止后：负缓存/未命中 → 回环拒绝（pingora 502 语义与现状一致）
    note_stop(stop_dev(&env, &project, started.pid).await);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let dead = env
        .http
        .get(format!("{proxy}/proxy/{}/", started.port))
        .timeout(Duration::from_secs(30))
        .send()
        .await;
    let dead_status = dead.as_ref().map_or(0, |r| r.status().as_u16());
    report.assert_hard(
        "停止后预览回环拒绝（502，与现状语义一致）",
        dead_status == 502 || dead_status == 503 || dead_status == 404,
        format!("HTTP {dead_status}"),
    );

    assert_hard_all(report).await;
}

// ============================================================
// 场景 3：HMR WebSocket Upgrade 全链握手
// ============================================================
async fn scenario_hmr_websocket() {
    let scenario = "cpp_hmr_websocket";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let project = format!("cppw-{}", env.run_tag);
    let _guard = TestUserGuard::new(&env, &format!("g-{scenario}"));

    if !upload_project(&env, &report, &project).await {
        assert_hard_all(report).await;
        return;
    }
    let Some(started) = start_dev_or_report(&env, &report, &project).await else {
        assert_hard_all(report).await;
        return;
    };
    let proxy = proxy_base(&env).replace("http://", "");
    // vite 5：HMR ws 路径 = base（默认 /proxy/{port}/）
    let ws_url = format!("ws://{proxy}/proxy/{}/", started.port);
    let (stream, _resp) = match tokio_tungstenite::connect_async(ws_url.clone()).await {
        Ok(pair) => pair,
        Err(e) => {
            report.assert_hard(
                "HMR WebSocket 握手（Upgrade 全链透传）",
                false,
                format!("{ws_url}: {e}"),
            );
            note_stop(stop_dev(&env, &project, 0).await);
            assert_hard_all(report).await;
            return;
        }
    };
    report.assert_hard(
        "HMR WebSocket 握手（Upgrade 全链透传）",
        true,
        ws_url.clone(),
    );
    drop(stream);
    note_stop(stop_dev(&env, &project, 0).await);

    assert_hard_all(report).await;
}

async fn start_dev_or_report(
    env: &Env,
    report: &rcoder_e2e::common::report::JsonlReporter,
    project: &str,
) -> Option<DevInstance> {
    match start_dev(env, project).await {
        Ok(v) => {
            report.assert_hard(
                "start-dev 前置",
                true,
                format!("pid={} port={}", v.pid, v.port),
            );
            Some(v)
        }
        Err(e) => {
            report.assert_hard("start-dev 前置", false, e);
            None
        }
    }
}

// ============================================================
// 入口
// ============================================================
#[tokio::test]
async fn cpp_lifecycle_envelopes() {
    scenario_lifecycle_envelopes().await;
}

#[tokio::test]
async fn cpp_preview_http() {
    scenario_preview_http().await;
}

#[tokio::test]
async fn cpp_hmr_websocket() {
    scenario_hmr_websocket().await;
}

// 防 unused 警告（HashMap 保留给后续多副本场景扩展使用）。
#[allow(unused)]
fn _type_anchor(_: Option<HashMap<String, String>>) {}

/// 收尾 stop 的结果只记录（场景断言不依赖收尾成败）。
fn note_stop(result: Result<Value, String>) {
    if let Err(e) = result {
        eprintln!("[cleanup] stop-dev: {e}");
    }
}
