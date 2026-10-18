use super::*;
use agent_client_protocol::schema::v1::SessionId;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct ControlledLifecycle {
    events: Mutex<Vec<&'static str>>,
    fail_stop: AtomicBool,
    fail_wait: AtomicBool,
    hold_stop: AtomicBool,
    hold_wait: AtomicBool,
    completed: AtomicBool,
    stop_entered: tokio::sync::Notify,
    release_stop: tokio::sync::Notify,
    wait_entered: tokio::sync::Notify,
    release_wait: tokio::sync::Notify,
    cancel: CancellationToken,
}

impl ControlledLifecycle {
    fn new(fail_stop: bool, fail_wait: bool) -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            fail_stop: AtomicBool::new(fail_stop),
            fail_wait: AtomicBool::new(fail_wait),
            hold_stop: AtomicBool::new(false),
            hold_wait: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            stop_entered: tokio::sync::Notify::new(),
            release_stop: tokio::sync::Notify::new(),
            wait_entered: tokio::sync::Notify::new(),
            release_wait: tokio::sync::Notify::new(),
            cancel: CancellationToken::new(),
        })
    }
}

impl AgentLifecycle for ControlledLifecycle {
    fn graceful_stop(&self) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.events.lock().unwrap().push("stop");
            anyhow::ensure!(
                !self.fail_stop.load(Ordering::SeqCst),
                "controlled stop failure"
            );
            if self.hold_stop.load(Ordering::SeqCst) {
                self.stop_entered.notify_one();
                self.release_stop.notified().await;
            }
            Ok(())
        })
    }

    fn wait_for_exit(&self) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.events.lock().unwrap().push("wait");
            anyhow::ensure!(
                !self.fail_wait.load(Ordering::SeqCst),
                "controlled reaper failure"
            );
            if self.hold_wait.load(Ordering::SeqCst) {
                self.wait_entered.notify_one();
                self.release_wait.notified().await;
            }
            self.completed.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }

    fn is_stopped(&self) -> bool {
        self.completed.load(Ordering::SeqCst)
    }

    fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

fn entry(
    session_id: &str,
    lifecycle: Option<Arc<dyn AgentLifecycle>>,
) -> (
    ProjectAndAgentInfo,
    mpsc::Receiver<agent_client_protocol::schema::v1::PromptRequest>,
) {
    let (prompt_tx, prompt_rx) = mpsc::channel(1);
    let (cancel_tx, _cancel_rx) = mpsc::channel(1);
    (
        ProjectAndAgentInfo {
            project_id: "owned-project".into(),
            session_id: SessionId::new(Arc::from(session_id)),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: Utc::now(),
            created_at: Utc::now(),
            stop_handle: lifecycle,
            agent_binary_snapshot: None,
        },
        prompt_rx,
    )
}

