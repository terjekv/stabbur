# Storage backend policy

Stabbur has two separate kinds of storage: relational persistence and immutable artifact stores.
Neither backend may change software, release, variant, channel, or artifact identity.

## Relational persistence

SQLite is the v0.0.1 default and the required single-host backend. It uses WAL, foreign keys,
embedded migrations, a bounded busy timeout, and short write transactions. It is deliberately
available without another service so `stabbur-server all` remains a useful small deployment.

PostgreSQL is the first planned external relational adapter. It will live in a separate
`stabbur-storage-postgres` crate, implement the complete `stabbur-storage-core` contract, own its
driver, SQL dialect, migrations, pooling, TLS, locking, and adapter errors, and receive dedicated
transaction, migration, concurrency, restart, and recovery tests.

MySQL is not enabled today, but it can be introduced later as a separate
`stabbur-storage-mysql` adapter without changing domain or application services. It must satisfy
the same complete contract and conformance suite as SQLite and PostgreSQL. The current adapter
links exactly pinned `sqlx-core` and `sqlx-sqlite` crates, so unused database drivers and their
transitive dependencies do not enter the lockfile.

Application services consume opaque aggregate- or operation-shaped storage capabilities. They
must not select a backend, name a pool, depend on SQLx, use SQLx `Any`, or contain dialect-specific
queries. Database rows and driver errors remain adapter-private.

Backend construction and selection live in `stabbur-storage-runtime`. The server supplies only a
backend name, an optional redacted database URL, and a local SQLite fallback path. Adding an
adapter changes its own crate and storage composition; it does not change HTTP handlers, workers,
domain crates, or the backend-neutral contract unless genuinely new portable behavior is needed.
See [storage boundary](storage-boundary.md).

## Artifact stores

The local filesystem CAS is independent of relational persistence. Future S3-compatible and
read-only HTTP stores implement `stabbur-store-core`; they do not require a database change.
Artifact locations remain independently tracked so later replication does not alter artifact
identity.
