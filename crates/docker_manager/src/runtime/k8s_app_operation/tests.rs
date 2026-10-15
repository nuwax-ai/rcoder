use super::*;

#[test]
fn operation_names_separate_prefix_shaped_app_ids_across_families() {
    assert_ne!(
        operation_name("builder-foo", &ServiceType::Userapp).expect("prod"),
        operation_name("foo", &ServiceType::UserappBuilder).expect("builder")
    );
    assert_eq!(
        operation_name("builder-foo", &ServiceType::Userapp).expect("prod"),
        "rcoder-operation-prod-builder-foo"
    );
}

/// D2 判定锁：身份不符的活 Lease 是被接管（对旧回执是已释放态），
/// 不是 release 错误——修复前它是字符串错误，喂进清理链的 `?` 造成
/// release/forget 永久重试与 completed→fence 翻转。
#[test]
fn identity_mismatch_means_taken_over_not_release_error() {
    let mut object = sample_lease(Some(0));
    object.metadata.uid = Some("uid-a".into());
    object.metadata.annotations =
        Some([("rcoder.io/operation-id".to_string(), "token-a".to_string())].into());
    assert!(!lease_taken_over(&object, "uid-a", "token-a"));
    assert!(
        lease_taken_over(&object, "uid-b", "token-a"),
        "uid 换=被接管"
    );
    assert!(
        lease_taken_over(&object, "uid-a", "token-b"),
        "token 换=被接管"
    );
    assert!(lease_taken_over(&object, "uid-a", ""), "空 token 不匹配");
    object.metadata.uid = None;
    assert!(
        lease_taken_over(&object, "uid-a", "token-a"),
        "缺 uid=被接管"
    );
}

/// 固定观察时刻，保证年龄构造与判定的基准一致（两次 now() 的微秒差
/// 会让边界用例抖动）。
fn fixed_now() -> k8s_openapi::jiff::Timestamp {
    "2026-09-21T12:00:00Z"
        .parse::<k8s_openapi::jiff::Timestamp>()
        .expect("fixed timestamp")
}

fn sample_lease(renew_age_secs: Option<i64>) -> Lease {
    let renew = renew_age_secs.map(|age| {
        let stamp = fixed_now() - k8s_openapi::jiff::SignedDuration::from_secs(age);
        k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(stamp)
    });
    Lease {
        metadata: kube::api::ObjectMeta {
            name: Some("rcoder-operation-prod-app".into()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some("executor-one:operation-one".into()),
            lease_duration_seconds: Some(LEASE_TTL_SECONDS),
            acquire_time: None,
            renew_time: renew,
            lease_transitions: Some(0),
            ..Default::default()
        }),
    }
}

/// 过期判据：新鲜续租未过期；超过 TTL 过期；缺 renewTime/spec 过期。
/// 修复前的行为（永不过期、operator recovery）正是事故根源。
#[test]
fn lease_expiry_requires_stale_renewal() {
    let now = fixed_now();
    assert!(!lease_expired_at(now, &sample_lease(Some(10))));
    assert!(!lease_expired_at(
        now,
        &sample_lease(Some(LEASE_TTL_SECONDS as i64))
    ));
    assert!(lease_expired_at(
        now,
        &sample_lease(Some(i64::from(LEASE_TTL_SECONDS) + 1))
    ));
    assert!(lease_expired_at(now, &sample_lease(None)));
    let mut missing_spec = sample_lease(Some(0));
    missing_spec.spec = None;
    assert!(lease_expired_at(now, &missing_spec));
}

/// TTL 覆盖缺省：对象未携带 leaseDurationSeconds 时用本实现常量判定。
#[test]
fn lease_expiry_falls_back_to_configured_ttl() {
    let mut object = sample_lease(Some(30));
    object.spec.as_mut().expect("spec").lease_duration_seconds = None;
    let now = fixed_now();
    assert!(!lease_expired_at(now, &object));
    let mut stale = sample_lease(Some(61));
    stale.spec.as_mut().expect("spec").lease_duration_seconds = None;
    assert!(lease_expired_at(now, &stale));
}

