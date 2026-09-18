# PostgreSQL lifecycle write contracts

Migration `0004_lifecycle_identity.sql` assigns stable opaque identities to existing
projects and sessions. An update of a live project retains its identity; recreation
after removal gets a fresh identity with the prior identity as predecessor.
Deletion records a durable tombstone for the accepted identity and cannot target a
later incarnation. Session clearing operates on captured session identities.
Container deletion checks the captured project identity and current container ID.

All write transactions acquire their advisory lock keys in sorted order before
executing statements. Local registration serializes mirror changes and immutable
operation capture; it never holds a synchronous lock across database awaits.
Durable operations remain registered until commit or queue fallback. Dropping a
cancelled request queues its operations. Shutdown closes admission, waits for
registered direct writes, and then drains the writer. Concurrent/repeated writer
flush calls share a terminal `FlushOutcome`; a caller timeout does not detach the
completion observer.

## Rollout

Old writers must be stopped and successfully drained before migration/new writers
start. Mixed old/new writer operation is not supported: old code does not supply
identities or respect tombstones. Do not delete tombstones while old operations may
still replay. Migration is additive and existing rows are backfilled once; load,
cross-replica synchronization and session fetch preserve those values.

## Isolated PostgreSQL 17 regression

The test runner owns a fresh PostgreSQL 17 container/database and sets:

```
RCODER_PG_TEST_DSN=<isolated-run-dsn> RCODER_PG_TEST_STRICT=1 \
  cargo test -p rcoder-storage --features pg --lib lifecycle_contract -- --nocapture
```

Strict mode fails when the DSN is absent. Tests assert PostgreSQL major version 17.
The suite covers delayed deletes, tombstones, session reuse, container reassignment,
load/sync, legacy schema backfill, concurrent shutdown, and cancellation while a
real PostgreSQL advisory lock blocks a durable write. No production cluster or
existing database is part of this suite. The enclosing E2E runner must verify a
nonzero expected test count and remove only the container/database it created.

Without strict mode these integration cases remain environment-gated for ordinary
workspace test runs. That skip behavior is not E2E acceptance evidence.

## Operation scopes and active-operation slots (2026-09)

Each operation persists a server-derived `scope` (`Dev` builder resources,
`Prod` production runtime, `Application` both environments). The lifecycle
record carries `active_operations {dev, prod, application}` instead of the
former single `current_operation_id`: admission occupies only the request
kind's scope slot, a terminal commit clears only that slot, and advance
validates ownership through the operation's own slot. Cross-scope operations
never fence each other; an application-scope operation requires every slot to
be idle and itself occupies the application slot in the same transaction that
flips the lifecycle to `Deleting`.

Migration 0007 backfills `scope` from the kind and rewrites the single
pointer into its scope slot; terminal pointed operations leave all slots
empty. A dangling pointer or an unknown kind aborts the whole migration
transaction (SQLite raises through temporary triggers; PostgreSQL through a
DO block) so no half-migrated state is accepted. Migration must run under the
same rollout rule above: stop old writers first; new readers fail closed on
unmigrated rows instead of guessing a scope. Operation leases follow the same
scope: dev operations require builder-family runtime receipts
(`acquire_builder_family_operation`), prod/application the app family.
