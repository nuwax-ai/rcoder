//! Dev forwarding connection observation; never sends or replays the request body.
use super::{HttpResultError, tcp_connect_once_within};
use crate::app_state::AppState;
use shared_types::UserAppOperationView;
use tracing::warn;

#[derive(Debug)]
pub(super) enum DevConnectionObservation {
    Connected,
    Unreachable(Option<Box<UserAppOperationView>>),
}

pub(super) async fn wait_for_dev_service(
    state: &AppState,
    app_id: &str,
    addr: &str,
) -> Result<(), HttpResultError> {
    let budget = std::time::Duration::from_secs(
        state
            .config
            .userapp_storage
            .dev_forward_connect_wait_seconds,
    );
    let deadline = tokio::time::Instant::now() + budget;
    let uri = addr.parse::<reqwest::Url>().map_err(|error| {
        HttpResultError::bad_gateway(format!("Invalid dev container address: {error}"))
    })?;
    let host = uri
        .host_str()
        .ok_or_else(|| HttpResultError::bad_gateway("Dev container address has no host"))?;
    let port = uri
        .port_or_known_default()
        .ok_or_else(|| HttpResultError::bad_gateway("Dev container address has no port"))?;
    let observation = observe_dev_connection_until(
        deadline,
        || tcp_connect_once_within(host, port, deadline),
        || async {
            state
                .app_service
                .get_current_operations(app_id)
                .await
                .map_err(|error| {
                    HttpResultError::bad_gateway(format!(
                        "Read dev container operations for app {app_id}: {error:#}"
                    ))
                })
        },
    )
    .await?;
    match observation {
        DevConnectionObservation::Connected => Ok(()),
        DevConnectionObservation::Unreachable(operation) => {
            Err(unreachable_error(app_id, operation))
        }
    }
}

fn unreachable_error(
    app_id: &str,
    operation: Option<Box<UserAppOperationView>>,
) -> HttpResultError {
    let locale = shared_types::current_request_locale();
    if let Some(operation) = operation {
        warn!(app_id, operation_id = %operation.operation_id, kind = ?operation.kind,
            "dev container unreachable while a Dev-scope operation is in flight");
        return HttpResultError::from_app_error(
            shared_types::AppError::with_message(
                shared_types::error_codes::ERR_OPERATION_IN_PROGRESS,
                shared_types::get_error_message(
                    shared_types::error_codes::ERR_OPERATION_IN_PROGRESS,
                    locale,
                ),
            )
            .with_operation_id(operation.operation_id),
        );
    }
    warn!(
        app_id,
        "dev container service path unreachable within the connect-wait budget"
    );
    HttpResultError::dev_container_unreachable(locale)
}

/// Same bounded observation path used by the real forwarder. The operation
/// snapshot is diagnostic evidence only; it never grants runtime ownership.
pub(super) async fn observe_dev_connection_until<Connect, Operations>(
    deadline: tokio::time::Instant,
    connect: impl Fn() -> Connect,
    operations: impl Fn() -> Operations,
) -> Result<DevConnectionObservation, HttpResultError>
where
    Connect: Future<Output = bool>,
    Operations: Future<Output = Result<Vec<UserAppOperationView>, HttpResultError>>,
{
    let mut inflight = None;
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let connected = tokio::time::timeout_at(deadline, connect()).await;
        if tokio::time::Instant::now() >= deadline {
            // A query from an earlier round is not current in-flight evidence.
            return Ok(DevConnectionObservation::Unreachable(None));
        }
        if matches!(connected, Ok(true)) {
            return Ok(DevConnectionObservation::Connected);
        }
        let observed = tokio::time::timeout_at(deadline, operations()).await;
        if tokio::time::Instant::now() >= deadline {
            return Ok(DevConnectionObservation::Unreachable(None));
        }
        let operations = match observed {
            Ok(result) => result?,
            Err(_) => return Ok(DevConnectionObservation::Unreachable(None)),
        };
        // Refresh every failed attempt: completion or a successor invalidates
        // the earlier snapshot. Storage failure remains an observation error.
        inflight = operations
            .into_iter()
            .find(|operation| {
                operation.scope == shared_types::UserAppOperationScope::Dev
                    && !operation.state.is_terminal()
            })
            .map(Box::new);
        tokio::time::sleep(
            std::time::Duration::from_millis(250)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
    Ok(DevConnectionObservation::Unreachable(inflight))
}
