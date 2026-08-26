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

/// supervisord 控制客户端（unix socket）。
pub(crate) struct SupervisorClient {
    socket: std::path::PathBuf,
}

impl SupervisorClient {
    pub(crate) fn new(socket: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// 连通性探测（supervisor.getVersion → 版本串）。
    pub(crate) async fn ping(&self) -> Result<String> {
        let value = self.call("supervisor.getVersion", &[]).await?;
        value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("getVersion returned non-string: {value}"))
    }

    /// reloadConfig（即 supervisorctl reread）：返回变更组列表
    /// `[[name, change_code, description], ...]`。
    pub(crate) async fn reload_config(&self) -> Result<Vec<(String, String)>> {
        let value = self.call("supervisor.reloadConfig", &[]).await?;
        let mut changes = Vec::new();
        if let Some(items) = value.as_array() {
            // 响应结构：[[[["name","code","desc"],...]]]（外层 struct 含 added/changed/removed）
            for item in items {
                if let Some(triples) = item.as_array() {
                    for triple in triples {
                        if let Some(fields) = triple.as_array()
                            && fields.len() >= 2
                            && let (Some(name), Some(code)) =
                                (fields[0].as_str(), fields[1].as_str())
                        {
                            changes.push((name.to_string(), code.to_string()));
                        }
                    }
                }
            }
        }
        Ok(changes)
    }

    /// addProcessGroup：把 reloadConfig 发现的新组加入托管（返回 false=已存在）。
    pub(crate) async fn add_process_group(&self, group: &str) -> Result<bool> {
        let value = self
            .call("supervisor.addProcessGroup", &[group.into()])
            .await?;
        Ok(value.as_bool().unwrap_or(false))
    }

    /// stopProcessGroup + removeProcessGroup：停并摘除托管（幂等，组不存在容忍）。
    pub(crate) async fn stop_remove_group(&self, group: &str) -> Result<()> {
        match self
            .call("supervisor.stopProcessGroup", &[group.into()])
            .await
        {
            Ok(_) => {}
            // 组不存在（已移除）→ 跳过 remove
            Err(e) if is_no_such_process(&e) => return Ok(()),
            Err(e) => return Err(e),
        }
        match self
            .call("supervisor.removeProcessGroup", &[group.into()])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_no_such_process(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// startProcess（wait=true：等待 startsecs 通过才返回——编排的顺序控制点）。
    pub(crate) async fn start_process_wait(&self, name: &str) -> Result<()> {
        self.call("supervisor.startProcess", &[name.into(), true.into()])
            .await
            .map(|_| ())
    }

    /// getAllProcessInfo：每组一行状态（name/group/statename/description/...）。
    pub(crate) async fn get_all_process_info(&self) -> Result<Vec<serde_json::Value>> {
        let value = self.call("supervisor.getAllProcessInfo", &[]).await?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// 执行一次 XML-RPC 调用（HTTP/1.1 POST /RPC2 over unix socket）。
    async fn call(&self, method: &str, params: &[serde_json::Value]) -> Result<serde_json::Value> {
        let body = build_request(method, params);
        let request = format!(
            "POST /RPC2 HTTP/1.1\r\nHost: supervisor\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ) + &body;

        let mut stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect supervisord socket {}", self.socket.display()))?;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;

        let text = String::from_utf8_lossy(&response);
        let xml = split_body(&text).ok_or_else(|| {
            anyhow!(
                "malformed XML-RPC response (no body split): {}",
                truncate(&text, 200)
            )
        })?;
        parse_response(xml)
    }
}

/// 按标签名取首个子元素（roxmltree Children 无 find_element 便捷方法）。
fn child_elem<'a>(node: Node<'a, '_>, name: &str) -> Option<Node<'a, 'a>> {
    node.children()
        .find(|e| e.is_element() && e.has_tag_name(name))
}

fn is_no_such_process(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    // supervisord 对不存在组的 stop/remove 返回 faultCode 10 ("SHUTDOWN_STATE" 10?
    // NO: 10=SHUTDOWN_STATE；不存在组= faultString "no process group named 'x'")
    text.contains("no process group") || text.contains("BAD_NAME")
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
        return Err(anyhow!("supervisord fault: {detail}"));
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
mod tests {
    use super::*;

    #[test]
    fn request_envelope_shape() {
        let xml = build_request(
            "supervisor.startProcess",
            &["app-svc-web".into(), true.into()],
        );
        assert!(xml.contains("<methodName>supervisor.startProcess</methodName>"));
        assert!(xml.contains("<string>app-svc-web</string>"));
        assert!(xml.contains("<boolean>1</boolean>"));
        assert!(!xml.contains('&'));
    }

    #[test]
    fn escapes_xml_specials_in_params() {
        let xml = build_request("m", &["a<b&c".into()]);
        assert!(xml.contains("a&lt;b&amp;c"));
    }

    #[test]
    fn parses_scalar_and_fault() {
        let ok = parse_response(
            r#"<?xml version="1.0"?><methodResponse><params><param><value><string>4.2.5</string></value></param></params></methodResponse>"#,
        )
        .unwrap();
        assert_eq!(ok, serde_json::Value::String("4.2.5".into()));

        let fault = parse_response(
            r#"<?xml version="1.0"?><methodResponse><fault><value><struct><member><name>faultString</name><value><string>no process group named 'x'</string></value></member></struct></value></fault></methodResponse>"#,
        )
        .unwrap_err();
        assert!(is_no_such_process(&fault));
    }

    #[test]
    fn parses_nested_array_of_structs() {
        // getAllProcessInfo 形状
        let xml = r#"<?xml version="1.0"?>
<methodResponse><params><param><value><array><data>
<value><struct>
  <member><name>name</name><value><string>app-svc-web</string></value></member>
  <member><name>statename</name><value><string>RUNNING</string></value></member>
  <member><name>pid</name><value><int>42</int></value></member>
  <member><name>exitstatus</name><value><int>0</int></value></member>
</struct></value>
</data></array></value></param></params></methodResponse>"#;
        let value = parse_response(xml).unwrap();
        let arr = value.as_array().unwrap();
        assert_eq!(arr[0]["name"], "app-svc-web");
        assert_eq!(arr[0]["pid"], 42);
    }

    #[test]
    fn splits_http_body() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(split_body(raw), Some("hello"));
        assert_eq!(split_body("garbage"), None);
    }
}
