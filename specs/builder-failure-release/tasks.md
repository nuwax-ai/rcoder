# Tasks and evidence

## Implemented

- Structured API rejection evidence lives in shared_types and crosses the runtime error boundary.
- Both builder runtimes use one completion policy; timeout/transport/5xx/unknown retains the fence.
- Docker create/start preserves the SDK error; status strings are not parsed.
- PVC force-delete errors stop creation; cached Running creation propagates Service reconciliation failures.
- Existing RBAC commit 84e95b9 is unchanged. No cluster access or residual-lock deletion.

## Regression mapping

| Invariant | Test |
| --- | --- |
| Only success/explicit rejection releases; failure to release is visible | completion_releases_only_success_or_explicit_rejection; release_failure_preserves_original_error_and_rejects_success |
| K8s 403 releases with UID/resourceVersion preconditions | create_container_releases_operation_lease_when_create_fails |
| Lost PATCH response and HTTP 500 retain the lease | create_container_retains_lease_when_claim_response_is_lost; create_container_retains_lease_when_claim_returns_server_error |
| Cached Running cannot swallow Service mutation failure | cached_builder_propagates_service_write_failure |
| Docker structured rejection survives actual HTTP adapter | docker_create_preserves_rejection_and_lost_response_evidence |
| Docker marker clears only after confirmed completion | failed_builder_completion_clears_only_confirmed_rejection_marker |
| Failed marker release preserves replacement owner and original failure | failed_marker_release_does_not_erase_other_owner_or_original_error |
| HTTP 408/499/5xx and string-only errors are not rejections | docker_status_classification_does_not_guess_from_messages; k8s_status_classification_retains_timeouts_and_server_errors |

## Validation

- Working baseline: fc552ba; concurrent unrelated file-server commit advanced HEAD to 7753812.
- Kubernetes docker_manager library tests: 151 passed, 5 existing ignored, 0 failed.
- Kubernetes Clippy for docker_manager and rcoder all targets with -D warnings passed.
- Final frozen-source commands (all exit 0):
  - `cargo fmt --all --check`
  - `git diff --check`
  - `cargo clippy -p docker_manager --all-targets --locked -- -D warnings`
  - `cargo clippy -p docker_manager -p rcoder --features kubernetes --all-targets --locked -- -D warnings`
  - `cargo test -p docker_manager --features kubernetes --lib --locked` (151 passed, 5 ignored, 0 failed)
- Logs: `/tmp/builder-classification-clippy-default.log`, `/tmp/builder-classification-clippy-k8s-final.log`, `/tmp/builder-classification-tests-final.log`.
- The five ignored tests are existing environment-dependent mount/path and real-container timestamp tests; they are not counted as passed. Full workspace tests and Compose E2E were not run for this focused repair.
- Evidence is component plus local HTTP API-contract testing. No real Docker deployment or K8s cluster acceptance was run in this repair.
