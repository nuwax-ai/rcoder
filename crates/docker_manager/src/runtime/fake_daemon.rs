//! 测试用的假 Docker daemon：按剧本逐步响应，并记下每个请求的正文。
//!
//! 真实的 Bollard 客户端对着它发请求。容器绑定了什么、带了哪些环境变量和 label，都在创建
//! 请求的正文里，可以确定性地断言，不用靠 sleep 碰运气，也不依赖时序。

use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// 一步预设响应
pub(crate) struct Step {
    pub(crate) method: &'static str,
    pub(crate) contains: String,
    pub(crate) status: u16,
    pub(crate) body: Value,
    /// 不应答，直接断开连接：模拟应答丢失
    pub(crate) hang_up: bool,
    /// 收下请求之后一直不应答：模拟 daemon 迟迟不回
    pub(crate) stall: bool,
}

pub(crate) fn step(
    method: &'static str,
    contains: impl Into<String>,
    status: u16,
    body: Value,
) -> Step {
    Step {
        method,
        contains: contains.into(),
        status,
        body,
        hang_up: false,
        stall: false,
    }
}

/// 应答丢失
pub(crate) fn lost_reply(method: &'static str, contains: impl Into<String>) -> Step {
    Step {
        method,
        contains: contains.into(),
        status: 0,
        body: Value::Null,
        hang_up: true,
        stall: false,
    }
}

/// 收下请求之后一直不应答
pub(crate) fn stalled(method: &'static str, contains: impl Into<String>) -> Step {
    Step {
        method,
        contains: contains.into(),
        status: 0,
        body: Value::Null,
        hang_up: false,
        stall: true,
    }
}

pub(crate) fn no_such_container() -> Value {
    json!({"message": "No such container"})
}

pub(crate) fn image_inspect() -> Value {
    serde_json::to_value(bollard::models::ImageInspect::default()).unwrap()
}

pub(crate) fn daemon_info() -> Value {
    let info = bollard::models::SystemInfo {
        id: Some("restart-fence-daemon".into()),
        ..Default::default()
    };
    serde_json::to_value(info).unwrap()
}

pub(crate) type RequestHook = Arc<dyn Fn(&str) + Send + Sync>;

pub(crate) struct ScriptedDaemon {
    pub(crate) address: std::net::SocketAddr,
    /// 请求行，按到达顺序
    requests: Arc<Mutex<Vec<String>>>,
    /// 请求正文，与 `requests` 一一对应
    bodies: Arc<Mutex<Vec<String>>>,
    mismatches: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl ScriptedDaemon {
    /// 请求必须逐步命中剧本；任何偏离都记录下来
    pub(crate) fn assert_followed(&self, expected_requests: usize) {
        let mismatches = self.mismatches.lock().unwrap();
        assert!(mismatches.is_empty(), "unexpected requests: {mismatches:?}");
        assert_eq!(
            self.requests.lock().unwrap().len(),
            expected_requests,
            "every scripted request must have been made"
        );
    }

    /// 到目前为止的请求行，按到达顺序
    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// 一个请求都没到过
    pub(crate) fn assert_no_request(&self) {
        assert!(
            self.requests.lock().unwrap().is_empty(),
            "no request must reach Docker: {:?}",
            self.requests.lock().unwrap()
        );
    }

    /// 第 `index` 个请求的正文
    pub(crate) fn body(&self, index: usize) -> Value {
        let bodies = self.bodies.lock().unwrap();
        serde_json::from_str(&bodies[index])
            .unwrap_or_else(|error| panic!("request {index} has no JSON body: {error}"))
    }
}

pub(crate) async fn scripted(steps: Vec<Step>) -> ScriptedDaemon {
    scripted_with_hook(steps, Arc::new(|_: &str| {})).await
}

/// 假 daemon 按剧本应答，并在每个请求到达、应答之前调用 `hook`（传入请求行）
pub(crate) async fn scripted_with_hook(steps: Vec<Step>, hook: RequestHook) -> ScriptedDaemon {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut script = std::collections::VecDeque::new();
    script.extend(steps);
    let script = Arc::new(Mutex::new(script));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let mismatches = Arc::new(Mutex::new(Vec::new()));
    let (recorded, recorded_bodies, failed) = (
        Arc::clone(&requests),
        Arc::clone(&bodies),
        Arc::clone(&mismatches),
    );
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            // 先读完请求头和请求体再应答：客户端可能还在写请求体，过早关连接会让它读不到应答
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).await.is_ok_and(|n| n == 1)
            {
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let content_length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0_u8; content_length];
            if stream.read_exact(&mut body).await.is_err() {
                continue;
            }

            let request = head.lines().next().unwrap().to_owned();
            let expected = script.lock().unwrap().pop_front();
            let matched = expected.as_ref().is_some_and(|step| {
                request.starts_with(step.method) && request.contains(&step.contains)
            });
            if !matched {
                failed.lock().unwrap().push(format!(
                    "{request} (next step: {:?})",
                    expected
                        .as_ref()
                        .map(|step| (step.method, step.contains.clone()))
                ));
            }
            let step = expected.filter(|_| matched);
            recorded.lock().unwrap().push(request.clone());
            recorded_bodies
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            hook(&request);
            if step.as_ref().is_some_and(|step| step.stall) {
                std::future::pending::<()>().await;
            }
            if step.as_ref().is_some_and(|step| step.hang_up) {
                continue;
            }
            let body = step.as_ref().map_or_else(
                || no_such_container().to_string(),
                |step| step.body.to_string(),
            );
            let status = step.as_ref().map_or(500, |step| step.status);
            let response = format!(
                "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _written = stream.write_all(response.as_bytes()).await;
        }
    });
    ScriptedDaemon {
        address,
        requests,
        bodies,
        mismatches,
        server,
    }
}
