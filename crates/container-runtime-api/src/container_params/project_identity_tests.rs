use super::*;

fn params() -> ContainerCreateParams {
    ContainerCreateParams::builder()
        .project_id("identity-one")
        .service_type(ServiceType::Userapp)
        .execution_context(shared_types::UserAppExecutionContext {
            app_id: "identity-one".into(),
            lifecycle_id: "life-one".into(),
            operation_id: "deploy-one".into(),
            executor_id: "executor-one".into(),
            request_fingerprint: "a".repeat(64),
        })
        .build()
}

#[test]
fn prod_project_identity_rejects_foreign_env_even_if_secret_masks_it() {
    let mut params = params();
    params.env = Some(HashMap::from([("PROJECT_ID".into(), "foreign".into())]));
    params.secrets = Some(HashMap::from([(
        "PROJECT_ID".into(),
        "identity-one".into(),
    )]));
    assert!(matches!(
        params.validate_execution_context(),
        Err(crate::types::ContainerRuntimeError::ConfigurationError(_))
    ));
}

#[test]
fn prod_project_identity_rejects_foreign_secret_and_empty_values() {
    for (env, secret) in [
        (Some("identity-one"), Some("foreign")),
        (Some(""), None),
        (None, Some("")),
    ] {
        let mut params = params();
        params.env = env.map(|value| HashMap::from([("PROJECT_ID".into(), value.into())]));
        params.secrets = secret.map(|value| HashMap::from([("PROJECT_ID".into(), value.into())]));
        assert!(
            matches!(
                params.validate_execution_context(),
                Err(crate::types::ContainerRuntimeError::ConfigurationError(_))
            ),
            "env={env:?} secret={secret:?}"
        );
    }
}

#[test]
fn prod_project_identity_accepts_only_matching_explicit_or_missing_input() {
    let mut params = params();
    params
        .validate_execution_context()
        .expect("platform will inject original app identity");
    params.env = Some(HashMap::from([(
        "PROJECT_ID".into(),
        "identity-one".into(),
    )]));
    params.secrets = params.env.clone();
    params.validate_execution_context().expect("same identity");
    params.project_id = Some("foreign".into());
    assert!(params.validate_execution_context().is_err());
}
