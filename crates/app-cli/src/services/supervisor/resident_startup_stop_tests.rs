//! Controlled owner/actor barriers and real retained business Child.
//! No official Pingap or full container acceptance is claimed here.
use super::*;

#[test]
fn startup_failure_cleanup_cannot_revoke_admitted_native_drain() {
    let _environment = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let (_lease, mut proxy) = tests::owned_proxy(directory.path());
            let generation = uuid::Uuid::new_v4().to_string();
            let work_root = directory
                .path()
                .canonicalize()
                .unwrap()
                .join("work")
                .join(&generation);
            process_utils::command_authority::Gate::try_acquire(&work_root)
                .unwrap()
                .initialize()
                .unwrap();
            let scope = Some(runtime_supervisor::SessionScope {
                work_root,
                generation,
                supervisor_id: proxy.owner.scope.identity().owner_instance.clone(),
            });
            let (tx, rx) = mpsc::channel(8);
            Arc::get_mut(&mut proxy.owner).unwrap().tx = tx.clone();
            let owner = proxy.owner.clone();
            *RESIDENT.lock().unwrap() = Some(owner.clone());
            let mut command = tokio::process::Command::new("/bin/sleep");
            command.arg("30");
            let mut business = process_utils::managed_tree::spawn_managed(command).unwrap();
            let business_pid = business.id().unwrap();
            let cancel = tokio_util::sync::CancellationToken::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            // Old Ensure already reported its ordinary failure after exact candidate
            // cleanup. Native Stop has acquired the original drain barrier, but its
            // actor message is held until the failure handler has seen that owner.
            let mut control = control_publication_until(&owner, deadline, None, scope.clone())
                .await
                .unwrap();
            let epoch = owner.control_epoch.load(Ordering::Acquire);
            let (reply, native_result) = oneshot::channel();
            tx.send(Request::Standby {
                operation_id: Some("captured-native-stop".into()),
                deadline,
                epoch,
                watch_was_paused: control.watch_was_paused,
                release_control: control.release_control,
                publication: control.publication.take().unwrap(),
                reply,
                completion: control.completion.clone(),
            })
            .await
            .unwrap();
            control.dispatched = true;
            let (entered, reached) = oneshot::channel();
            let failure_scope = scope.clone();
            let failure_cancel = cancel.clone();
            let failure = tokio::spawn(async move {
                entered.send(()).unwrap();
                super::super::supervise::drain_or_close_resident_for_startup(
                    &failure_scope,
                    Some(&failure_cancel),
                    deadline,
                )
                .await
            });
            reached.await.unwrap();
            tokio::task::yield_now().await;
            let before_actor = owner.control_epoch.load(Ordering::Acquire);
            let retained_before_drain = process_utils::process_exists(business_pid).unwrap();
            let actor = tokio::spawn(proxy.run(rx));
            let native = native_result.await.unwrap();
            // Native GenerationControl.shutdown happens after successful entry
            // preparation. Only that captured token releases old business cleanup.
            if native.is_ok() {
                cancel.cancel();
            }
            let cleanup = failure.await.unwrap();
            let stopped = business.stop(Duration::ZERO).await;
            let (reply, done) = oneshot::channel();
            tx.send(Request::Shutdown { reply }).await.unwrap();
            let shutdown = done.await.unwrap();
            actor.abort();
            let _ = actor.await;
            *RESIDENT.lock().unwrap() = None;
            assert!(retained_before_drain);
            assert_eq!(
                before_actor, epoch,
                "startup failure revoked the admitted control epoch"
            );
            assert!(native.is_ok(), "captured native drain failed: {native:?}");
            assert!(cleanup.is_ok(), "failure cleanup failed: {cleanup:?}");
            assert!(stopped != StopOutcome::Unconfirmed);
            assert!(shutdown.is_ok());
            assert!(!process_utils::process_exists(business_pid).unwrap());
        });
}
