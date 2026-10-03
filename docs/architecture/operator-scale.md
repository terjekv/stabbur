# Operator experience and scale

Status: adopted October 2026. This is development scope, not a released-image compatibility claim.

## Product decisions

The library is the primary application inventory. Needs attention selects current failed checks,
missing compatible workers, and available candidate releases awaiting review. Exports own saved
application selections and reviewed publication snapshots. Activity retains execution evidence;
advanced build, recipe, worker, storage, and access administration remains available separately.

Build success, release approval, repository publication, and confirmed device installation are
separate facts. The console must not imply installation or fleet compliance from a download or a
successful build. Versions remain opaque: library sorting uses names or creation identities,
never inferred release ordering.

The new library API evaluates search and attention predicates before keyset pagination and returns
small aggregate summaries in one database statement. Cursors bind to search, view and ordering.
Pages are live observations: renaming an application while traversing name-sorted pages can move
it across the cursor. They are not immutable collection snapshots. Wildcards are literal; ASCII
case folding matches SQLite's portable built-in behavior. Non-ASCII text remains literal.

The console provides bookmarkable/shareable views, selectable summary columns, keyboard search,
and explicit selection across fetched pages and queries. An export's existing 100-application
limit remains enforced. Selection is transient and cleared on logout; it is never authorization.
The export picker searches bounded server pages and reads full metadata only for selected apps.
Publication history is also paginated.

Restoring a publication means copying its exact release selections and installation settings into
a new draft under the current definition revision. Keep the current destination and catalog.
Preview again and publish separately; current release eligibility and concurrent changes are
rechecked. This does not downgrade or uninstall already installed device software.

Queue observations group jobs by their complete capability requirements. Show missing compatible
workers separately from workers holding active leases. A lease count is not a configured worker
capacity. Existing scheduler coalescing and skipped missed intervals remain authoritative.

## Follow-on work and explicit adoption criteria

These decisions are accepted; implementation is conditional on evidence or a defined operator
policy. Do not introduce infrastructure or automatic publication solely to satisfy a feature list.

| Area                   | Decision and trigger                                                                                                                                                                                                                                                                                         |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Notifications          | Use the attention predicates as the basis for opt-in digests and transition notifications. Add a durable deduplicated outbox when a notification destination, recipient ownership, and acknowledgement policy are specified. Do not send one notification per retry.                                         |
| Source changes         | Keep upstream observations separate from trusted definitions. Extend the existing source-pin proposal and catalog-plan workflow with reviewed before/after trust evidence when source-update intake is added. Never accept parent trust automatically.                                                       |
| Testing and stable     | Preserve exact-release promotion and preview affected selections. Add structured test attestations or device telemetry through an explicit integration before claiming installation success.                                                                                                                 |
| Bulk operations        | Extend reviewed batch actions when operators identify repeated tasks beyond export selection. Freeze explicit identities and preconditions, bound batch size, show individual outcomes, and distinguish atomic publication from resumable reconciliation. Avoid implicit selection of unseen future matches. |
| Scheduling             | Preserve non-overlap for recurring target runs. Add explicit schedule spreading and worker concurrency controls when measured synchronized queue bursts warrant them; retain durable cursors and lease fencing.                                                                                              |
| Ownership              | Add ownership labels and scoped permissions with a concrete multiple-team authorization model. Presentation filters never establish access control.                                                                                                                                                          |
| Artifact delivery      | Add S3-compatible storage and separately scalable delivery when sustained download concurrency or bandwidth becomes limiting. Keep control, execution and delivery independently deployable. Cached delivery must retain withdrawal and authorization semantics.                                             |
| Relational persistence | Implement the complete PostgreSQL adapter when measured SQLite write contention or availability requirements justify it, with the shared conformance suite.                                                                                                                                                  |
| Console replicas       | Add a shared server-side session adapter when seamless replica failover is required. Current process-local sessions and restart invalidation remain explicit.                                                                                                                                                |
| Retention              | First implement a dry-run reference graph and reclaimed-byte report. Protect current channels, published exports, retained rollback snapshots and active work. Adopt a retention horizon explicitly before deleting bytes. Preserve append-only history and content identity.                                |

## Acceptance and measurement

Use a seeded catalog larger than one page. Verify exact search beyond the first page, duplicate-name
cursor traversal, literal wildcard handling, stale-query rejection, current attention predicates,
selection across pages and searches, and publication preconditions. Check mobile reflow and
keyboard access. Library loading must not fetch status once per row or download the entire catalog.

Record p50/p95 library and picker latency, requests per page, database transaction latency and busy
errors, queue age by capability, active watchers, artifact concurrency, transfer rates, and disk
usage. Increase the fixture toward deployment-specific catalog, history and download volumes
before claiming a supported capacity. The 240-application regression is functional evidence,
not a throughput benchmark or a production sizing claim.

## Design references

- [AutoPkgr](https://www.lindegroup.com/autopkgr/): application subscriptions and accessible setup.
- [MunkiAdmin](https://github.com/hjuutilainen/munkiadmin): coexistence with existing repository tools.
- [Cloudsmith search](https://docs.cloudsmith.com/artifact-management/search-filter-sort-packages): consistent UI/API/CLI filtering; Stabbur does not adopt semantic-version ordering.
- [Jamf App Installers](https://learn.jamf.com/r/en-US/jamf-pro-documentation-current/Distributing_Software_Titles_with_App_Installers): test and production separation.
- [Pulp retention](https://pulpproject.org/pulpcore/docs/user/guides/update-repo-retention/): protect distributed repository versions.
- [Pulp scaling](https://pulpproject.org/pulp-operator/docs/admin/guides/install/ha/): scale API, work, and binary delivery separately.
