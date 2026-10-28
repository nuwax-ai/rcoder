# Upstream source and narrow patch

- Crate: `toasty-driver-postgresql` **0.11.0**, MIT.
- Official archive: https://static.crates.io/crates/toasty-driver-postgresql/toasty-driver-postgresql-0.11.0.crate
- Official archive SHA-256 (verified against crates.io): `b6cdb083715b23faddc3b667151cfda8b02feac02e0066acd933d8f8028fce4c`.
- Upstream commit: `d71114e4101ce33ee5272ef2bd69ac6d5a6d749a`; path `crates/toasty-driver-postgresql`.
- Original package metadata, source files and original lockfile are retained.
- `LICENSE` is the unchanged upstream repository license at that commit.

## Local API extension

Only `src/lib.rs` differs from the published crate: add `SupervisedConnection`
and `PostgreSQL::connect_supervised(connect_timeout)`. The method returns the
original `tokio_postgres::Client` and owned I/O-driver future using the original
parsed configuration and private TLS connector. It overrides only the configured
transport connection timeout. No TLS-mode parsing, trust policy, certificate
verification, ORM operation, schema, default feature, or existing connection
behavior changes.

The workload admission actor owns the future, polls it in its existing task,
and aborts and joins it before returning its bounded session slot. This is
required for advisory-lock ownership and uncertain commits; dropping a regular
Toasty connection alone cannot prove its detached I/O driver has exited.
All lock/canonical queries and registration-frame transactions remain on that
same raw client. There is no automatic reconnect or shared ORM pool checkout.

The root Cargo patch applies to the root workspace only. The independent
app-cli graph does not depend on this driver and receives no unused patch.
Remove the local extension when an upstream release exposes equivalent owned
session supervision; recheck the original archive and the tests before updating.
