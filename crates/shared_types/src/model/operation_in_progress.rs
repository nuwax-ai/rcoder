//! Admission conflicts produced by an independently running application operation.
use serde::{Deserialize, Serialize};
use utoipa::{PartialSchema, ToSchema};

/// The blocking operation is observational evidence, never a newly accepted request.
/// Unknown holders remain null; a runtime lease annotation is not a durable ID.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OperationInProgressData {
    /// Real durable operation ID, or null before durable admission / after a race.
    #[schema(example = "95a2d059-50ee-4ac7-b315-0a4ba7929f96")]
    pub holder_operation_id: Option<String>,
    /// Actual kind in snake_case, e.g. start, start_deployment, restart_deployment,
    /// hot_deploy, or a compute control action. Never inferred from a message.
    #[schema(example = "start")]
    pub holder_kind: Option<String>,
    /// True only when the durable command records start with traffic=true.
    pub holder_traffic_wake: bool,
    /// Actual state in snake_case, or null when the holder could not be verified.
    #[schema(example = "running")]
    pub holder_state: Option<String>,
    /// Stage from the same durable holder snapshot.
    #[schema(example = "traffic_wake_observing")]
    pub holder_step: Option<String>,
    /// Whether this rejected, unaccepted request can be retried with the same
    /// request identity and input. Does not authorize replay of unknown writes.
    pub retryable: bool,
    /// Advisory delay in seconds; zero when safe retry is not established.
    #[schema(example = 20)]
    pub retry_after_seconds: u64,
}

impl OperationInProgressData {
    pub fn from_blocker(
        blocker: &crate::UserAppOperationBlocker,
        holder_traffic_wake: bool,
        retryable: bool,
        retry_after_seconds: u64,
    ) -> Self {
        if crate::validate_identifier(&blocker.operation_id, "operation_id").is_err() {
            return Self::default();
        }
        Self {
            holder_operation_id: Some(blocker.operation_id.clone()),
            holder_kind: Some(operation_kind_name(blocker.kind).into()),
            holder_traffic_wake,
            holder_state: Some(operation_state_name(blocker.state).into()),
            holder_step: Some(blocker.step.clone()),
            retryable,
            retry_after_seconds: if retryable { retry_after_seconds } else { 0 },
        }
    }

    pub fn localized_message(&self, locale: &str) -> String {
        crate::get_operation_in_progress_message(
            self.holder_kind.as_deref(),
            self.retryable,
            self.retry_after_seconds,
            locale,
        )
    }
}

fn operation_kind_name(kind: crate::UserAppOperationKind) -> &'static str {
    use crate::UserAppOperationKind as Kind;
    match kind {
        Kind::EnsureBuilder => "ensure_builder",
        Kind::AdoptBuilder => "adopt_builder",
        Kind::AdoptApplication => "adopt_application",
        Kind::StopBuilder => "stop_builder",
        Kind::RestartBuilder => "restart_builder",
        Kind::Create => "create",
        Kind::StartDeployment => "start_deployment",
        Kind::RestartDeployment => "restart_deployment",
        Kind::Update => "update",
        Kind::Start => "start",
        Kind::Restart => "restart",
        Kind::Stop => "stop",
        Kind::SetRecyclePolicy => "set_recycle_policy",
        Kind::HotDeploy => "hot_deploy",
        Kind::DeleteCompute => "delete_compute",
        Kind::PurgeResources => "purge_resources",
        Kind::DestroyDevStorage => "destroy_dev_storage",
        Kind::DestroyProdStorage => "destroy_prod_storage",
        Kind::ClearDevStorage => "clear_dev_storage",
        Kind::ClearProdStorage => "clear_prod_storage",
        Kind::ResetDevDatabasePassword => "reset_dev_database_password",
        Kind::ResetProdDatabasePassword => "reset_prod_database_password",
        Kind::PrepareProdDatabase => "prepare_prod_database",
        Kind::DeleteApplication => "delete_application",
    }
}

fn operation_state_name(state: crate::UserAppOperationState) -> &'static str {
    use crate::UserAppOperationState as State;
    match state {
        State::Pending => "pending",
        State::Running => "running",
        State::WaitingRetry => "waiting_retry",
        State::RecoveryRequired => "recovery_required",
        State::Succeeded => "succeeded",
        State::Failed => "failed",
    }
}

