# v0.1 release-candidate evidence

Recorded 2026-08-26 on macOS ARM64. This document distinguishes repeatable local evidence from
evidence that can exist only after an immutable release is published.

## Server

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

## Live AutoPkg acceptance

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

## Public Rust client

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

## CLI and worker control

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

## Evidence still required for publication

- Publish the server tag/image from green multi-platform CI and record its immutable digest and
  provenance.
- Re-run the client and CLI live suites against that exact digest, then replace every
  `pending-server-release` marker and publish in server, client, CLI order.
