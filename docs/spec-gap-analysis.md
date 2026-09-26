# Remaining v0.0.1 release work

The server v0.0.1 feature surface is implemented, including desired build targets, scheduling,
worker-observed recipe catalogs, and durable server-requested scans. The independently versioned
public client and CLI now ingest the 75-operation contract locally. Remaining release work is
immutable publication and cross-platform CI evidence for all four repositories.

## Server release-candidate evidence

- The full local release gate set and a production container smoke test passed on 2026-08-26; see
  [release evidence](release-evidence.md). Retain the CI multi-platform/container results for the
  tagged commit.
- The opt-in real macOS AutoPkg acceptance passed with a full pinned source commit, AutoPkg 2.9.0,
  Xcode 26.6, processor receipts, a 52,253,994-byte server-rehashed installer, verified primary
  location, and transactional candidate publication. See the committed compact attestation.
- Publish the server tag, immutable container digest, provenance, changelog, compatibility file,
  and the committed OpenAPI contract from one CI run.

The deterministic suite already covers selector translation and containment, exact-byte logs,
worker upload and server rehash, transactional gating, lease races and restart recovery, queue
starvation, cancellation, opaque-version create/no-change/conflict finalization, concurrent
build-target scheduling, worker credential lifecycle, identity administration, resolver policy,
OpenAPI operation coverage, and real-socket 64 MiB upload/range streaming.

## Public Rust client

The independent client pins the current 75-operation OpenAPI document, commits
reviewed/generated source, exposes typed async/blocking APIs and a constrained `raw()` extension,
bounds buffered responses and streaming downloads, and passes local mock, parity, compile-fail,
redaction, downstream-consumer, and live control-plane workflows. Before publishing it:

- replace `pending-server-release` with the immutable server image digest;
- run the same complete live suite against that image rather than a local binary; and
- record the image, server tag, OpenAPI hash, and immutable CI evidence in `COMPATIBILITY.md`.

## CLI

The independent CLI implements the complete command families through the exact blocking client,
with owner-only credentials, stable JSON and human tables, confirmations, live logs, and verified
resumable downloads. Its CI defines stable/beta/nightly builds on Linux, macOS, and Windows plus
static release targets and checksums. Before publishing it:

- replace the local exact client checkout with the released `stabbur_client` 0.0.1 tag/crate;
- pass the configured platform, MSRV, package, and supply-chain jobs; and
- exercise the full CLI workflow against the same immutable server image used by the client.

## Later work

- A separate PostgreSQL adapter is the first scale-out persistence addition. A possible MySQL
  adapter must satisfy the same complete port and conformance suite.
- S3-compatible and read-only HTTP stores, multiple locations and replication, manual uploads,
  directory migration, OIDC, encrypted secret providers, and richer native recipes
  remain post-v0.1.
- The independent browser console now lives in `stabbur-frontend`; it is never embedded in the server binary.
- The CLI now exports reviewed Munki snapshots; real test-device installation remains an acceptance gate.