#[tokio::test]
async fn unusable_factory_identity_stops_and_waits_without_dispatching_a_prompt() {
    for sid in ["", "pending"] {
        let lifecycle = ControlledLifecycle::new(false, false);
        let (entry, mut prompts) = entry(sid, Some(lifecycle.clone()));
        let error = confirm_created_session_identity(
            &FactoryRegistry::default(),
            "owned-project",
            &entry,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("session ID"), "{error}");
        assert_eq!(*lifecycle.events.lock().unwrap(), ["stop", "wait"]);
        assert!(lifecycle.is_stopped());
        assert!(matches!(
            prompts.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}

#[tokio::test]
async fn established_factory_identity_remains_opaque_and_its_process_is_not_stopped() {
    for sid in ["opaque/session:7", "pending-session", "Pending", " pending"] {
        let lifecycle = ControlledLifecycle::new(false, false);
        let (entry, _prompts) = entry(sid, Some(lifecycle.clone()));
        confirm_created_session_identity(
            &FactoryRegistry::default(),
            "owned-project",
            &entry,
            Some("owned-project"),
        )
        .await
        .unwrap();
        assert_eq!(entry.session_id.0.as_ref(), sid);
        assert!(lifecycle.events.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn foreign_factory_identity_preserves_the_owner_and_confirms_its_new_child_exit() {
    let lifecycle = ControlledLifecycle::new(false, false);
    let (entry, _prompts) = entry("real-foreign-session", Some(lifecycle.clone()));
    let error = confirm_created_session_identity(
        &FactoryRegistry::default(),
        "owned-project",
        &entry,
        Some("foreign-project"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("already owned by project foreign-project"));
    assert_eq!(*lifecycle.events.lock().unwrap(), ["stop", "wait"]);
}

#[tokio::test]
async fn rejected_factory_cleanup_failure_preserves_both_identity_and_exit_diagnostics() {
    for (fail_stop, fail_wait, expected_events, expected_error) in [
        (true, false, vec!["stop"], "controlled stop failure"),
        (
            false,
            true,
            vec!["stop", "wait"],
            "controlled reaper failure",
        ),
    ] {
        let lifecycle = ControlledLifecycle::new(fail_stop, fail_wait);
        let (entry, _prompts) = entry("pending", Some(lifecycle.clone()));
        let error = confirm_created_session_identity(
            &FactoryRegistry::default(),
            "owned-project",
            &entry,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("preparation state"), "{error}");
        assert!(error.contains("child exit unconfirmed"), "{error}");
        assert!(error.contains(expected_error), "{error}");
        assert_eq!(*lifecycle.events.lock().unwrap(), expected_events);
        assert!(!lifecycle.is_stopped());
    }
}

#[tokio::test]
async fn rejected_factory_without_a_lifecycle_never_claims_exit_confirmation() {
    let (entry, _prompts) = entry("pending", None);
    let error = confirm_created_session_identity(
        &FactoryRegistry::default(),
        "owned-project",
        &entry,
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("preparation state"));
    assert!(error.contains("child exit unconfirmed: lifecycle handle is missing"));
}

#[derive(Default)]
struct FactoryRegistry {
    entries: DashMap<String, ProjectAndAgentInfo>,
    routes: DashMap<String, String>,
    publications: AtomicUsize,
}

impl SessionRegistry for FactoryRegistry {
    type Entry = ProjectAndAgentInfo;

    fn get(&self, project: &str) -> Option<Self::Entry> {
        self.entries.get(project).map(|entry| entry.clone())
    }
    fn insert(&self, project: &str, session: &str, entry: Self::Entry) {
        self.publications.fetch_add(1, Ordering::SeqCst);
        self.routes.insert(session.to_owned(), project.to_owned());
        self.entries.insert(project.to_owned(), entry);
    }
    fn remove(&self, project: &str) -> Option<Self::Entry> {
        let (_, entry) = self.entries.remove(project)?;
        self.routes
            .remove_if(entry.session_id.0.as_ref(), |_, owner| owner == project);
        Some(entry)
    }
    fn contains(&self, project: &str) -> bool {
        self.entries.contains_key(project)
    }
    fn get_project_by_session(&self, session: &str) -> Option<String> {
        shared_types::SessionHint::classify(Some(session)).as_session_id()?;
        self.routes.get(session).map(|owner| owner.clone())
    }
    fn get_entry_by_session(&self, session: &str) -> Option<Self::Entry> {
        self.get(&self.get_project_by_session(session)?)
            .filter(|entry| entry.session_id.0.as_ref() == session)
    }
    fn list_project_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }
    fn count(&self) -> usize {
        self.entries.len()
    }
    fn entry(&self, project: String) -> dashmap::mapref::entry::Entry<'_, String, Self::Entry> {
        self.entries.entry(project)
    }
}

fn creation_error(result: Result<(ProjectAndAgentInfo, bool), String>) -> String {
    match result {
        Ok(_) => panic!("expected a rejected creation"),
        Err(error) => error,
    }
}

fn creation_config() -> AgentStartConfig {
    AgentStartConfig {
        session_create_deadline: Some(tokio::time::Instant::now() + Duration::from_secs(10)),
        ..AgentStartConfig::new(shared_types::ServiceType::ComputerAgentRunner)
    }
}

#[tokio::test]
async fn unconfirmed_factory_stop_retains_exact_execution_and_blocks_new_factory_until_reaped() {
    let registry = Arc::new(FactoryRegistry::default());
    let creates = Arc::new(AtomicUsize::new(0));
    let lifecycle = ControlledLifecycle::new(true, false);
    let owner: Arc<dyn AgentLifecycle> = lifecycle.clone();
    let (mut rejected, _old_prompts) = entry("pending", Some(owner.clone()));
    rejected.request_id = Some("original-factory-request".into());
    let config = creation_config();
    let first = {
        let _transition = crate::session::lock_execution_transition("owned-project").await;
        create_after_reconciliation(
            registry.as_ref(),
            "owned-project",
            &None,
            &config,
            false,
            || {
                creates.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(rejected.clone()))
            },
        )
        .await
    };
    assert!(creation_error(first).contains("controlled stop failure"));
    let retained = registry.get("owned-project").unwrap();
    assert_eq!(retained.status, AgentStatus::Terminating);
    assert_eq!(retained.request_id, rejected.request_id);
    assert!(retained.prompt_tx.same_channel(&rejected.prompt_tx));
    assert!(retained.cancel_tx.same_channel(&rejected.cancel_tx));
    assert!(Arc::ptr_eq(retained.stop_handle.as_ref().unwrap(), &owner));
    assert_eq!(registry.publications.load(Ordering::SeqCst), 0);
    assert!(registry.get_project_by_session("pending").is_none());
    // No permanent transition marker: a control request can acquire this
    // project's lock and observe the retained exact execution after failure.
    drop(
        tokio::time::timeout(
            Duration::from_secs(1),
            crate::session::lock_execution_transition("owned-project"),
        )
        .await
        .unwrap(),
    );

    let (replacement, _new_prompts) = entry("new-opaque-session", None);
    let blocked = {
        let _transition = crate::session::lock_execution_transition("owned-project").await;
        create_after_reconciliation(
            registry.as_ref(),
            "owned-project",
            &None,
            &config,
            false,
            || {
                creates.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(replacement.clone()))
            },
        )
        .await
    };
    assert!(creation_error(blocked).contains("old agent exit unconfirmed"));
    assert_eq!(creates.load(Ordering::SeqCst), 1);

    lifecycle.fail_stop.store(false, Ordering::SeqCst);
    lifecycle.hold_wait.store(true, Ordering::SeqCst);
    let retry_registry = registry.clone();
    let retry_creates = creates.clone();
    let retried = tokio::spawn(async move {
        let _transition = crate::session::lock_execution_transition("owned-project").await;
        create_after_reconciliation(
            retry_registry.as_ref(),
            "owned-project",
            &None,
            &config,
            false,
            || {
                retry_creates.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(replacement))
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), lifecycle.wait_entered.notified())
        .await
        .unwrap();
    assert!(!retried.is_finished());
    assert_eq!(
        creates.load(Ordering::SeqCst),
        1,
        "no factory while original child exit is unconfirmed"
    );
    assert!(
        registry
            .get("owned-project")
            .unwrap()
            .cancel_tx
            .same_channel(&rejected.cancel_tx)
    );
    lifecycle.release_wait.notify_one();
    let (created, is_new) = retried.await.unwrap().unwrap();
    assert!(is_new);
    assert!(lifecycle.is_stopped());
    assert_eq!(created.session_id.0.as_ref(), "new-opaque-session");
    assert_eq!(
        registry.get("owned-project").unwrap().session_id,
        created.session_id
    );
    assert_eq!(creates.load(Ordering::SeqCst), 2);
    assert_eq!(registry.publications.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_foreign_factory_with_failed_reaping_retains_only_raw_entry_and_owner_route() {
    let registry = FactoryRegistry::default();
    registry
        .routes
        .insert("foreign-session".into(), "foreign-project".into());
    let lifecycle = ControlledLifecycle::new(false, true);
    let (rejected, _old_prompts) = entry("foreign-session", Some(lifecycle.clone()));
    let creates = AtomicUsize::new(0);
    let config = creation_config();
    let first =
        create_after_reconciliation(&registry, "owned-project", &None, &config, false, || {
            creates.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(rejected.clone()))
        })
        .await;
    assert!(creation_error(first).contains("controlled reaper failure"));
    assert_eq!(
        registry.get("owned-project").unwrap().status,
        AgentStatus::Terminating
    );
    assert_eq!(
        registry
            .get_project_by_session("foreign-session")
            .as_deref(),
        Some("foreign-project")
    );
    assert_eq!(registry.publications.load(Ordering::SeqCst), 0);

    let (replacement, _new_prompts) = entry("replacement-opaque", None);
    let blocked =
        create_after_reconciliation(&registry, "owned-project", &None, &config, false, || {
            creates.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(replacement.clone()))
        })
        .await;
    assert!(blocked.is_err());
    assert_eq!(creates.load(Ordering::SeqCst), 1);
    lifecycle.fail_wait.store(false, Ordering::SeqCst);
    let recovered =
        create_after_reconciliation(&registry, "owned-project", &None, &config, false, || {
            creates.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(replacement))
        })
        .await;
    assert!(recovered.is_ok());
    assert!(lifecycle.is_stopped());
    assert_eq!(creates.load(Ordering::SeqCst), 2);
    assert_eq!(
        registry
            .get_project_by_session("foreign-session")
            .as_deref(),
        Some("foreign-project")
    );
}

#[tokio::test]
async fn missing_factory_lifecycle_blocks_only_live_execution_and_closed_history_can_recover() {
    for sid in ["pending", "foreign-session"] {
        let registry = FactoryRegistry::default();
        registry
            .routes
            .insert("foreign-session".into(), "foreign-project".into());
        let (rejected, old_prompts) = entry(sid, None);
        let creates = AtomicUsize::new(0);
        let config = creation_config();
        let first =
            create_after_reconciliation(&registry, "owned-project", &None, &config, false, || {
                creates.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(rejected.clone()))
            })
            .await;
        assert!(creation_error(first).contains("lifecycle handle is missing"));
        let (replacement, _new_prompts) = entry("replacement", None);
        let blocked =
            create_after_reconciliation(&registry, "owned-project", &None, &config, false, || {
                creates.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(replacement.clone()))
            })
            .await;
        assert!(creation_error(blocked).contains("lifecycle handle is missing"));
        assert_eq!(creates.load(Ordering::SeqCst), 1);
        drop(old_prompts);
        let recovered =
            create_after_reconciliation(&registry, "owned-project", &None, &config, false, || {
                creates.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(replacement))
            })
            .await;
        assert!(
            recovered.is_ok(),
            "closed historical channels do not prove a live execution"
        );
        assert_eq!(creates.load(Ordering::SeqCst), 2);
        assert_eq!(
            registry
                .get_project_by_session("foreign-session")
                .as_deref(),
            Some("foreign-project")
        );
    }
}

