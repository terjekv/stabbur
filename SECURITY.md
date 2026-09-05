# Security policy

Report vulnerabilities privately to the repository maintainers. Do not open a public issue with
credentials, backend paths, bootstrap material, personal data, or a working exploit. Include the
affected version, impact, reproducible conditions, and any suggested mitigation. Maintainers will
acknowledge the report, coordinate a fix and advisory, and credit the reporter when requested.

## Supported versions

Until the first stable release, only the latest v0.1 release candidate is supported. After v0.1,
the latest patch release in the current minor line receives security fixes.

## Deployment requirements

- Expose the API only through a maintained TLS reverse proxy. The native listener is HTTP/1.1.
- Apply login and bootstrap rate limits at the proxy, bound connection/header timeouts, and set
  standard transport and content-type hardening headers.
- Keep the data directory, bootstrap secret, worker credential, password input, and token/profile
  files owner-only. Never pass a password or token as a command argument.
- Restrict worker egress to the control plane, reviewed Git source hosts, and required upstream
  package hosts. Treat AutoPkg processors and their fetched content as untrusted build input.
- Never grant the long-running worker installation privileges. Stage AutoPkg packages through a
  trusted provisioning system and run `worker prepare` locally with an exact digest and explicit
  signature policy before starting or restarting the unprivileged worker. Prefer a pinned leaf
  Developer ID team; unsigned packages provide no publisher identity and require independently
  reviewed bytes.
- Authorize any different AutoPkg installation with the exact installed-manifest digest from the
  owner-only receipt. Do not infer upgrade or rollback safety from the opaque version string.
- Back up the database and artifact store together and encrypt backups at rest.

## Dependency policy

`cargo audit` and `cargo deny check` must both pass without advisory exceptions. The server links
only the SQLx SQLite driver; unused MySQL and PostgreSQL drivers are excluded from the lockfile.
Server-side HTTP/2 is disabled while the selected Actix dependency line cannot consume the patched
HTTP/2 stack. Reconsider that boundary on every dependency update.

The complete threat review, residual risks, and verification commands are recorded in
[docs/security-review.md](docs/security-review.md).
