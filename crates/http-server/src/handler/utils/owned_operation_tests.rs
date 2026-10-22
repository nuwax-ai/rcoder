use super::spawn_owned_operation;
use std::sync::Arc;
use tracing::Instrument as _;

#[tokio::test]
async fn owned_task_retains_request_locale_and_span_after_parent_scope_exits() {
    // This test uses a current-thread Tokio runtime, so the scoped subscriber
    // remains active for both parent and spawned task without changing globals.
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry());
    let gate = Arc::new(shared_types::OperationFlightGate::default());
    let span = tracing::info_span!("owned-request-context");
    let expected_span = span.id().expect("test span is enabled");
    let parent_locale = shared_types::current_request_locale();
    let mut worker = None;
    shared_types::scope_request_locale("zh-CN", async {
        worker = Some(spawn_owned_operation(
            gate.guard().expect("admit owned write"),
            async {
                // Poll beyond the parent task-local's lifetime. Reading the real
                // task-local here must not silently fall back to English.
                tokio::task::yield_now().await;
                (
                    shared_types::current_request_locale(),
                    tracing::Span::current().id(),
                )
            },
        ));
    })
    .instrument(span)
    .await;
    assert_eq!(shared_types::current_request_locale(), parent_locale);
    let (locale, actual_span) = worker
        .expect("parent admits owned operation")
        .await
        .expect("owned operation completes");
    assert_eq!(locale, "zh-CN");
    assert_eq!(actual_span, Some(expected_span));
    assert_eq!(gate.active(), 0, "completion releases the shutdown flight");
}
