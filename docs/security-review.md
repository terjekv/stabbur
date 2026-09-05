# v0.1 security review

Review date: 2026-08-26. Scope: server API, SQLite adapter, local artifact store, outbound worker,
AutoPkg adapter, container, and release dependency graph.

## Trust boundaries

| Boundary                     | Server guarantee                                                          | Operator responsibility                             |
| ---------------------------- | ------------------------------------------------------------------------- | --------------------------------------------------- |
| Public client to API         | Bearer authentication, RBAC, validation, request IDs, safe problems       | TLS, rate limits, secure credential storage         |
| API to persistence           | Backend-neutral operations, explicit transactions, no row leakage         | Owner-only disk, coordinated backup                 |
| API to artifact store        | Digest/size verification, private paths, atomic publication               | Capacity monitoring, encrypted backup               |
| Server to worker             | Server-issued identity, capability ceiling, leases, idempotent completion | Protect credential, supervise process, TLS          |
| Worker to source/build tools | Pinned commits, no shell, scrubbed environment, output confinement        | Egress policy, source review, disposable build host |

## Authentication and authorization

- Passwords use Argon2id through one policy. Login bounds username/password inputs and performs a
  dummy verification for unknown users to reduce the username timing oracle.
- Login sessions are short-lived. Expired/revoked records are cleaned during session issuance.
  Password changes, principal disabling, reset, and explicit revocation invalidate credentials.
- API and worker tokens are random, stored only as hashes, redacted in `Debug`, and returned once.
  Token names are unique per principal.
- Built-in and custom roles resolve from storage for every authenticated request. Permissions are
  checked at the handler boundary. Worker credentials authenticate only the internal protocol.
- Bootstrap is one-time, atomically disabled after the first administrator, stored in a mode-0600
  file, and never logged. Local break-glass mutations require an exclusive SQLite process lock.
- Identity, worker, catalog, artifact, run, promotion, cancellation, and break-glass mutations
  append actor/request evidence in the same transaction where publication correctness requires it.

Residual risk: the application has no in-process account/IP rate limiter. A production proxy must
rate-limit login and bootstrap, and operators should alert on repeated authentication failures.

## Input, path, and process safety

- JSON bodies are limited to 2 MiB. Page limits are 1-200. Log entries are 64 KiB and batches are
  1 MiB. Installation metadata is two objects totaling at most 256 KiB.
- Recipe catalog manifests are canonical, content-addressed, limited to 1 MiB, and accepted only
  from an authenticated worker advertising the matching `builder.*` producer capability. Text and
  diagnostics are bounded and reject control characters; publications expose no worker paths.
- Artifact bodies stream through bounded channels. Content length, calculated size, SHA-256, media
  type, active attempt, selected role, and immutable metadata are verified before publication.
- CAS object paths are derived only from validated lowercase SHA-256 digests. Uploads use private
  random names, fsync, and atomic hard-link publication. Existing objects are size-verified.
- Recipe Git URLs must be absolute HTTPS without user information, query, or fragment and commits
  must be full lowercase hashes. Source, input, entrypoint, variant, artifact, verification, pointer,
  and raw-report sizes/counts have explicit bounds. Entrypoints and input names cannot inject command
  options. Git runs without system configuration, prompts, credentials, or an inherited environment.
- AutoPkg is invoked directly without a shell under isolated home/cache/source/work directories.
  Recipe and run keys that look secret are rejected. Selected output must canonicalize to a regular,
  non-symlink file under the attempt root.
- Entrypoints are resolved through a bounded non-symlink scan. Full pinned commits are the trust
  root for base recipes; overrides must additionally pass AutoPkg parent-trust verification. The
  adapter alone supplies the reserved trust result consumed by publication policy.