#[tokio::test]
async fn rejected_factory_with_confirmed_exit_does_not_leave_a_startup_gate() {
    let registry = FactoryRegistry::default();
    let lifecycle = ControlledLifecycle::new(false, false);
    let (rejected, _old_prompts) = entry("pending", Some(lifecycle.clone()));
    let config = creation_config();
    let result =
        create_after_reconciliation(&registry, "owned-project", &None, &config, false, || {
            std::future::ready(Ok(rejected))
        })
        .await;
    assert!(result.is_err());
    assert!(lifecycle.is_stopped());
    assert!(!registry.contains("owned-project"));
    assert_eq!(registry.publications.load(Ordering::SeqCst), 0);
    let (replacement, _new_prompts) = entry("next-real-session", None);
    assert!(
        create_after_reconciliation(&registry, "owned-project", &None, &config, false, || {
            std::future::ready(Ok(replacement))
        })
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn cancelled_factory_rejection_remains_manageable_and_retry_waits_for_original_exit() {
    for sid in ["pending", "foreign-session"] {
        for cancel_during_stop in [true, false] {
            let registry = Arc::new(FactoryRegistry::default());
            registry
                .routes
                .insert("foreign-session".into(), "foreign-project".into());
            let creates = Arc::new(AtomicUsize::new(0));
            let lifecycle = ControlledLifecycle::new(false, false);
            lifecycle
                .hold_stop
                .store(cancel_during_stop, Ordering::SeqCst);
            lifecycle
                .hold_wait
                .store(!cancel_during_stop, Ordering::SeqCst);
            let owner: Arc<dyn AgentLifecycle> = lifecycle.clone();
            let (mut rejected, mut old_prompts) = entry(sid, Some(owner.clone()));
            rejected.request_id = Some("cancelled-factory-request".into());
            let first = {
                let registry = registry.clone();
                let creates = creates.clone();
                let rejected = rejected.clone();
                tokio::spawn(async move {
                    let _transition =
                        crate::session::lock_execution_transition("owned-project").await;
                    create_after_reconciliation(
                        registry.as_ref(),
                        "owned-project",
                        &None,
                        &creation_config(),
                        false,
                        || {
                            creates.fetch_add(1, Ordering::SeqCst);
                            std::future::ready(Ok(rejected))
                        },
                    )
                    .await
                })
            };
            let entered = if cancel_during_stop {
                &lifecycle.stop_entered
            } else {
                &lifecycle.wait_entered
            };
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            // Observe ownership before dropping the first future, at either cleanup await.
            let retained = registry
                .get("owned-project")
                .expect("rejected execution retained before cleanup await");
            assert_eq!(retained.session_id, rejected.session_id);
            assert_eq!(retained.status, AgentStatus::Terminating);
            assert_eq!(retained.request_id, rejected.request_id);
            assert!(retained.prompt_tx.same_channel(&rejected.prompt_tx));
            assert!(retained.cancel_tx.same_channel(&rejected.cancel_tx));
            assert!(Arc::ptr_eq(retained.stop_handle.as_ref().unwrap(), &owner));
            assert_eq!(registry.publications.load(Ordering::SeqCst), 0);
            first.abort();
            let Err(cancelled) = first.await else {
                panic!("cleanup future completed before cancellation");
            };
            assert!(cancelled.is_cancelled());
            assert!(!lifecycle.is_stopped());

            // The original entry, rather than a permanent lock, is the recovery gate.
            drop(
                tokio::time::timeout(
                    Duration::from_secs(1),
                    crate::session::lock_execution_transition("owned-project"),
                )
                .await
                .unwrap(),
            );
            lifecycle.hold_stop.store(false, Ordering::SeqCst);
            lifecycle.hold_wait.store(true, Ordering::SeqCst);
            let (replacement, _new_prompts) = entry("new-after-cancel", None);
            let retry = {
                let registry = registry.clone();
                let creates = creates.clone();
                tokio::spawn(async move {
                    let _transition =
                        crate::session::lock_execution_transition("owned-project").await;
                    create_after_reconciliation(
                        registry.as_ref(),
                        "owned-project",
                        &None,
                        &creation_config(),
                        false,
                        || {
                            creates.fetch_add(1, Ordering::SeqCst);
                            std::future::ready(Ok(replacement))
                        },
                    )
                    .await
                })
            };
            tokio::time::timeout(Duration::from_secs(2), lifecycle.wait_entered.notified())
                .await
                .unwrap();
            assert!(!retry.is_finished());
            assert_eq!(
                creates.load(Ordering::SeqCst),
                1,
                "cancelled cleanup must not permit a second factory before original exit"
            );
            assert!(
                registry
                    .get("owned-project")
                    .unwrap()
                    .cancel_tx
                    .same_channel(&rejected.cancel_tx)
            );
            assert_eq!(
                registry
                    .get_project_by_session("foreign-session")
                    .as_deref(),
                Some("foreign-project")
            );
            assert!(registry.get_project_by_session("pending").is_none());
            assert!(matches!(
                old_prompts.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));

            lifecycle.release_wait.notify_one();
            let (created, is_new) = retry.await.unwrap().unwrap();
            assert!(is_new);
            assert!(lifecycle.is_stopped());
            assert_eq!(created.session_id.0.as_ref(), "new-after-cancel");
            assert_eq!(creates.load(Ordering::SeqCst), 2);
            assert_eq!(registry.publications.load(Ordering::SeqCst), 1);
            assert_eq!(
                registry
                    .get_project_by_session("foreign-session")
                    .as_deref(),
                Some("foreign-project")
            );
            assert!(
                registry
                    .get("owned-project")
                    .unwrap()
                    .cancel_tx
                    .same_channel(&created.cancel_tx)
            );
        }
    }
}

#[tokio::test]
async fn confirmed_rejected_exit_does_not_remove_same_sid_replacement_or_foreign_route() {
    let registry = Arc::new(FactoryRegistry::default());
    registry
        .routes
        .insert("same-session".into(), "foreign-project".into());
    let lifecycle = ControlledLifecycle::new(false, false);
    lifecycle.hold_wait.store(true, Ordering::SeqCst);
    let (rejected, _old_prompts) = entry("same-session", Some(lifecycle.clone()));
    let retired = {
        let registry = registry.clone();
        let rejected = rejected.clone();
        tokio::spawn(async move {
            confirm_created_session_identity(
                registry.as_ref(),
                "owned-project",
                &rejected,
                Some("foreign-project"),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), lifecycle.wait_entered.notified())
        .await
        .unwrap();
    assert!(
        registry
            .get("owned-project")
            .unwrap()
            .cancel_tx
            .same_channel(&rejected.cancel_tx)
    );
    let (replacement, _new_prompts) = entry("same-session", None);
    drop(
        registry
            .entry("owned-project".into())
            .insert(replacement.clone()),
    );
    lifecycle.release_wait.notify_one();
    assert!(matches!(
        retired.await.unwrap(),
        Err(CreatedSessionRejection::Retired(_))
    ));
    assert!(lifecycle.is_stopped());
    let retained = registry.get("owned-project").unwrap();
    assert_eq!(retained.session_id, replacement.session_id);
    assert!(retained.cancel_tx.same_channel(&replacement.cancel_tx));
    assert!(retained.prompt_tx.same_channel(&replacement.prompt_tx));
    assert_eq!(retained.status, replacement.status);
    assert_eq!(
        registry.get_project_by_session("same-session").as_deref(),
        Some("foreign-project")
    );
    assert_eq!(registry.publications.load(Ordering::SeqCst), 0);
}
