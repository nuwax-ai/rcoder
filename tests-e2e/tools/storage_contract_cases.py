"""Frozen storage acceptance catalog; do not derive required cases from test discovery."""
import re

PREFIX = 'userapp_lifecycle::tests::'
TURSO_PREFIX = 'userapp_lifecycle::common::local_tests::'
OWNER_PREFIX = 'db::tests::'
PG_USERAPP_CASE = PREFIX + 'postgres_real_transactions_and_restart_contract'
# Explicit cases exercised on independent real PG connections. Keep names fixed:
# discovery alone must not silently shrink the integration gate.
PG_EXTRA_TARGETS = {
    'PG ' + name: 'pg::project_store::tests::' + name for name in (
        'roundtrip_persists_and_reload_recovers',
        'clear_sessions_and_delete_container',
        'cross_replica_sync_visibility_and_removal',
        'cross_replica_sync_is_idempotent_for_own_entries',
        'leader_election_mutual_exclusion',
        'durable_commit_returns_after_pg_visible',
        'session_miss_backfills_from_pg_into_mirror',
        'shutdown_failure_is_not_success_on_first_or_repeated_call',
    )
} | {
    'PG dedicated leader session closes under cancellation and transport faults':
        'pg::project_store::leader::fault_tests::dedicated_leader_session_closes_after_shutdown_cancel_disconnect_and_timeout',
    'PG committed admission reply loss preserves original request and fence':
        'userapp_lifecycle::common::pg_commit_reply_tests::committed_admission_reply_loss_preserves_original_identity_and_fence',
    'PG independent owner scope CAS and bounded waits':
        'userapp_lifecycle::common::concurrency_tests::independent_pg_owners_enforce_scope_cas_and_bound_lock_waits',
    'PG preparation recovery independent owner CAS and retained fence':
        'userapp_lifecycle::common::concurrency_tests::independent_pg_preparation_recovery_has_one_winner_and_retains_fence',
    'PG lifecycle-bound activity':
        'userapp_lifecycle::common::activity_tests::pg_activity_is_monotonic_and_lifecycle_bound',
    'PG Preview contract': 'preview_lifecycle::pg_tests::pg_preview_store_satisfies_contract',
}

# These are component contracts on an actual temporary Turso database. They do
# not prove Docker volume mounting or rcoder process-recreation persistence.
TURSO_CASES = (
    'metadata_changes_commit_with_admission_and_never_before_rejection',
    'recreation_and_control_cannot_reuse_the_same_request_identity',
    'storage_deletion_does_not_end_the_application_lifecycle',
    'turso_paginated_recovery_and_identity',
    'turso_control_command_persistence_contract',
    'turso_configuration_policy_contract',
    'turso_private_execution_input_contract',
    'turso_physical_binding_contract',
    'turso_operation_lease_contract',
    'turso_policy_commit_contract',
    'turso_instance_directory_is_exclusive_and_restart_preserves_data',
    'repeated_registration_is_an_idempotent_noop',
    'concurrent_ensure_joins_but_different_intent_does_not',
    'old_progress_cannot_overwrite_new_checkpoint',
    'request_replay_returns_original_and_rejects_changed_parameters',
    'deletion_and_recreation_fence_late_unqualified_requests',
    'restart_keeps_operation_and_uncertainty_blocks_new_creators',
    'closed_database_returns_error_not_absence_or_admission',
    'rejected_admission_does_not_leave_new_identity',
    'joined_request_identity_remains_idempotent_after_completion',
    'metadata_cas_preserves_unmentioned_fields_and_noop_revision',
    'turso_control_snapshot_contract',
    'turso_deletion_success_requires_evidence',
    'another_executor_cannot_advance_a_running_operation',
    'turso_cross_scope_admission_is_independent',
    'turso_dev_uncertainty_does_not_block_prod',
    'scoped_terminals_clear_own_slots_and_blocker_is_structured',
    'turso_operation_deadline_contract',
)

# Backend-specific structural protections (worker/quarantine/exclusive lock).
TURSO_EXTRA_CASES = {
    case: TURSO_PREFIX + case for case in (
        'turso_store_basic_lifecycle',
        'turso_store_exclusive_directory_rejects_second_instance',
        'turso_legacy_sqlite_directory_is_rejected',
        'turso_existing_new_db_with_legacy_sibling_opens',
        'turso_restart_quarantines_only_interrupted_claims_without_replaying',
        'failed_admission_rolls_back_every_row_and_same_request_can_retry',
        'operation_id_collision_does_not_leave_an_unrelated_application',
        'broken_slot_is_rejected_instead_of_reported_idle',
        'damaged_recovery_rolls_back_prior_rows_and_releases_directory_lock',
    )
} | {
    case: OWNER_PREFIX + case for case in (
        'cancelled_caller_does_not_cancel_admitted_transaction',
        'shutdown_waiter_cancellation_keeps_lock_until_jobs_and_runtime_end',
        'last_owner_drop_drains_accepted_job_and_releases_resource',
        'baseline_is_repeatable_and_rejects_checksum_future_and_catalog_drift',
        'old_unversioned_database_is_preserved_and_rejected',
        'failed_initialization_joins_before_returning_error',
        'old_database_bytes_are_never_opened_by_downgraded_engine',
        'task_panic_is_unknown_and_every_shutdown_reports_failure',
        'worker_thread_panic_is_replayed_to_all_shutdown_waiters',
        'full_queue_rejects_without_waiting_for_the_running_transaction',
        'abandoned_transaction_rolls_back_before_connection_reuse',
        'toasty_turso_rows_affected_preserves_cas_and_conflict_semantics',
    )
}
TURSO_TARGETS = {case: PREFIX + case for case in TURSO_CASES} | TURSO_EXTRA_CASES


def passed_exactly_one(returncode, output):
    """Exit zero, no skip, exactly one passing libtest terminal record."""
    summaries = re.findall(r'^test result: (.+)$', output, re.MULTILINE)
    return returncode == 0 and len(summaries) == 1 and bool(re.fullmatch(
        r'ok\. 1 passed; 0 failed; 0 ignored; 0 measured; \d+ filtered out; finished in [\d.]+s\s*',
        summaries[0],
    ))
