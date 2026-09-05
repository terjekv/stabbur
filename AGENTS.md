# Stabbur server contributor guidance

This repository owns the `stabbur-server` control plane and worker runtime. Keep the public
Rust client, user-facing CLI, and browser frontend in their independent repositories.

## Architecture

- Keep Actix, CLI parsing, configuration, and composition in the root application crate.
- Keep domain and port crates independent of Actix, SQLx, runtime configuration, and AutoPkg.
- Do not expose SQL rows, filesystem layouts, or third-party implementation types through
  Stabbur-owned traits.
- Persist lifecycle, promotion, recipe, run, job, authentication, and audit history append-only.
- Treat versions as opaque strings. Never infer SemVer ordering.
- Treat artifact bytes as immutable and identify them with lowercase SHA-256 digests.
- Preserve validated facts as types across architectural boundaries. Convert raw API and
  database representations once with a fallible constructor, keep proof-type fields private,
  and make downstream services and storage operations accept the proof instead of reconstructing
  or rechecking it. Use newtypes for scalar invariants, enums for mutually exclusive or correlated
  states, and capability wrappers for validated, resolved, authorized, or claimed state.
  Deserialization must pass through validation; never derive an unchecked constructor for a proof.
  Keep database constraints, transactions, and lease fencing for concurrent and cross-row
  invariants: types complement them and do not prove that mutable state is still current.

## Quality gates

Before handing off a change, run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo run --locked -- openapi --check
cargo deny check
cargo audit
```

CI additionally checks the committed OpenAPI contract, Rust 1.88 MSRV, Linux x86_64 and aarch64,
macOS aarch64, Windows x86_64, the production container context, and dependency/license policy.
When adding or removing a workspace crate, update the manifest-only `COPY` entries in `Dockerfile`
and run `python3 scripts/check-docker-manifests.py`. Changes to API schemas are incomplete until
`docs/openapi.json` is regenerated and clean.

Tests should exercise policy in neutral crates, adapter behavior at adapter boundaries, and HTTP
semantics through Actix's in-process test harness. Never log credentials, bootstrap secrets,
passwords, bearer tokens, backend paths, or recipe secret values.

The SQLx adapter uses only exactly pinned `sqlx-core` and `sqlx-sqlite` driver crates. Unused
MySQL/PostgreSQL drivers and their transitive dependencies must remain absent from the lockfile.
Both advisory tools must pass without exceptions. See [SECURITY.md](SECURITY.md).

## Persistence and execution policy

- SQLite is the v0.1 default. PostgreSQL is the first planned external relational adapter; MySQL
  may follow if justified. Never use SQLx `Any` or let driver/pool types escape an adapter.
- Every selectable relational adapter implements every operation trait aggregated by
  `stabbur_storage_core::Storage`, opts in explicitly, and passes the shared conformance suite.
  Do not add default no-ops or an unsupported-operation escape hatch to make an adapter compile.
- The server controls durable jobs, capability matching, leases, idempotency, verification, and
  publication policy. Workers initiate outbound connections and remain under local process
  supervision; the server must never SSH to or remotely shell into a worker.
- Treat AutoPkg as a macOS worker capability, not application composition or a special public API.
  Keep builder-neutral messages independent of AutoPkg report and cache layout.
