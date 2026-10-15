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
        // 契约 C5: 判据仅 faultCode==10——无结构化码的 fault 不算 NoSuchProcess
        //（旧 faultString 兜底已按批准需求删除）。
        assert!(!is_no_such_process(&fault));
    }

    #[test]
    fn no_such_process_classifies_only_by_fault_code() {
        // T004 反例（修复前必挂）: faultString 含 BAD_NAME/no process group
        // 不得再驱动分类; 官方码 faultCode==10 才是 NoSuchProcess。
        let fault_code_10 = parse_response(
            r#"<?xml version="1.0"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>10</int></value></member><member><name>faultString</name><value><string>no such process</string></value></member></struct></value></fault></methodResponse>"#,
        )
        .unwrap_err();
        assert!(is_no_such_process(&fault_code_10));

        let fault_code_50_with_bad_name = parse_response(
            r#"<?xml version="1.0"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>50</int></value></member><member><name>faultString</name><value><string>BAD_NAME: app-svc-web</string></value></member></struct></value></fault></methodResponse>"#,
        )
        .unwrap_err();
        assert!(
            !is_no_such_process(&fault_code_50_with_bad_name),
            "faultCode=50 按官方码不是 NoSuchProcess, 文案含 BAD_NAME 不得误判"
        );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn rpc_fault_distinguishes_completed_start_failure_from_unknown_mutation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let fault = r#"<methodResponse><fault><value><struct><member><name>faultCode</name><value><int>50</int></value></member><member><name>faultString</name><value><string>SPAWN_ERROR: app-svc-web</string></value></member></struct></value></fault></methodResponse>"#;
        let server = tokio::spawn(async move {
            for body in [fault, "malformed XML", fault, "lost read response"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let header = String::from_utf8(request).unwrap();
                let length: usize = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                stream.read_exact(&mut vec![0; length]).await.unwrap();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let client = SupervisorClient::new(socket);
        let error = client.start_process_wait("app-svc-web").await.unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::supervisor::ShutdownUnconfirmed>()
                .is_none(),
            "Completed start fault must permit the caller's stop-and-confirm recovery: {error:#}"
        );
        assert!(error.to_string().contains("SPAWN_ERROR"));
        let malformed = client.start_process_wait("app-svc-web").await.unwrap_err();
        assert!(
            malformed
                .downcast_ref::<crate::supervisor::ShutdownUnconfirmed>()
                .is_some()
        );
        let stop = client.stop_remove_group("app-svc-web").await.unwrap_err();
        assert!(
            stop.downcast_ref::<crate::supervisor::ShutdownUnconfirmed>()
                .is_none(),
            "a complete stop fault is a rejection, not a lost response"
        );
        let read = client.running_process("app-svc-worker").await.unwrap_err();
        assert!(
            read.downcast_ref::<crate::supervisor::ShutdownUnconfirmed>()
                .is_none(),
            "an observation failure must not invent an in-flight mutation"
        );
        server.await.unwrap();
    }

    #[test]
    fn splits_http_body() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(split_body(raw), Some("hello"));
        assert_eq!(split_body("garbage"), None);
    }
}

#[cfg(test)]
mod r11_tests {
    use super::*;

    fn fault(code: i64, message: &str) -> anyhow::Error {
        RpcFault(serde_json::json!({"faultCode": code, "faultString": message})).into()
    }

    /// R11 反例：typed 分类——判据仅 `faultCode == 10`（契约 C5）。
    /// 网络错误文案恰含 "no process group" 不再被误判为"不存在"；
    /// 非 10 码即使 faultString 含 BAD_NAME 也按官方码分类（措辞兜底已删除）；
    /// malformed fault（缺字段）不判为不存在。
    #[test]
    fn no_such_process_classification_is_typed() {
        // faultCode 10（BAD_NAME）→ 不存在
        assert!(is_no_such_process(&fault(10, "no process group named 'x'")));
        // faultCode 10 + 其他措辞 → 仍按官方码分类
        assert!(is_no_such_process(&fault(10, "whatever")));
        // 非 10 码但措辞含 BAD_NAME → 按官方码不是"不存在"（旧措辞兜底已删）
        assert!(!is_no_such_process(&fault(1, "BAD_NAME: foo")));
        // 非 10 码 + 无相关措辞 → 不是"不存在"
        assert!(!is_no_such_process(&fault(6, "SHUTDOWN_STATE")));
        // 反例：普通网络错误文案恰含关键词 → 不误判（typed 边界）
        let network = anyhow::Error::msg("connect failed: no process group left in pool");
        assert!(!is_no_such_process(&network));
        // malformed fault（缺 faultCode）→ 无官方码不判为不存在
        assert!(!is_no_such_process(
            &RpcFault(serde_json::json!({"faultString": "unknown"})).into()
        ));
        assert!(!is_no_such_process(
            &RpcFault(serde_json::Value::Null).into()
        ));
    }
}

