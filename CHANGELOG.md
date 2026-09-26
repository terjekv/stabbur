# Changelog

All notable changes to Stabbur server are documented here.

## [0.0.1] - 2026-09-26

- Target the first coordinated release as 0.0.1.
- Update rustls to 0.23.45 to address RUSTSEC-2026-0285 without advisory exceptions.

### Fixed

- Explain that macOS test console login requires the exact printed Management UI address.
- Show macOS test installation options in top-level help and identify conflicting loopback ports
  with an actionable `--api-port` / `--web-port` hint.
- Associate invalid software slugs with their input field without changing the public error code.
- Extend coordinated browser acceptance to linked operator workflows, inline validation, catalog
  review, named build controls and small-screen navigation. Document the first-delivery workflow.

### Added

- Add opt-in AutoPkg worker inventory discovery with pinned parent source closures and safe import diagnostics. Publish import sources on immutable catalog snapshots and normalize single-run receipt output variables for reviewed imports.

- Add single-host macOS acceptance across all four repositories: headless management workflows,
  deterministic real AutoPkg delivery, guarded Munki install/detection, backup restore, crash/lease
  recovery and withdrawal. CI pins tool packages and uploads only credential-free evidence.

- Add publication availability separate from attained lifecycle, append-only release withdrawal,
  worker draining, recurring-work coalescing, and software/queue status read models.
- Distinguish unchanged output with changed evidence from actual version-content conflicts.
- Add transactional recipe append-sequence preconditions and a forward operator-workflow migration.
- Add bounded offline store integrity/free-space inspection and expired upload cleanup.
- Add cross-repository integration for the client, CLI and separate management console.
- Preserve validated facts in private proof types and contributor guidance across all projects.

- Add idempotent, noninteractive Linux server and macOS worker host installers with atomic binary
  replacement, strict account/path validation, explicit activation, and guarded credential
  rotation, plus disposable-runner CI installation coverage.
- Add an executable default deployment runbook for a hardened systemd-managed Linux control plane
  and separately supervised launchd macOS AutoPkg worker, plus a single-host macOS variant and a
  live-evidence-backed first repository example.
- Ship validated systemd and launchd service definitions, plus a hardened nginx starting point, for
  the supported filesystem/SQLite server and native macOS worker topology.
- Add a deterministic macOS live E2E that starts fresh API and outbound-worker processes, performs
  public bootstrap and worker provisioning, verifies exact capability registration, completes a
  durable fake run/job, and runs in the macOS ARM64 CI leg.
- Specify and support versioned, additive catalog manifest reconciliation through the independent
  supported client and CLI without implicit execution.
- Add atomic opaque-version finalization outcomes: create a release once, accept evidence-identical
  rebuilds as `no_change`, and fail different same-version output as `version_content_conflict`
  without mutating release lifecycle or channel history.
- Add persisted manual and fixed-interval build targets with ETag updates, append-only snapshots,
  target-linked run history, idempotent manual triggers, and a concurrent-instance-safe server
  scheduler exercised by the real macOS server/worker E2E.
- Add bounded, canonical builder-neutral recipe catalog manifests, authenticated idempotent worker
  publication, immutable snapshot inspection, and exact latest-source recipe lookup. Catalog
  observations never create revisions, targets, or jobs.
- Add a bounded AutoPkg catalog generator plus durable server-requested scan jobs, typed terminal
  history, atomic snapshot publication, cancellation, audit, and public client/CLI coverage.
- Add an idempotent root-only macOS `worker prepare` command for locally staged AutoPkg packages,
  with strict manifests, SHA-256 and explicit signature-policy verification, private staging,
  bounded non-shell health checks, expected package receipts, and atomic owner-only preparation
  receipts.
- Add a default fixture pinned to the official AutoPkg 2.9.0 release and a guarded disposable-macOS
  CI acceptance that verifies, installs, repeats, and capability-probes the real package.
- Record a clean opaque AutoPkg version separately from exact health output and require an exact
  installed-manifest digest precondition for upgrades, downgrades, or other replacements while
  retaining idempotent reruns and same-manifest repair.

### Changed

- Make public recipe-revision creation builder-neutral while retaining strict server-side
  validation for production AutoPkg definitions and empty deterministic fake definitions.
- Allow the exclusive local administrator bootstrap command to initialize a pristine database or
  recover a pending bootstrap whose raw secret file was lost, while still failing closed after any
  principal exists or bootstrap was permanently disabled.
- Collapse all pre-release SQLite migrations into one complete baseline. Existing development
  databases created from the discarded migration history must be recreated.
- Generalize durable jobs around an explicit build-run or recipe-catalog-scan subject while
  preserving optional subject identities in the public response.

### Security

- Keep worker tool installation outside the outbound daemon and control-plane job protocol;
  AutoPkg preparation requires an explicit local privileged command and exactly pinned inputs.
- Fail AutoPkg replacement closed when the local receipt is absent or differs from the manifest's
  explicit precondition; never infer ordering from version strings.

## Initial development - 2026-08-26

### Added

- Add the multi-crate server workspace, Actix runtime roles, narrow break-glass commands, committed
  OpenAPI 3.1 contract, and production container.
- Add validated UUIDv7 domain identities, lifecycle and compatibility rules, release variants,
  channels, deterministic resolution, promotion, rejection, and installation/detection metadata.
- Add one-time bootstrap, Argon2id authentication, short sessions, named hashed API tokens, worker
  identities, built-in/custom RBAC, principal administration, password recovery, and audit events.
- Add the backend-neutral complete storage contract, SQLite adapter and embedded migrations, WAL,
  exclusive break-glass locking, conformance tests, lease recovery, and query-plan evidence.
- Add immutable local CAS ingestion and serving with digest/size rehashing, atomic publication,
  independent locations, ETags, ranges, resumable reads, and bounded streaming.
- Add immutable pinned AutoPkg revisions, reviewed selectors, capability-matched remote workers,
  leases, heartbeats, ordered log/SSE replay, worker uploads, provenance, verification gates,
  automatic candidate publication, retry/cancellation, credential rotation, and disabling.
- Add multi-platform, MSRV, rustdoc, OpenAPI, container, advisory, license, race, restart, and
  real-socket streaming release gates.
- Add local worker capability inspection and bounded, actionable control-plane rejection details
  for remote-worker provisioning and troubleshooting.
- Add explicit worker-local AutoPkg executable selection, AutoPkg 2.9-compatible source search,
  bounded processor-receipt capture, pinned-base/override trust enforcement, and live macOS
  acceptance evidence.

### Security

- Scrub AutoPkg and Git subprocess environments, reject credential-bearing source URLs and
  secret-like recipe values, confine selected files to regular files under the attempt root, and
  revalidate every worker result and artifact on the server.
- Normalize private paths in persisted receipts and redact attempt roots from streaming logs,
  including matches split across process-read boundaries.
- Remove unused SQLx database drivers and their dormant cryptographic advisory from the lockfile.
- Keep native HTTP/2 disabled until the server dependency line consumes a patched implementation.