/// OpenAPI data alternatives: ordinary business payload or this specific error.
/// Opaque JSON business payloads can also match the holder schema, so the union
/// uses anyOf. Runtime serialization still selects the error by its typed code.
pub enum ApiBody<T> {
    Business(T),
    OperationInProgress(OperationInProgressData),
}

// ComposeSchema preserves utoipa's generic component registration when an
// enclosing HttpResult supplies a concrete schema for T.
impl<T: ToSchema> utoipa::__dev::ComposeSchema for ApiBody<T> {
    fn compose(
        generics: Vec<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>,
    ) -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        let business = generics.into_iter().next().unwrap_or_else(T::schema);
        utoipa::openapi::schema::AnyOfBuilder::new()
            .item(business)
            .item(utoipa::openapi::Ref::from_schema_name(
                OperationInProgressData::name(),
            ))
            .into()
    }
}

impl<T: ToSchema> ToSchema for ApiBody<T> {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ApiBody")
    }

    fn schemas(
        schemas: &mut Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) {
        schemas.push((T::name().into_owned(), T::schema()));
        T::schemas(schemas);
        schemas.push((
            OperationInProgressData::name().into_owned(),
            OperationInProgressData::schema(),
        ));
        OperationInProgressData::schemas(schemas);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn opaque_json_and_typed_holder_schema_use_nonexclusive_alternatives() {
        use utoipa::PartialSchema as _;
        let schema = serde_json::to_value(super::ApiBody::<serde_json::Value>::schema()).unwrap();
        assert!(schema.get("oneOf").is_none());
        let alternatives = schema["anyOf"].as_array().unwrap();
        assert_eq!(alternatives.len(), 2);
        // serde_json::Value accepts every JSON value, including the holder
        // object accepted by the second branch. oneOf would reject that object.
        assert_eq!(alternatives[0], serde_json::json!({}));
        assert_eq!(
            alternatives[1]["$ref"],
            "#/components/schemas/OperationInProgressData"
        );
        let holder = serde_json::to_value(super::OperationInProgressData::default()).unwrap();
        assert!(holder.is_object());
        assert!(serde_json::from_value::<super::OperationInProgressData>(holder).is_ok());
    }

    #[test]
    fn holder_wire_names_preserve_real_kinds_without_changing_existing_blocker() {
        for (kind, expected) in [
            (crate::UserAppOperationKind::Start, "start"),
            (
                crate::UserAppOperationKind::StartDeployment,
                "start_deployment",
            ),
            (
                crate::UserAppOperationKind::RestartDeployment,
                "restart_deployment",
            ),
            (crate::UserAppOperationKind::HotDeploy, "hot_deploy"),
        ] {
            let blocker = crate::UserAppOperationBlocker {
                scope: crate::UserAppOperationScope::Prod,
                operation_id: "real-holder".into(),
                kind,
                state: crate::UserAppOperationState::RecoveryRequired,
                step: "inspect_original_write".into(),
            };
            let data = super::OperationInProgressData::from_blocker(&blocker, false, false, 45);
            let value = serde_json::to_value(data).unwrap();
            assert_eq!(value["holder_kind"], expected);
            assert_eq!(value["holder_state"], "recovery_required");
            assert_eq!(value["retry_after_seconds"], 0);
            let legacy = serde_json::to_value(blocker).unwrap();
            assert_eq!(legacy["state"], "RecoveryRequired");
        }
        let unknown = serde_json::to_value(super::OperationInProgressData::default()).unwrap();
        for field in [
            "holder_operation_id",
            "holder_kind",
            "holder_state",
            "holder_step",
        ] {
            assert!(unknown[field].is_null());
        }
        assert_eq!(unknown["retryable"], false);
        assert_eq!(unknown["retry_after_seconds"], 0);

        let unadmitted = crate::UserAppOperationBlocker {
            scope: crate::UserAppOperationScope::Prod,
            operation_id: String::new(),
            kind: crate::UserAppOperationKind::Start,
            state: crate::UserAppOperationState::Pending,
            step: "acquiring".into(),
        };
        let unknown = super::OperationInProgressData::from_blocker(&unadmitted, true, true, 20);
        assert_eq!(unknown, super::OperationInProgressData::default());
    }
}
