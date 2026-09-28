use super::*;

/// ports label 编解码往返：Http/Tcp 类型精确保留、编码按端口排序稳定。
#[test]
fn ports_label_roundtrip_preserves_expose_types() {
    let ports = vec![
        AppPortSpec {
            name: "http".into(),
            port: 8080,
            expose_type: ExposeType::Http,
            strip_prefix: Some(true),
        },
        AppPortSpec {
            name: "db".into(),
            port: 5432,
            expose_type: ExposeType::Tcp,
            strip_prefix: None,
        },
    ];
    let encoded = encode_ports_label(&ports);
    assert_eq!(encoded, "5432:tcp,8080:http");
    let parsed = parse_ports_label(&encoded);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].port, 5432);
    assert!(matches!(parsed[0].expose_type, ExposeType::Tcp));
    assert_eq!(parsed[1].port, 8080);
    assert!(matches!(parsed[1].expose_type, ExposeType::Http));
}

/// 解码容错：非法条目（非数字端口/缺类型）跳过，其余保留。
#[test]
fn ports_label_parse_skips_invalid_entries() {
    let parsed = parse_ports_label("8080:http,abc:tcp,:http,9090:tcp");
    assert_eq!(parsed.len(), 2);
    assert!(parsed.iter().all(|p| p.port == 8080 || p.port == 9090));
}

/// dev 树 identifier 扫描：跨 uid 去重、跳过 data/logs/agent-store 兄弟目录、
/// 排序稳定；树不存在返回空（幂等）。
#[tokio::test]
async fn dev_workspace_scan_collects_apps_across_uids_skipping_siblings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    // 两个 uid 各持有不同 app；data/logs/agent-store 下也有按 app_id 键的目录，
    // 但它们是兄弟段不是 uid，不得进入结果；文件与非 dev 内容忽略
    for (uid, app) in [("u1", "app-a"), ("u2", "app-b"), ("u1", "app-c")] {
        std::fs::create_dir_all(root.join(uid).join(app)).expect("mkdir app");
    }
    for sibling in ["data", "logs", "agent-store"] {
        std::fs::create_dir_all(root.join("u1").join(sibling).join("app-a"))
            .expect("mkdir sibling");
    }
    std::fs::write(root.join("u1").join("notes.txt"), "").expect("write file");

    let ids = crate::runtime::docker_workspace::scan_dev_workspace_identifiers(root)
        .await
        .expect("scan");
    assert_eq!(ids, vec!["app-a", "app-b", "app-c"]);
}

#[tokio::test]
async fn dev_workspace_scan_empty_when_tree_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ids =
        crate::runtime::docker_workspace::scan_dev_workspace_identifiers(&dir.path().join("dev"))
            .await
            .expect("scan");
    assert!(ids.is_empty());
}
