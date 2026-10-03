# Stabbur server

Stabbur is a control plane for building, verifying, publishing, resolving, and serving immutable
software artifacts. This repository builds one runtime binary:

```text
stabbur-server api
stabbur-server worker
stabbur-server worker --print-capabilities
stabbur-server worker prepare --manifest /absolute/path/autopkg-installer.json
stabbur-server all
stabbur-server admin bootstrap
stabbur-server admin reset-password USER
stabbur-server admin revoke-sessions USER
stabbur-server admin migrate
stabbur-server admin doctor
stabbur-server catalog generate-autopkg --source-url URL --source-revision COMMIT
stabbur-server openapi
```

`all` runs the API with an embedded portable fake builder and is the smallest installation. A
production AutoPkg executor runs `worker` on macOS and makes outbound HTTPS connections to the API.
The server assigns work through durable capability-matched leases; it never opens a remote shell or
manages the worker host.

macOS automation can prepare an exactly pinned AutoPkg package through the explicit, root-only
`worker prepare` command. The long-running worker never downloads, installs, or upgrades its own
tools. Replacements require the exact installed-manifest digest and never infer ordering from the
opaque version. The repository includes a pinned AutoPkg 2.9.0 fixture and disposable macOS
acceptance flow. See the [worker operations guide](docs/operations/workers.md#prepare-autopkg-on-macos).

## v0.0.1 server scope

The server v0.0.1 implementation includes:

- local human and service identities, Argon2id passwords, short sessions, hashed API and worker
  tokens, custom RBAC roles, one-time bootstrap, and append-only audit events;
- software installation/detection metadata, releases, lifecycle history, variants, channels,
  explicit promotion and rejection, and deterministic compatibility resolution;
- immutable SHA-256 artifacts, independently tracked locations, verified streaming ingestion,
  single-range HTTP serving, strong ETags, and a private filesystem content-addressed store;
- builder-neutral immutable revisions with strictly validated fake and pinned AutoPkg definitions,
  reviewed output selectors, isolated no-shell execution, durable ordered logs and SSE replay,
  server-issued deadlines, leased retries, cancellation, provenance, verification gates, and
  atomic create/no-change/same-version-conflict decisions with automatic candidate publication;
- persisted manual or fixed-interval build targets, append-only configuration/cursor history,
  idempotent manual triggers, and a concurrency-safe server scheduler;
- immutable, content-addressed recipe catalog observations published by authenticated workers,
  durable scans of exact pinned sources, exact latest-source lookup, and no implicit recipe
  creation or execution;
- backend-neutral aggregate-shaped storage ports, SQLite WAL persistence, embedded migrations,
  restart recovery, race-tested leases, and narrow exclusive break-glass commands; and
- a committed OpenAPI 3.1 contract plus Linux, macOS, Windows, MSRV, container, rustdoc, dependency,
  license, security, and streaming test gates.

The supported Rust client, user-facing `stabbur` CLI, and optional browser management console
are implemented in independent, independently versioned repositories. The console supports
software, build, release, and publication workflows alongside the CLI. Their release status does
not change the server API contract; see
[compatibility](COMPATIBILITY.md) and the [post-release priorities](docs/spec-gap-analysis.md).

## Management console and operator workflows

The independent [`stabbur-frontend`](https://github.com/terjekv/stabbur-frontend) repository provides a self-hosted
management console using server-held Stabbur login sessions. See [operator workflows](docs/operator-workflows.md) and [operator scale decisions](docs/architecture/operator-scale.md)
for catalog plans, source-pin proposals, status views, draining, withdrawal, storage inspection and
Munki export. The supported client, CLI and console pin the same 75-operation public contract.

## Quick start

For a complete persistent macOS test setup, including the worker, CLI and management UI, run
`python3 scripts/install-test-macos.py install`. See the
[macOS test installer guide](docs/operations/macos-test-setup.md) for prerequisites, configurable
directories, optional AutoPkg installation and start/stop commands.

```bash
cargo build --release --locked
install -d -m 0700 /var/lib/stabbur
./target/release/stabbur-server all \
  --data-dir /var/lib/stabbur \
  --bind 127.0.0.1:8080
```

On first start, the process writes a one-time owner-only bootstrap secret and logs only its path.
The user-facing CLI completes the public bootstrap operation without an existing login:

```bash
stabbur --server http://127.0.0.1:8080 bootstrap \
  --username admin \
  --bootstrap-secret-file /var/lib/stabbur/bootstrap.secret \
  --password-file /run/secrets/stabbur-admin-password
```

Automation may instead create the first administrator before initial startup, or recover a missing
pending secret, through the exclusive local command:

```bash
stabbur-server admin bootstrap \
  --data-dir /var/lib/stabbur \
  --username admin \
  --password-file /run/secrets/stabbur-admin-password
```

The local command runs embedded migrations and requires no bootstrap-secret file. It requires the
service to be stopped when the configured database already exists.

For the default split-host deployment, the repository ships idempotent noninteractive host
installers. They install a supplied native binary and the platform service definition but never
download Stabbur, handle an administrator password, or issue a worker credential:

```bash
sudo scripts/install-server-linux.sh \
  --binary target/release/stabbur-server
sudo scripts/install-worker-macos.sh \
  --binary target/release/stabbur-server \
  --server-url https://stabbur.example.net
```

Both stage changes by default. Rerun with `--start` only after local server bootstrap or worker
credential delivery and AutoPkg preparation. See the default runbook for the complete ordering.

Production deployments must put the HTTP/1.1 listener behind a TLS reverse proxy. The default
operator path is the
[Linux server and macOS AutoPkg worker runbook](docs/operations/linux-server-macos-worker.md); a
[single-host macOS variant](docs/operations/single-host-macos.md) is available for small
installations. Use the [deployment and recovery guide](docs/operations/deployment.md) for lifecycle
policy and the [worker and AutoPkg guide](docs/operations/workers.md) for protocol details. API
consumers should read the [API conventions](docs/api-conventions.md).

For local and GitHub Actions acceptance, use the
[single-host macOS E2E guide](docs/operations/single-host-e2e.md). It exercises all four projects,
real AutoPkg delivery, browser management, Munki export and recovery with disposable state.

## Storage boundary

SQLite is the v0.0.1 backend. Application and domain crates depend only on the complete
`stabbur-storage-core` contract; SQLx and database rows remain private to adapter crates.
PostgreSQL is the first planned external adapter. MySQL can be implemented later behind the same
contract and conformance suite without changing handlers, workers, or domain identities. See the
[storage boundary](docs/architecture/storage-boundary.md) and
[backend policy](docs/architecture/storage-backends.md).

Recipe discovery, reviewed desired state, and unattended build selection are specified separately
in [catalog discovery and scheduling](docs/architecture/catalog-and-scheduling.md). The server
persists manual or interval build targets against exact recipe revisions and schedules ordinary
durable runs from them. The supported client and CLI can reconcile software and recipes with
`stabbur catalog plan/sync`, inspect observations with `stabbur catalog`, and manage desired
execution with `stabbur target`. The server never implicitly executes every recipe visible in an
upstream AutoPkg repository.

`stabbur-server catalog generate-autopkg` generates the builder-neutral manifest from one exact
HTTPS Git commit for review or AutoCfg automation. A capable outbound worker can also claim a
durable public catalog-scan request, run that same generator, and publish the immutable snapshot
atomically. AutoCfg should exchange this neutral JSON or call the public API; it does not need a
server-side library integration. Discovery is advisory: only a separately reviewed immutable
recipe revision and enabled build target can create work.

## Development and release gates

The workspace uses Rust 2024 with MSRV 1.88. Run the same essential gates as CI:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo run --locked -- openapi --check
cargo deny check
cargo audit
```

The complete bar and recorded review evidence are in the
[v0.0.1 release checklist](docs/v0.0.1-release-checklist.md),
[security review](docs/security-review.md), and
[performance review](docs/performance-review.md).

On macOS, also run the deterministic live API/outbound-worker fake-job test:

```bash
cargo build --locked
scripts/live-worker-e2e.sh
```

The live fixture also exercises a server-requested catalog job through the separate outbound
worker and verifies its typed terminal history. The macOS CI leg also runs the pinned
`scripts/live-worker-prepare-autopkg.sh` fixture on its disposable host to install AutoPkg 2.9.0,
prove preparation idempotency, and probe the resulting worker capability. The separate opt-in
`scripts/live-autopkg-acceptance.sh` fixture performs a complete real AutoPkg build, artifact
ingestion, verification, and candidate-publication workflow.
