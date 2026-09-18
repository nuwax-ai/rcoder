"""Frozen storage acceptance catalog; do not derive required cases from test discovery."""
import re

PREFIX = 'userapp_lifecycle::tests::'
TURSO_PREFIX = 'userapp_lifecycle::turso::tests::'
PG_USERAPP_CASE = PREFIX + 'postgres_real_transactions_and_restart_contract'
# These are component contracts on an actual temporary Turso database. They do
# not prove Docker volume mounting or rcoder process-recreation persistence.
TURSO_CASES = (
    'metadata_changes_commit_with_admission_and_never_before_rejection',
    'recreation_and_control_cannot_reuse_the_same_request_identity',
    'storage_deletion_does_not_end_the_application_lifecycle',
    'turso_paginated_recovery_and_legacy_import',
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
    'turso_store_basic_lifecycle': TURSO_PREFIX + 'turso_store_basic_lifecycle',
    'turso_store_exclusive_directory_rejects_second_instance':
        TURSO_PREFIX + 'turso_store_exclusive_directory_rejects_second_instance',
    'turso_store_rejects_checksum_mismatch':
        TURSO_PREFIX + 'turso_store_rejects_checksum_mismatch',
    'turso_failure_midway_rolls_back_entire_admission':
        TURSO_PREFIX + 'turso_failure_midway_rolls_back_entire_admission',
    'turso_control_snapshot_rejects_broken_operation_link':
        TURSO_PREFIX + 'turso_control_snapshot_rejects_broken_operation_link',
    'turso_cancelled_caller_and_dangling_transaction_do_not_leak':
        TURSO_PREFIX + 'turso_cancelled_caller_and_dangling_transaction_do_not_leak',
    'turso_restart_quarantines_only_interrupted_claims_without_replaying':
        TURSO_PREFIX + 'turso_restart_quarantines_only_interrupted_claims_without_replaying',
    'turso_invalid_interrupted_record_blocks_startup_without_partial_quarantine':
        TURSO_PREFIX + 'turso_invalid_interrupted_record_blocks_startup_without_partial_quarantine',
}
TURSO_TARGETS = {case: PREFIX + case for case in TURSO_CASES} | TURSO_EXTRA_CASES


def passed_exactly_one(returncode, output):
    """Exit zero, no skip, exactly one passing libtest terminal record."""
    summaries = re.findall(r'^test result: (.+)$', output, re.MULTILINE)
    return returncode == 0 and len(summaries) == 1 and bool(re.fullmatch(
        r'ok\. 1 passed; 0 failed; 0 ignored; 0 measured; \d+ filtered out; finished in [\d.]+s\s*',
        summaries[0],
    ))