#[cfg(test)]
mod process_contract_tests {
    use super::*;

    #[test]
    fn complete_state_contract_rejects_missing_unknown_and_conflicting_identity() {
        let mut row = serde_json::json!({"group":"app-pingap", "name":"app-pingap", "statename":"RUNNING", "state":20, "pid":42, "start":123});
        for (state, code) in [
            ("STOPPED", 0),
            ("STARTING", 10),
            ("RUNNING", 20),
            ("BACKOFF", 30),
            ("STOPPING", 40),
            ("EXITED", 100),
            ("FATAL", 200),
        ] {
            row["statename"] = state.into();
            row["state"] = code.into();
            let info: SupervisordProcessInfo = serde_json::from_value(row.clone()).unwrap();
            assert_eq!(info.checked_state("app-pingap").unwrap().code(), code);
        }
        for field in ["name", "statename", "state", "pid", "start"] {
            let mut invalid = serde_json::json!({"group":"app-pingap", "name":"app-pingap", "statename":"RUNNING", "state":20, "pid":42, "start":123});
            invalid.as_object_mut().unwrap().remove(field);
            let info: SupervisordProcessInfo = serde_json::from_value(invalid).unwrap();
            assert!(
                info.checked_state("app-pingap").is_err(),
                "missing {field} cannot prove running identity"
            );
        }
        row["statename"] = "UNKNOWN".into();
        row["state"] = 1000.into();
        assert!(
            serde_json::from_value::<SupervisordProcessInfo>(row.clone())
                .unwrap()
                .checked_state("app-pingap")
                .is_err()
        );
        row["statename"] = "unexpected".into();
        assert!(serde_json::from_value::<SupervisordProcessInfo>(row).is_err());
    }

    #[test]
    fn reload_config_decodes_supervisors_actual_added_changed_removed_shape() {
        let changes =
            decode_reload_changes(serde_json::json!([[["new"], ["app-pingap"], ["old"]]])).unwrap();
        assert_eq!(changes.added, ["new"]);
        assert_eq!(changes.changed, ["app-pingap"]);
        assert_eq!(changes.removed, ["old"]);
        for malformed in [
            serde_json::json!([]),
            serde_json::json!([[[]]]),
            serde_json::json!([[[1], [], []]]),
            serde_json::json!([[[], [], []], [[], [], []]]),
        ] {
            assert!(decode_reload_changes(malformed).is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn only_complete_already_added_for_the_exact_program_is_idempotent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            for body in [
                r#"<fault><value><struct><member><name>faultCode</name><value><int>90</int></value></member><member><name>faultString</name><value><string>ALREADY_ADDED</string></value></member></struct></value></fault>"#,
                r#"<params><param><value><struct><member><name>group</name><value><string>other</string></value></member><member><name>name</name><value><string>other</string></value></member><member><name>statename</name><value><string>STOPPED</string></value></member><member><name>state</name><value><int>0</int></value></member></struct></value></param></params>"#,
                r#"<fault><value><struct><member><name>faultCode</name><value><int>50</int></value></member><member><name>faultString</name><value><string>ALREADY_ADDED wording but SPAWN_ERROR code</string></value></member></struct></value></fault>"#,
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let body = format!("<methodResponse>{body}</methodResponse>");
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let client = SupervisorClient::new(socket);
        assert!(
            client
                .add_process_group("app-pingap")
                .await
                .unwrap_err()
                .to_string()
                .contains("identity mismatch")
        );
        assert!(
            client
                .add_process_group("app-pingap")
                .await
                .unwrap_err()
                .to_string()
                .contains("SPAWN_ERROR")
        );
        server.await.unwrap();
    }
}
