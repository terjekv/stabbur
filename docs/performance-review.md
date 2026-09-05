# v0.1 performance review

Review date: 2026-08-26. The v0.1 objective is predictable bounded resource use and correct queue
behavior for a small SQLite control plane, not a synthetic throughput claim.

## Artifact streaming

- Filesystem reads use Tokio file streams and `take(length)` for ranges; response bodies are never
  collected by the server.
- HTTP uploads cross the Actix/Send boundary through a bounded channel of eight chunks. The store
  hashes and writes one received chunk at a time.
- Worker hashing uses a 64 KiB buffer and upload uses a streaming file body. AutoPkg output remains
  on disk through hashing and transfer.
- AutoPkg revisions allow at most 16 sources, 128 inputs, 64 variants, 16 artifacts per variant,
  and 128 verification selectors. A report plist and each of at most eight processor receipts are
  individually capped at 512 KiB; the normalized combined JSON is capped at 1 MiB. Recipe discovery
  scans at most 50,000 non-symlink tree entries to depth 32.
- `artifact_socket` sends a generated 64 MiB object in 64 KiB chunks over a real TCP socket,
  verifies publication, checks an exact bodyless `HEAD` content length, then reads a suffix range.
  This exercises client, Actix, CAS, and response streaming without constructing a 64 MiB request
  buffer.

Memory therefore scales with bounded transport/file chunks and concurrent streams, not artifact
size. Multi-gigabyte behavior uses the same paths; production validation should run the socket test
with deployment-specific proxy and filesystem limits before raising concurrency.

## Logs and SSE

- AutoPkg stdout/stderr drains in 16 KiB chunks into a bounded 64-item channel.
- Streaming path redaction retains at most the longest private-root length minus one byte per
  stdout/stderr stream, so matches crossing read boundaries do not require buffering whole logs.
- The worker submits at most 32 chunks or about 512 KiB per batch. The API enforces 64 KiB per
  entry and 1 MiB per request.
- Logs receive server-assigned monotonic sequences in short transactions. Pagination reads at most
  200 rows. Run and job collection queries deliberately omit potentially 2 MiB parameter, result,
  raw-report, and builder-envelope fields; clients retrieve those only from detail endpoints. SSE
  polls durable storage, replays from a cursor/`Last-Event-ID`, sends keepalives, and closes after a
  terminal event.

This trades some database polling for restart/replica correctness and bounded process memory.
Monitor polling load before running many thousands of simultaneous watchers.

## SQLite transactions and indices

- WAL permits readers during writes. Foreign keys are enabled, the busy timeout is five seconds,
  and normal service pools use at most five connections.
- Mutations acquire `BEGIN IMMEDIATE` only after request parsing and domain validation. Network,
  hashing, AutoPkg execution, and artifact transfer occur outside database write transactions.
- The rollback guard retains a pooled connection until asynchronous rollback completes, preventing
  a leaked transaction from racing a later writer.
- Critical tests use `EXPLAIN QUERY PLAN` to require indices for token lookup, queued compatible
  job claim, expired-attempt recovery, due build targets, and release paging. The consolidated
  baseline also indexes location state, catalog source recency, promotion history, and lifecycle
  history.
- Role resolution uses one joined query rather than one query per role.

SQLite serializes writers. If measured write contention becomes the limiting factor, implement the
complete PostgreSQL adapter and run the same conformance suite; do not leak database-specific
behavior into application services.

## Queue fan-out and recovery

- Compatible claims filter capabilities inside SQLite with `json_each`; an early page of
  incompatible jobs cannot starve later compatible work.
- A regression test places one compatible job after 150 incompatible jobs and requires it to be
  claimed.
- A concurrent-claim test starts two workers against one job and requires exactly one active lease.
- A durable reopen test expires a lease, closes the database, reopens it, and requires another
  worker to reclaim the job.
- Attempts have indexed expiry, bounded lease durations of 30-300 seconds, and at most three
  attempts for API-created v0.1 runs.

## Catalog and resolver

Collection endpoints are cursor-bounded to 200. Resolver work is scoped to the variants of one
release and performs deterministic in-memory filtering over validated compatibility records.
Exactly one primary artifact is required before storage resolution. The expected v0.1 catalog size
does not justify a cache whose invalidation would weaken promotion correctness.

Worker-published recipe catalog manifests are capped at 1 MiB, 50,000 sorted entries, and 2,000
sorted diagnostics. Snapshot list responses omit the manifest; only detail lookup returns it.
Exact recipe lookup uses SQLite JSON inspection only across the latest snapshot for each indexed
producer/source pair.

For large deployments, record p50/p95 query latency and variant counts per release before adding a
cache. Ambiguity and lifecycle decisions must remain transactionally authoritative.

## Operational measurements

Track at minimum:

- SQLite busy/locked failures, transaction latency, WAL size, and database/store filesystem usage;
- queued jobs by required capability, claim latency, expired leases, retries, and terminal failures;
- worker heartbeat latency and upload throughput;
- artifact request concurrency, bytes, range ratio, and proxy buffering; and
- log ingest rate, active SSE watchers, and page/poll latency.

Alert before disk exhaustion, on repeated lease expiry, and on sustained SQLite busy failures.
Performance changes must preserve the real-socket, query-plan, queue-starvation, race, restart, and
storage conformance tests.
