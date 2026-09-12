# UserApp runtime operation ownership

Production application mutations acquire one ConfigMap per application and service family before reading deletion identities or reusing storage. Creation, update, wake, scale, restart, deletion and purge must hold that ownership across all runtime writes and metadata cleanup. Internal runtime methods do not reacquire the lease. Builder and production use separate names.

A PVC claim advances its resourceVersion before reuse; deletion sends captured UID and resourceVersion. The operation mutex additionally excludes the inverse race where a writer has claimed storage but has not yet created its Deployment. Deletion does not remove the operation ConfigMap through app resource selectors.

Explicit successful completion releases the ConfigMap with UID/resourceVersion preconditions. Preflight rejection releases ownership; version mismatch explicitly awaits release before returning. Once a mutation starts, cancellation or an uncertain backend error retains the ConfigMap. This deliberately blocks further operations rather than allowing a late API-server write to race deletion. A process crash or an uncertain ConfigMap creation response can also retain a lock. There is no TTL takeover.

Recovery requires an operator to establish that every owner process and any outstanding API request for that application has stopped, inspect the actual Deployment, PVC claims and configuration resources, and reconcile the intended operation. Only then may the captured lock UID/resourceVersion be conditionally deleted. Never delete by name alone and never assume a timestamp proves quiescence. Drain old writer binaries before rollout: older implementations do not participate in this mutex or metadata generation contract.

Docker uses persistent lock files under the configured shared UserApp data root. All platform processes sharing that data must share this root. Lock files must not be unlinked while users or waiters can hold their inode. Before the first mutation, the lock file stores an operation UUID and syncs it to disk. Completion validates that UUID and clears the marker. Cancellation or process exit leaves the marker; a subsequent OS-lock holder rejects the operation until an operator proves the old daemon request is quiescent. Production and builder share this marker contract with distinct file names.

Validation levels remain separate: local HTTP adapters validate Kubernetes request preconditions and mutual exclusion; no real cluster was used. The opt-in Docker identity test verifies a stale physical container receipt cannot remove a same-name replacement or its volume marker.

### Deployment protocol boundary

app-cli deploy protocol v3 guarantees that a terminal operation has no remaining deployment mutation. The platform releases a failed hot-deployment operation only for a matching operation/release, a terminal server phase, and absent or completed recovery metadata from protocol v3 or newer. Protocol v2 failure snapshots cannot establish this guarantee; their protection is retained for explicit recovery. Upgrade the app-runtime image before relying on automatic release after failed hot deployment. A stop that cannot be confirmed must remain busy and must not switch or restore code directories.
