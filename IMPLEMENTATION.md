# Implementation boundary

The server v0.1 feature surface is implemented. This document identifies the architectural seams
that must remain stable as later adapters and clients are added.

## Runtime composition

- The root application crate owns Actix, configuration, error conversion, authorization selection,
  OpenAPI composition, process roles, and local administrative commands.
- Domain, authentication, jobs, builder, storage-port, and store-port crates do not depend on
  Actix, SQLx, application configuration, or AutoPkg implementation details.
- `stabbur-storage-runtime` is the only backend selector. It returns an opaque complete `Storage`
  implementation and fails closed for adapters not linked into the binary.
- `stabbur-storage-sqlite` owns SQL, migrations, row mapping, WAL behavior, transaction mechanics,
  process locking, and SQLite-specific recovery.
- `stabbur-builder-autopkg` owns pinned-source materialization, the direct AutoPkg process, report
  parsing, output containment, and adapter-specific evidence translation.

## Released server behavior

- UUIDv7 identities, lowercase software slugs, opaque non-ordered versions, numeric Apple-style
  macOS compatibility, deterministic resolution, and explicit ambiguity errors.
- One-time bootstrap, Argon2id local passwords, short sessions, named API tokens, service and
  worker principals, built-in and custom roles, permission enforcement, and append-only auditing.
- Complete principal, token, role, software, recipe, run, job, worker, store, artifact, release,
  variant, channel, promotion, rejection, cancellation, and resolver operations under `/api/v1`.
- Optimistic concurrency for mutable resources, cursor pages, scoped atomic idempotency for run
  creation/cancellation and worker completion, and stable problem responses with request IDs.
- Filesystem CAS ingestion with streaming SHA-256/size validation, fsync, atomic publication,
  deduplication, independently tracked locations, and bounded full/range HTTP streaming.
- Outbound worker provisioning, capability ceilings, registration, compatible lease claims,
  heartbeats, durable exact-byte logs, server-issued execution timeouts, retries, cancellation,
  credential rotation, and disabling with active-attempt invalidation.
- Immutable AutoPkg source commits and selectors, secret-like input rejection, isolated process
  state, scrubbed Git/process environments, safe output containment, worker uploads, server-side
  rehashing, provenance and verification persistence, and transactional candidate gating.
- SQLite migration, transaction, query-plan, lease-race, retry, and restart tests; Actix in-process
  tests; a real-socket 64 MiB streaming/range test; OpenAPI operation coverage; and supply-chain
  policy checks.

## Deliberate v0.1 limits

- SQLite and the private local primary artifact store are the only linked production adapters.
- Production TLS, request-rate controls, and general HTTP hardening headers are supplied by the
  deployment reverse proxy.
- AutoPkg source URLs are operator-reviewed HTTPS URLs. Network egress restriction and source
  allowlisting belong at the worker network boundary.
- Recipe and run values cannot contain secrets. External encrypted secret providers are later work.
- Local `admin` mutations are break-glass only and require exclusive SQLite process access.
- The public Rust client and CLI are separate releases and are not workspace members.

See [storage architecture](docs/architecture/storage-boundary.md),
[API conventions](docs/api-conventions.md), and
[distributed worker operations](docs/operations/workers.md).
