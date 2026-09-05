# Catalog discovery and build scheduling

Stabbur has an explicit catalog and server-owned execution policy. An operator creates software,
recipe metadata, and an immutable recipe revision, then persists a build target binding one
software identity to that exact revision. A target may be manually triggered or carry a fixed
interval and durable next-run cursor. The server can request bounded observations of exact pinned
repositories, but does not turn observations into desired recipes or execute everything visible
to AutoPkg.

This keeps unattended operation rooted in reviewed desired state plus server-owned scheduling,
not implicit execution of third-party repository contents.

## Source of truth

A version-controlled Stabbur catalog manifest should declare:

- software slug, display name, and installation metadata;
- recipe name and immutable builder definition;
- pinned source URL and commit for AutoPkg definitions;
- an allowlisted recipe entrypoint and non-secret parameters; and
- enabled build targets and their polling policy.

Schema version 1 is implemented by the supported Rust client and `stabbur catalog plan/sync`. It
reconciles software and recipe metadata through the public API, appends a new immutable revision
only when the canonical builder definition or capabilities change, and leaves unlisted resources
untouched. Build targets are currently managed through their public CRUD API. A later manifest
schema can reconcile them declaratively, disabling a removed target without deleting catalog or
execution history.

An upstream AutoPkg repository is input to a reviewed definition, not Stabbur's source of truth.
Stabbur must never build every recipe found in a repository implicitly. AutoPkg recipes execute
third-party processors and therefore require an explicit allowlist and review boundary.

## Knowing whether a recipe exists

A pinned source commit and entrypoint are authoritative execution inputs. The AutoPkg worker
materializes that exact commit and fails safely if the entrypoint cannot be resolved. This is the
minimum existence check and remains necessary even if earlier discovery succeeded.

An authenticated worker may publish a bounded, canonical, builder-neutral catalog manifest from a
pinned source observation. The snapshot contains normalized recipe identifiers, parent
relationships, execution capabilities, and safe validation diagnostics. It is content-addressed,
append-only, and idempotent per worker and digest. Exact lookup considers only the newest snapshot
for each producer/source pair, so an entry removed by a later pinned revision is no longer reported
as present. Complete older snapshots remain available as observation history.

The worker accepts this document through `--catalog-manifest FILE`. AutoPkg, AutoCfg, or another
adapter may generate that file, but the server imports none of those implementation types and does
not receive cache paths. `stabbur-server catalog generate-autopkg` supplies a bounded reference
generator that materializes an exact credential-free HTTPS commit and emits the same neutral JSON.
AutoCfg can invoke it or consume its output without becoming an application dependency.

The public scan API automates the same boundary: a caller submits an exact source and stable
producer with an idempotency key; the server creates an append-only scan and a durable
capability-matched job; an outbound worker claims it, generates a bounded manifest, and atomically
publishes the snapshot or a typed safe failure. Scan status, job attempts, snapshot identity, and
audit history remain queryable. Publishing a snapshot cannot create recipe metadata or revisions,
mutate targets, or enqueue builds. A reviewed desired-state manifest can still name known
entrypoints directly without discovery, and the server never inspects a worker over SSH.

## Picking what to build

The server persists each build target with:

- stable target identity and operator-facing name;
- software identity;
- exact immutable recipe revision identity;
- non-secret builder parameters;
- enabled state;
- manual or fixed-interval trigger policy;
- next eligible run time; and
- an optimistic-concurrency revision.

The scheduler selects only enabled, due targets and creates a normal durable run/job through the
same capability matching used by manual execution. One storage transaction verifies the exact
target revision and due cursor, advances the cursor, appends the target-to-run record and target
snapshot, and creates the run/job. A key scoped to target and due time makes retries idempotent;
the cursor precondition prevents duplicate work across concurrent server instances. Missed
intervals are skipped without schedule drift instead of creating an unbounded catch-up queue.

Updating a recipe never changes an existing target silently. An operator must explicitly update
the target to a new immutable revision under its ETag. Scheduler cursor changes also advance that
ETag, preventing a concurrent configuration update from restoring a consumed cursor.

Do not use discovered version ordering to decide whether a target is due. Versions are opaque.
Polling policy determines when to run; exact returned version equality is evaluated only after a
validated build result arrives.

## Deciding what to store

Workers may discover an upstream version only by executing the reviewed recipe. After execution,
the server still owns acceptance:

1. validate the run, attempt lease, builder result, source/tool provenance, and recipe trust;
2. independently verify uploaded bytes, digests, roles, and primary-store placement;
3. look up the exact `(software, version)` identity without inferring version ordering;
4. create a new immutable release when the version is absent;
5. treat an existing version with the exact same immutable artifact graph, reproducibility
   provenance, and verification evidence as a successful `no_change` run linked to that release;
   and
6. fail an existing version with different bytes, variant metadata, provenance, or verification
   evidence as `version_content_conflict`, preserving both run records for investigation.

This policy is implemented atomically. Every build run records its release link and publication
disposition. Per-run provenance capture times remain recorded but do not participate in evidence
equality; observation time cannot make otherwise identical output into different content. A
no-change rebuild does not append lifecycle or promotion history or advance a channel. A conflict
terminalizes the attempt, job, and run as failed without mutating the existing release or channels.

## Delivery order

1. **Delivered:** use the builder-neutral revision contract for deterministic fake and AutoPkg
   definitions.
2. **Delivered:** add schema-versioned manifest canonicalization and `catalog plan`/`catalog sync`
   to the public client and CLI.
3. **Delivered:** add exact no-change versus same-version content-conflict finalization semantics.
4. **Delivered:** add append-only build-target history, CRUD and trigger APIs, shared storage
   conformance coverage, and the server scheduler.
5. **Delivered:** add immutable worker-published catalog snapshots and exact latest-source lookup.
6. **Delivered:** add durable server-requested catalog-scan jobs that reuse the neutral manifest
   contract and outbound worker protocol.

This order makes catalog setup automatable before introducing background execution, and keeps
every scheduled build on the same durable worker protocol already exercised by manual runs.