- AutoPkg installation is outside the worker protocol. The explicit macOS-only preparation command
  accepts only a bounded strict local manifest and non-symlink package, verifies lowercase SHA-256
  and an explicit signature policy, copies into private root-owned staging, invokes fixed system
  tools without a shell or inherited environment, bounds health-check output and time, and
  publishes its owner-only idempotency receipt only after the expected package receipt and exact
  health check succeed. Developer ID policy matches the leaf team exactly. Unsigned policy accepts
  only `pkgutil`'s exact unsigned status and provides integrity from the pinned digest but no
  publisher identity. A different installed identity requires an exact receipt-digest precondition;
  versions remain opaque, and neither the server nor a claimed job can invoke preparation.
- Processor receipts are size/count bounded. Persisted receipt/report paths are attempt-relative or
  redacted, and a streaming redactor removes private attempt roots from ordered logs even when a
  path is split across process reads. Nonstandard AutoPkg executables must be absolute, executable,
  and not writable by group or world.
- Builder messages are versioned and builder-neutral. The server revalidates run binding, recipe
  selectors, provenance, required verification, recipe trust, uploaded digests, and readable primary
  location before transactional candidate publication.
- Catalog snapshots are append-only observations. Publication cannot create a recipe revision,
  target, run, or job, so discovered third-party content never crosses the reviewed desired-state
  boundary implicitly.

Residual risk: HTTPS alone does not prevent an operator-authorized source hostname from resolving
to an internal address or a processor from making arbitrary outbound requests. Run workers on a
segmented, disposable host with DNS/egress allowlists and no control-plane database or host secrets.

## Availability and concurrency

- Jobs are at-least-once with attempt identities, expiring leases, heartbeats, maximum attempts,
  idempotency records, and transactional terminalization. Worker disabling, capability reduction,
  rotation, and run cancellation invalidate active work.
- SQLite uses WAL, foreign keys, a five-second busy timeout, short `BEGIN IMMEDIATE` writes, and
  rollback-on-drop. Race tests prove one winner for concurrent lease claims and restart recovery.
- Worker execution has a server-issued deadline between 60 seconds and 24 hours. Heartbeats continue
  during log flush, output selection, hashing, and upload. Loss of lease prevents acceptance.
- A worker can inspect its exact local advertisement before enrollment. The control plane rejects
  every detected capability outside its explicit ceiling and the runtime reports bounded problem
  code, request ID, and safe detail for operator diagnosis.
- Artifact and log paths apply bounded backpressure rather than buffering full content.

Residual risk: a principal with `artifact:write` can consume disk with a valid large object. Apply
storage quotas/capacity alerts and grant write/execute permissions only to trusted operators.

## HTTP and privacy

- Problems withhold database errors, filesystem paths, credentials, cache layout, and internal
  builder details. Correlation IDs are bounded and propagated.
- Artifact responses implement exact lengths, one range, strong digest ETags, immutable caching,
  and explicit `416`. Authorization is checked before metadata or content access.
- The listener intentionally provides HTTP/1.1 only. TLS, HSTS, request rate limits, forwarding
  header normalization, header timeouts, and generic security headers belong at the reverse proxy.
- Audit and provenance can contain operator-supplied non-secret data and upstream report data.
  Access is permission-controlled, but retention and personal-data policy remain operational duties.

## Dependency and build evidence

- `cargo audit` passes with no ignored advisories.
- `cargo deny check` passes advisories, license allowlist, bans policy, and source policy.
- Only `sqlx-core` and `sqlx-sqlite` are linked. MySQL, PostgreSQL, and the unused RSA package are
  absent from the lockfile graph.
- Lockfile builds, warnings-denied Clippy/rustdoc, MSRV, multi-platform tests, manifest parity,
  OpenAPI drift, and an unprivileged container build are CI gates.
- The container uses pinned base-image digests and an unprivileged UID/GID. Release publication
  should additionally attach the CI provenance and immutable image digest.

## Re-run checklist

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo run --locked -- openapi --check
cargo deny check
cargo audit
cargo tree --target all | rg 'sqlx-(mysql|postgres)|(^| )rsa v'
```

The final command must produce no matches.
