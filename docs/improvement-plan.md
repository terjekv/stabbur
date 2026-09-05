# Operator workflow implementation

This tracks the coordinated server, supported Rust client, CLI, and new self-hosted management
console work authorized in September 2026. Published compatibility still requires immutable CI
evidence; local tests never establish a released-image claim.

- [x] Preserve validated facts as private proof types and document the rule in all four projects.
- [x] Reconcile the server contract and run one cross-repository integration scenario.
- [x] Preserve CLI pagination and make waits, deadlines, and SSE replay reliable.
- [x] Reconcile reviewed build targets with catalog schema 2 and propose pinned-source updates.
- [x] Add software summaries and operator diagnostics through the public contract.
- [x] Separate release availability from attained lifecycle and distinguish rebuild evidence.
- [x] Coalesce recurring work, support worker draining, and expose operational measurements.
- [x] Add bounded store integrity inspection and stale-upload maintenance.
- [x] Export promoted installers and metadata for Munki with a documented acceptance workflow.
- [x] Build a separate management console with upstream login, protected sessions, CSRF defense,
      constrained API access, safe output, and tests.
- [x] Split large modules along existing boundaries and complete the repository quality gates.

Implementation choices and validation evidence are recorded here as the work progresses.

## Implementation and evidence

The server adds the forward `0002_operator_workflows` migration and 75-operation public contract.
The client and console pin the same complete document. SQLite implementations now live in modules
matching their operation ports; API HTTP tests live separately. The client shares reconciliation
orchestration between transports; the CLI isolates verified downloads and Munki export.

Private proof types now preserve withdrawal reasons, normalized byte ranges, catalog manifests,
source pins, terminal event evidence, verified local downloads, maintenance budgets, configured
origins, authenticated console sessions, and reviewed gateway/revision requests. Append sequences,
ETags, durable leases, transactions and database constraints still enforce changing shared state.

The disposable cross-repository test passed the independent public-client consumer, CLI pagination
and schema 2 convergence, and console login, origin/CSRF rejection, role enforcement, logout and
upstream revocation. Fault-injected tests cover interrupted SSE replay, deduplication, failure and
cancellation, hard deadlines, corrupt-download preservation, and cursor cycles. Storage conformance
covers drain/active-lease behavior, stale appends, changed evidence, stable withdrawal, status reads
and migration preservation. Integrity maintenance has bounded-budget and retention tests.

All four advisory/license checks pass without exceptions. The client passes Rust 1.88 and all four
feature combinations. See the individual repository contributor gates and logs from this session
for test, formatting, Clippy, rustdoc, contract and release-build checks.

The September 5 local macOS acceptance passed with real AutoPkg 2.9.0, Munki 7.2.0 catalog
generation and headless Chromium. It exercised catalog review/apply, real build logs, promotion,
withdrawal, logout/login, UI expiry, complete backup restore, server crash recovery and stale-lease
fencing. It caught and fixed CLI resolver/Munki architecture naming and console rendering of
nested AutoPkg report arrays. [Recorded evidence](evidence/single-host-local-2026-09-05.json)
explicitly distinguishes this run from the disposable install/detection leg configured in CI.

## Practical limits

- The repeatable [single-host macOS suite](operations/single-host-e2e.md) now includes isolated
  headless browser acceptance against real AutoPkg data. It is independent of an attached user
  browser. Backend authentication integration and JavaScript boundary tests remain separate gates.
- This is development evidence. No image, package, repository or site was remotely published, and
  immutable released-server acceptance remains required before compatibility claims.
- Munki output is a reviewed delivery snapshot. A real package install/detection cycle on a test Mac
  remains operator acceptance; the exporter does not install software or modify a live Munki service.
- Catalog application consists of conditional API mutations, not a distributed transaction. Re-plan
  after partial failure. Withdrawals do not retract already exported delivery snapshots.
