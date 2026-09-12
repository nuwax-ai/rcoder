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
