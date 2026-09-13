"""Frozen deterministic lifecycle concurrency regressions (component evidence)."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
from storage_contract_cases import passed_exactly_one

REPO = Path(__file__).resolve().parents[2]
CASES = {
    'app_manager': (
        'activity_registry::tests::traffic_wake_retains_confirmation_gate_while_start_is_in_flight',
        'activity_registry::tests::wake_timeout_when_never_ready',
        'activity_registry::tests::wake_leader_panic_cleans_flight_but_retains_uncertain_operation',
        'activity_registry::tests::traffic_wake_does_not_confirm_a_replacement_running_resource',
        'service::tests::storage_expansion_receipt_is_bound_to_the_update_operation',
        'service::tests::update_commits_a_durable_operation_matching_runtime_context',
        'handlers::control::builder_retry_tests::retry_handler_separates_pending_execution_from_confirmed_builder_reconciliation',
    ),
    'docker_manager': (
        'runtime::docker_builder_deletion::tests::captured_file_release_requires_inactive_original_inode_and_owner',
        'runtime::docker_builder_deletion::tests::captured_file_release_rejects_symbolic_link_even_to_original_inode',
    ),
    'rcoder': tuple('userapp_builder::' + name for name in (
        'creation::tests::final_creation_evidence_survives_terminal_failure_and_rejects_wrong_identity',
        'creation::tests::expired_creation_retains_observation_and_discards_late_success',
        'creation::tests::expired_checkpoint_failure_still_observes_remote_completion',
        'creation::tests::expired_budget_never_starts_unpolled_creation',
        'creation::tests::late_subscriber_reads_committed_completion_even_after_signal_removal',
        'creation::tests::timeout_does_not_cancel_or_resubmit_accepted_operation',
        'deadline_tests::nested_waits_share_one_deadline',
        'deadline_tests::waiter_timeout_does_not_cancel_an_accepted_worker',
        'recovery::terminal_tests::terminal_scan_releases_exact_receipt_then_forgets_without_reexecution',
        'recovery::terminal_tests::terminal_scan_preserves_replacement_owner_and_durable_receipt_on_release_failure',
        'recovery::tests::blocked_recovery_does_not_block_other_applications_or_duplicate_itself',
        'recovery::tests::spawned_worker_keeps_its_slot_until_completion',
        'recovery::tests::capacity_and_panic_do_not_drop_other_recoveries',
        'dev_cleanup::tests::uncertain_mutation_does_not_release_distributed_lease',
        'dev_cleanup::tests::confirmed_external_write_releases_once_and_cannot_be_reused',
        'dev_cleanup::tests::failed_external_lease_release_preserves_uncertain_state',
    )) + (
        'userapp_forward::error_body::tests::stalled_http_upload_releases_rejection_handler_within_budget',
        'userapp_forward::error_body::tests::small_slow_multipart_receives_original_envelope_over_real_http',
    ),
    'file_server_userapp': (
        'service::userapp::run_dir::tests::activation_releases_lease_with_a_duplicated_file_description',
        'handlers::userapp_app_files::clear_identity_tests::registered_routes_preserve_the_internal_reset_wire_contract',
        'handlers::userapp_app_files::clear_identity_tests::replaced_instance_is_rejected_before_workspace_mutation',
        'service::userapp::tasks::tests::terminal_event_does_not_release_a_workers_workspace_lease',
        'service::userapp::tasks::tests::concurrent_terminal_events_commit_exactly_once',
        'service::userapp::tasks::tests::stopped_generation_cannot_commit_after_preparation',
    ),
}


def main():
    directory = Path(os.environ['E2E_REPORT_DIR']) / 'concurrency-contract'
    directory.mkdir(parents=True, exist_ok=False)
    assertions = []

    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': ok, 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))

    try:
        build = subprocess.run(['cargo', 'test', '-p', 'rcoder', '-p', 'file-server-userapp', '-p', 'docker_manager', '-p', 'app_manager',
                                '--locked', '--lib', '--no-run', '--message-format=json'], cwd=REPO,
                               capture_output=True, text=True, timeout=1800)
        (directory / 'build.jsonl').write_text(build.stdout)
        (directory / 'build.stderr').write_text(build.stderr)
        if build.returncode:
            raise RuntimeError('concurrency test compilation failed')
        artifacts = [json.loads(line) for line in build.stdout.splitlines()]
        identity = {'run_id': os.environ['E2E_RUN_ID'], 'case_id': os.environ['E2E_CASE_ID'],
                    'evidence_level': 'deterministic_components', 'binaries': {}}
        for target, cases in CASES.items():
            binaries = [row['executable'] for row in artifacts if row.get('reason') == 'compiler-artifact'
                        and row.get('executable') and row.get('target', {}).get('name') == target]
            if len(binaries) != 1:
                raise RuntimeError('missing or duplicate contract executable: ' + target)
            frozen = directory / target
            shutil.copy2(binaries[0], frozen)
            identity['binaries'][target] = {'sha256': hashlib.sha256(frozen.read_bytes()).hexdigest(), 'planned_cases': cases}
            (directory / 'identity.json').write_text(json.dumps(identity, indent=2))
            listing = subprocess.run([str(frozen), '--list', '--format=terse'], cwd=REPO,
                                     capture_output=True, text=True, check=True, timeout=30).stdout
            names = {line.removesuffix(': test') for line in listing.splitlines() if line.endswith(': test')}
            if set(cases) - names:
                raise RuntimeError('required concurrency cases absent: ' + ', '.join(sorted(set(cases) - names)))
            for case in cases:
                # 契约子进程自含运行镜像 env：部分组件用例（如 update 扩容回执）
                # 解析部署镜像，宿主 shell 未必导出该变量。
                case_env = dict(os.environ, RCODER_RUNTIME_IMAGE_DIGEST=os.environ.get(
                    'RCODER_RUNTIME_IMAGE_DIGEST', 'dev-app-runtime:latest'))
                result = subprocess.run([str(frozen), case, '--exact', '--nocapture'], cwd=REPO,
                                        capture_output=True, text=True, timeout=120, env=case_env)
                log = result.stdout + result.stderr
                (directory / (target + '-' + case.replace('::', '-') + '.log')).write_text(log)
                record('Concurrency ' + case, passed_exactly_one(result.returncode, log))
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        record('Concurrency execution', False, type(error).__name__ + ': ' + str(error))
    return int(not assertions or any(not item['ok'] for item in assertions))


if __name__ == '__main__':
    raise SystemExit(main())
