# Post-v0.0.1 priorities

The four release priorities are complete: AutoPkg inventory/import and operator workflows,
coordinated CI/browser coverage, disposable macOS installation/recovery acceptance and immutable
server publication, and public client/CLI releases against that same image. See
[release evidence](release-evidence.md) and the [completed checklist](v0.0.1-release-checklist.md).

## Next operating milestone

Exercise the released server image and CLI on a separate server and supervised macOS worker.
Verify outbound enrollment, HTTPS through the production reverse proxy, capability matching,
worker restart and credential rotation, artifact delivery and coordinated backup/restore across
that network boundary. The existing single-host acceptance remains the reproducible regression
baseline. Collect operator feedback on discovery/import, build policy, promotion and withdrawal
before expanding the public contract.

## Later implementation

- PostgreSQL is the first planned external relational adapter. It must explicitly implement the
  complete storage contract and pass the shared conformance suite without driver types escaping
  the adapter. MySQL remains optional if justified.
- S3-compatible and read-only HTTP stores, multiple locations and replication, manual uploads,
  directory migration, OIDC, encrypted secret providers and richer native recipes remain deferred.
- The console stays in its independent repository. Munki export and disposable-device installation
  are implemented and accepted; broader deployment rollout remains operational work.
- Native listener TLS/HTTP/2 remains deferred while the selected Actix dependency line cannot use
  the patched HTTP/2 stack. Production deployments terminate TLS at a maintained reverse proxy.
