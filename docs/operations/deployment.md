# Deployment, backup, upgrade, and recovery

This guide covers the SQLite and private-filesystem-store deployment shipped in v0.1. The database,
artifact store, bootstrap state, and embedded-worker identity live under one private data directory.
For a complete first installation, follow the default
[Linux server and macOS AutoPkg worker](linux-server-macos-worker.md) runbook. The
[single-host macOS](single-host-macos.md) guide documents the compact variant.

## Files and permissions

Use a dedicated unprivileged account and an owner-only durable directory:

```bash
install -d -o stabbur -g stabbur -m 0700 /var/lib/stabbur
```

The default layout is:

```text
/var/lib/stabbur/
  stabbur.db
  stabbur.db-wal
  stabbur.db-shm
  stabbur.db.lock
  bootstrap.secret        # present only until first bootstrap
  embedded-worker/
  store/
    objects/
    uploads/
```

Object paths and database files are private implementation details. Do not serve `store/` through a
web server or edit database rows directly.

## Start the service

Small installation:

```bash
sudo -u stabbur stabbur-server all \
  --data-dir /var/lib/stabbur \
  --bind 127.0.0.1:8080
```

Distributed control plane with only remote workers:

```bash
sudo -u stabbur stabbur-server api \
  --data-dir /var/lib/stabbur \
  --bind 127.0.0.1:8080
```

Use systemd, launchd, a container runtime, or an orchestrator to supervise the process and restart
it on failure. Allow at least 15 seconds for graceful HTTP shutdown. The unauthenticated probes have
deliberately different meanings:

- `GET /healthz` is process liveness. It does not query dependencies and remains `200 OK` during a
  database or artifact-store outage. Use it only to decide whether the process needs restarting.
- `GET /readyz` is traffic readiness. It checks the database and primary artifact store without
  writing data, waits at most two seconds, and returns a generic `503 Service Unavailable` when
  either dependency is unavailable. Use it for deployment completion and traffic admission.

Neither probe depends on bootstrap completion, registered workers, or external package sources.
Both responses use `Cache-Control: no-store` and deliberately omit backend diagnostics. Use
`stabbur-server admin doctor` for a local operator-facing storage diagnostic.

The container runs as UID/GID 10001, defaults to `all`, listens on port 8080, and expects a durable
volume at `/var/lib/stabbur`.

## TLS reverse proxy

Bind the server to loopback or a private network and terminate TLS in a maintained reverse proxy.
The proxy must:

- require modern TLS and redirect plaintext HTTP;
- preserve streaming request and response bodies without whole-body buffering;
- permit long artifact uploads and SSE connections while bounding header and idle timeouts;
- rate-limit `/api/v1/auth/login` and `/api/v1/auth/bootstrap` per source and account;
- replace, rather than append, untrusted forwarding headers;
- add HSTS on the public HTTPS origin and `X-Content-Type-Options: nosniff`; and
- pass or create `x-request-id` without logging `Authorization`.

Do not configure a proxy-wide small response limit: artifact downloads and SSE are streaming. Keep
the server's 2 MiB JSON limit intact.

## First administrator

First start logs only the path to `bootstrap.secret`. The preferred healthy-service flow uses the
public CLI and requires no existing login or bearer token:

```bash
sudo -u stabbur stabbur --server http://127.0.0.1:8080 bootstrap \
  --username admin \
  --bootstrap-secret-file /var/lib/stabbur/bootstrap.secret \
  --password-file /run/secrets/stabbur-admin-password
```

This calls `POST /api/v1/auth/bootstrap`; use the public HTTPS origin when running anywhere except
the server host. For non-interactive initialization before first startup, or local recovery when
the pending secret file is unavailable, stop every API process and run:

```bash
sudo -u stabbur stabbur-server admin bootstrap \
  --data-dir /var/lib/stabbur \
  --username admin \
  --password-file /run/secrets/stabbur-admin-password
```

The password file must be regular and owner-only. If it is omitted, the command reads the password
interactively. The local command needs no HTTP bootstrap secret, runs embedded migrations against
a new database, and refuses to create an administrator if any principal already exists or
bootstrap was permanently disabled. Delete the password input after use. Local mutations acquire
an exclusive process lock and refuse to run while an API or another admin process has the database
open.

## Coordinated backup

The safest v0.1 backup is an offline copy of the complete data directory:

1. Stop API and embedded-worker processes and wait for them to exit.
2. Confirm `stabbur-server admin doctor --data-dir /var/lib/stabbur` succeeds.
3. Copy the entire directory, preserving modes, ownership, sparse files, and hard links.
4. Encrypt the backup and record the server version and SHA-256 of `stabbur.db`.
5. Restart the service and check `/readyz` plus one authenticated catalog read.

Do not copy only `stabbur.db` while WAL writers are active. Do not back up the database without the
artifact store: metadata and content form one recovery unit. An online snapshot is acceptable only
when the storage platform gives a crash-consistent atomic snapshot of the entire directory.

## Restore and disaster recovery

1. Stop the service and preserve the failed directory for investigation.
2. Restore the complete backup into a new owner-only directory on the same filesystem type.
3. Run `stabbur-server admin doctor --data-dir RESTORED_DIR`.
4. Start the same server version against the restored directory.
5. Verify authentication, audit reads, store health, a known artifact HEAD/range read, and worker
   registration before reopening traffic.

If artifact files were lost after the last backup, metadata may still name `present` locations.
Keep traffic closed until affected locations are reconciled; v0.1 does not provide replication or
automatic store repair.

## Upgrade and rollback

The unreleased development history was squashed into one `0001_initial.sql` baseline before the
first supported release. A SQLite database initialized by an earlier development checkout is not
upgradeable through that discarded history: export anything needed, stop the service, and recreate
the database and store test environment from scratch. Never apply that instruction to a released
deployment. After v0.1 is published, migration history is permanent and only additive forward
migrations are allowed.

Before an upgrade, read the changelog, take a coordinated backup, and record the current binary or
image digest. Stop all v0.1 API processes, run the new binary's migration command once, then start
the new API before remote workers:

```bash
stabbur-server admin migrate --data-dir /var/lib/stabbur
stabbur-server admin doctor --data-dir /var/lib/stabbur
```

Migrations are embedded, forward-only, and checksum-verified. After startup, check health,
authentication, OpenAPI availability, a catalog page, and an artifact range. Then restart remote
workers from the same supported minor line. Database downgrade is unsupported; rollback means
restoring the coordinated pre-upgrade backup and old binary.

## Break-glass recovery

Ordinary identity recovery belongs in REST. When REST authentication is unavailable, stop the API
and use only these local operations:

```bash
stabbur-server admin reset-password USER --data-dir /var/lib/stabbur
stabbur-server admin revoke-sessions USER --data-dir /var/lib/stabbur
stabbur-server admin doctor --data-dir /var/lib/stabbur
```

Mutations are audited as `local-break-glass`, revoke existing credentials where applicable, and
require exclusive SQLite access. These commands cannot create software, run recipes, promote a
release, or perform normal administration.
