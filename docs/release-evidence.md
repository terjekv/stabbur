# v0.0.1 release evidence

Published 2026-09-26. The first supported release includes the server image/source, public Rust
client source/crate package, and CLI platform archives. The independent console is verified at
its exact source revision. This evidence remains tied to immutable release sources.

## Published components

- [Server 0.0.1](https://github.com/terjekv/stabbur/releases/tag/v0.0.1): source
  `d3a64710d608e19a14c4bb360bffe18e343bd7a6`, OpenAPI contract and image evidence.
- [Public Rust client 0.0.1](https://github.com/terjekv/stabbur-client-rust/releases/tag/v0.0.1): source
  `802b60e653b75b3694b0ed4607a6f61a139ba093` and verified `stabbur_client-0.0.1.crate` package.
  This is a GitHub source/package release; the crate is not published to crates.io.
- [CLI 0.0.1](https://github.com/terjekv/stabbur-cli/releases/tag/v0.0.1): source
  `7e498ec5c9f3f6532404934cb06449b1712887e4`, static Linux x86_64/aarch64 archives,
  self-contained macOS ARM64 and static-CRT Windows x86_64 archives, and SHA-256 checksums.
  Its manifest and lockfile pin the exact released client Git revision.
- [Console source](https://github.com/terjekv/stabbur-frontend/tree/028d664e4e0544c0e41b553ae11454f92ad2dc1a):
  `028d664e4e0544c0e41b553ae11454f92ad2dc1a`, with green console CI and browser acceptance.

## Immutable image and contract

The publicly pullable Linux amd64 image is:

```text
ghcr.io/terjekv/stabbur-server@sha256:41aba0e2051de7d74df09a6e57487a4e1755706d587aca54603167953fd0748d
```

The [publication run](https://github.com/terjekv/stabbur/actions/runs/36231611736) emitted full BuildKit
provenance. An anonymous registry read independently verified the OCI index digest and its
SLSA v1 provenance attachment. The released OpenAPI asset matches the committed 75-operation
contract byte for byte:

```text
SHA-256: 4a979bb27b5e4651320f87d7345995bd24868aa2a0cb9a072936f70dffda4901
```

[Server evidence](evidence/server-0.0.1.json) records all four exact source revisions and successful
main CI runs used for image publication. Client, independent downstream consumer, CLI and console
workflows passed against the returned image digest before the server source release was created.

The final consumer release commits passed their own complete main CI and repeated live acceptance
against the same image:

- [Client CI](https://github.com/terjekv/stabbur-client-rust/actions/runs/36232101504) and
  [client acceptance](https://github.com/terjekv/stabbur-client-rust/actions/runs/36232167363);
  [recorded evidence](evidence/client-0.0.1.json).
- [CLI CI](https://github.com/terjekv/stabbur-cli/actions/runs/36232301357) and
  [CLI/browser acceptance](https://github.com/terjekv/stabbur-cli/actions/runs/36232688132);
  [recorded evidence](evidence/cli-0.0.1.json).

The CLI release promotes the exact checksummed platform archives from its successful main CI.
All four published archives were downloaded and independently verified against their checksums;
the released macOS binary returned `stabbur 0.0.1`. The
[archive manifest](evidence/cli-artifacts-0.0.1.json) preserves names, sizes, hashes and download URLs.
Published tags are never moved; later main commits skip publication for an already released
version. A new release requires a new package version and fresh acceptance.

## macOS delivery and recovery

The [disposable macOS acceptance](https://github.com/terjekv/stabbur/actions/runs/36231299162) passed
pinned upstream AutoPkg catalog discovery, console import and review, real execution, immutable
artifact publication, Munki export, actual installation and detection, browser management,
withdrawal, backup restoration, process-crash recovery, and lease fencing. The Ubuntu integration
job passed in the same run. The installed macOS test uses separate server and outbound worker
processes on one host; it does not claim separate-host network acceptance.

[Local macOS evidence](evidence/single-host-local-2026-09-26.json) separately records the developer
run. Its source was dirty and installation was skipped; the clean hosted installation run above
supplies the release acceptance proof.

## Quality gates

All required formatting, warnings-denied Clippy/rustdoc, workspace tests, Rust 1.88 MSRV,
OpenAPI reconciliation, client feature combinations, dependency/license/advisory, workflow,
Markdown, browser and production-container checks passed. Server CI covered Linux x86_64 and
ARM64, macOS ARM64 and Windows x86_64. CLI CI passed stable/beta/nightly platform tests plus all
four package/linkage jobs. Rustls is pinned to patched 0.23.45 in all four lockfiles; neither
advisory tool uses exceptions, and unused SQLx database drivers remain absent.

## Historical development evidence: 2026-08-26

Recorded 2026-08-26 on macOS ARM64. This document distinguishes repeatable local evidence from
evidence that can exist only after an immutable release is published.

### Server

The following completed against the final source tree:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo run --locked -- openapi --check
cargo +1.88 check --workspace --all-targets --all-features --locked
python3 scripts/check-docker-manifests.py
cargo deny check
cargo audit
actionlint .github/workflows/*.yml
shellcheck entrypoint.sh scripts/*.sh
python3 -m py_compile scripts/*.py
markdownlint-cli2
```

The suite includes the complete shared storage-adapter contract, SQLite migration/reopen and
query-plan checks, concurrent lease races, capability starvation, API/RBAC/lifecycle/resolver
workflows, and a real-socket 64 MiB upload, bodyless `HEAD`, and range response.

The macOS CI leg also runs `scripts/live-worker-e2e.sh`. That black-box fixture starts a fresh API
and a separate outbound worker, bootstraps and provisions through public HTTP, requires the exact
locally detected capability advertisement, and verifies a fake revision through durable target
scheduling, run/job enqueue, outbound claim, one attempt, completion, and audit creation events.
The worker also publishes a builder-neutral recipe catalog snapshot and the script resolves and
loads it through the public catalog APIs before scheduling the build. The updated fixture also
queues a server-requested AutoPkg scan, verifies outbound claim, and records its expected typed
materialization failure against a deliberately unreachable local source.

A clean production image build exposed and fixed a cache-stage placeholder substitution bug. The
corrected image built at Rust 1.88 inside the pinned Alpine builder, ran as UID/GID 10001, returned
`{"status":"ok","version":"0.1.0"}`, served its embedded OpenAPI document, and created:

```text
700 10001:10001 /var/lib/stabbur
600 10001:10001 /var/lib/stabbur/bootstrap.secret
```

The local smoke image identity was
`sha256:10169316ae7caa0e38c409fe82905203b349a82ff8d6e188da6762c81881ee36`.
This is local evidence only and must not be copied into a published compatibility claim.

### Live AutoPkg acceptance

The opt-in macOS workflow passed against the final adapter on 2026-08-26. A fresh loopback control
plane provisioned an outbound-only worker, materialized
`https://github.com/autopkg/recipes.git` at full commit
`6c092b47e9c6324aa48758832b2597a0f3ff932e`, invoked AutoPkg 2.9.0 directly, captured its bounded
processor receipt, and uploaded the selected package. The server independently rehashed
52,253,994 bytes as
`b858161c4fe20429127a0429cdf1e6e1e2cca66b1b5ec2f81a2b98933b0a66f2`, recorded a verified
`present` primary location, completed required recipe-trust verification, created release 2.9.0,
and bound `candidate` transactionally.

The test used a SHA-256-verified temporary extraction of the official AutoPkg 2.9.0 package; that
package reports as unsigned and was not installed or allowed to load its launch daemons. Persisted
reports contained only attempt-relative paths, persisted logs replaced the private root with
`[attempt]`, and an evidence audit found no credential fields. The compact attestation is
[autopkg-live-2026-08-26.json](evidence/autopkg-live-2026-08-26.json); the repeatable input is
`tests/fixtures/autopkg-live/autopkg-release.json`.

### Public Rust client

All contributor gates passed: 70-operation reconciliation, warnings-denied Clippy and rustdoc,
async/blocking/no-default feature combinations, Rust 1.88, normalized OpenAPI validation,
dependency/license/advisory checks, Markdown/workflow validation, and verified `cargo package`.
Tests cover typestate compile failure, UUIDv7/digest validation, secret redaction, encoded paths,
ETags, constrained `raw()`, bounded declared responses, server problems, and an independent
downstream consumer. Additive mock contracts cover typed desired build targets and pinned durable
catalog-scan requests, including pre-transport source validation.

The local live workflow covered authentication, software, recipes, principals/tokens, worker
provision/rotation/disable, artifact streaming/`HEAD`/locations/download, stores, runs, jobs, and
audit against this server candidate.

### CLI and worker control

Formatting, warnings-denied Clippy, tests, optimized build, Rust 1.88, rustdoc,
dependency/license/advisory, Markdown, and workflow validation passed. Tests cover the complete
command catalog, exact client/no-direct-HTTP dependency, JSON output stability, gateway requests,
confirmation before network mutation, owner-only exclusive credential files, and secret redaction.
The `release-artifacts` Docker target also built and executed the static Linux ARM64 binary at
Rust 1.88 (`stabbur 0.1.0`). The remaining Linux x86-64 and Windows target artifacts are assigned
to their native CI matrix jobs.

A CLI-provisioned JSON credential successfully started an outbound-only worker. The worker's local
inspection and server record agreed on `builder.fake`, `os.macos`, `runtime.portable`, and
`tool.apple-xcode`; registration updated `last_seen_at` without an inbound worker listener. A
deliberately incomplete capability ceiling was rejected before the ceiling was corrected.
The deterministic server-owned form of this process-boundary check is now automated by
`scripts/live-worker-e2e.sh` and the macOS CI leg.

### Publication work outstanding on 2026-08-26

- Publish the server tag/image from green multi-platform CI and record its immutable digest and
  provenance.
- Re-run the client and CLI live suites against that exact digest, then replace every
  `pending-server-release` marker and publish in server, client, CLI order.