/// 持有者令牌优先级：durable operation-id 优先于 legacy 注解。
#[test]
fn holder_token_prefers_durable_operation_id() {
    let mut object = sample_lease(Some(0));
    object.metadata.annotations = Some(
        [
            ("rcoder.io/operation-id".to_string(), "op-1".to_string()),
            (
                "rcoder.io/legacy-operation-id".to_string(),
                "legacy-1".to_string(),
            ),
        ]
        .into(),
    );
    assert_eq!(holder_token(&object).as_deref(), Some("op-1"));
    object.metadata.annotations = Some(
        [(
            "rcoder.io/legacy-operation-id".to_string(),
            "legacy-1".to_string(),
        )]
        .into(),
    );
    assert_eq!(holder_token(&object).as_deref(), Some("legacy-1"));
    object.metadata.annotations =
        Some([("rcoder.io/operation-id".to_string(), String::new())].into());
    assert_eq!(holder_token(&object), None);
}

/// 续租体只带 CAS 锚（观察到的 resourceVersion）与新鲜 renewTime。
#[test]
fn renewal_patch_carries_cas_anchor_and_fresh_renew() {
    let body = renewal_patch("42").expect("patch");
    assert_eq!(body["metadata"]["resourceVersion"], "42");
    assert!(
        body["spec"]["renewTime"]
            .as_str()
            .is_some_and(|stamp| !stamp.is_empty())
    );
}

/// 接管体带 CAS 锚、持有者身份改写与 transitions 递增。
#[test]
fn takeover_patch_rewrites_identity_and_bumps_transitions() {
    let desired = sample_lease(Some(0));
    let body = takeover_patch(&desired, "7", 3).expect("patch");
    assert_eq!(body["metadata"]["resourceVersion"], "7");
    assert_eq!(body["spec"]["holderIdentity"], "executor-one:operation-one");
    assert!(
        body["metadata"]["annotations"]["rcoder.io/lease-token"].is_null(),
        "ordinary takeover must remove an old compute token"
    );
    assert!(
        body["metadata"]["annotations"]
            .as_object()
            .unwrap()
            .contains_key("rcoder.io/lease-token"),
        "merge patch must explicitly clear the old key"
    );
    assert_eq!(body["spec"]["leaseTransitions"], 4);
    assert_eq!(body["spec"]["leaseDurationSeconds"], LEASE_TTL_SECONDS);
}

/// 事故回归闸（2026-09-22 app-154 死锁）：patch 体时间戳必须恰好 6 位
/// 微秒小数。手拼 `Timestamp::to_string()` 是纳秒（9 位），apiserver 的
/// MicroTime 解析直接拒收（"cannot parse 488Z as Z07:00"）→ 续租/接管
/// 永不成功 → 操作围栏死锁。类型化序列化（MicroTime serde 写死 %.6f）
/// 是正确性的来源；本测试把该不变量锁死，防止退化回手拼。
#[test]
fn patch_timestamps_are_microsecond_precision() {
    /// RFC3339 时间戳小数位须恰好 6 位（`…T12:34:56.123456Z`）。
    fn assert_micros(field: &str, stamp: &str) {
        let fraction = stamp
            .rsplit_once('.')
            .map(|(_, tail)| tail.strip_suffix('Z').unwrap_or(tail))
            .unwrap_or_default();
        assert_eq!(
            fraction.chars().count(),
            6,
            "{field} must be exactly microsecond precision, got: {stamp}"
        );
        assert!(
            fraction.chars().all(|c| c.is_ascii_digit()),
            "{field} fraction must be digits, got: {stamp}"
        );
    }

    let renewal = renewal_patch("9").expect("renewal patch");
    assert_micros(
        "renewTime",
        renewal["spec"]["renewTime"].as_str().expect("renewTime"),
    );

    let desired = sample_lease(Some(0));
    let takeover = takeover_patch(&desired, "7", 0).expect("takeover patch");
    for field in ["acquireTime", "renewTime"] {
        let stamp = takeover["spec"][field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing in takeover patch: {takeover}"));
        assert_micros(field, stamp);
    }
}
