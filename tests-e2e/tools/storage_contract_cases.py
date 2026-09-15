"""Frozen storage acceptance catalog; do not derive required cases from test discovery."""
import re

PREFIX = 'userapp_lifecycle::tests::'
PG_USERAPP_CASE = PREFIX + 'postgres_real_transactions_and_restart_contract'
# These are component contracts on an actual temporary SQLite database. They do
# not prove Docker volume mounting or rcoder process-recreation persistence.
SQLITE_CASES = (
    'metadata_changes_commit_with_admission_and_never_before_rejection',
    'sqlite_failure_after_operation_insert_rolls_back_entire_admission',
    'recreation_and_control_cannot_reuse_the_same_request_identity',
    'storage_deletion_does_not_end_the_application_lifecycle',
    'sqlite_paginated_recovery_and_legacy_import',
    'sqlite_control_command_persistence_contract',
    'sqlite_configuration_policy_contract',
    'sqlite_private_execution_input_contract',
    'sqlite_physical_binding_contract',
    'sqlite_operation_lease_contract',
    'sqlite_policy_commit_contract',
    'sqlite_instance_directory_is_exclusive_and_restart_preserves_data',
    'sqlite_directory_alias_uses_the_same_instance_lock',
    'sqlite_database_file_alias_cannot_bypass_directory_ownership',
    'sqlite_linked_sidecars_and_lock_fail_before_touching_the_target',
    'sqlite_failed_initialization_releases_the_instance_lock',
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
    'sqlite_control_snapshot_contract',
    'sqlite_control_snapshot_rejects_broken_operation_link',
    'sqlite_deletion_success_requires_evidence',
    'another_executor_cannot_advance_a_running_operation',
    'cancelled_admission_waiting_for_sqlite_writer_does_not_leak_a_transaction',
)

SQLITE_EXTRA_CASES = {
    'invalid_interrupted_record_blocks_startup_without_partial_quarantine':
        'userapp_lifecycle::sqlite::restart::tests::invalid_interrupted_record_blocks_startup_without_partial_quarantine',

    'exclusive_restart_quarantines_only_interrupted_claims_without_replaying':
        'userapp_lifecycle::sqlite::restart::tests::exclusive_restart_quarantines_only_interrupted_claims_without_replaying',
}
SQLITE_TARGETS = {case: PREFIX + case for case in SQLITE_CASES} | SQLITE_EXTRA_CASES


def passed_exactly_one(returncode, output):
    """Exit zero, no skip, exactly one passing libtest terminal record."""
    summaries = re.findall(r'^test result: (.+)$', output, re.MULTILINE)
    return returncode == 0 and len(summaries) == 1 and bool(re.fullmatch(
        r'ok\. 1 passed; 0 failed; 0 ignored; 0 measured; \d+ filtered out; finished in [\d.]+s\s*',
        summaries[0],
    ))
