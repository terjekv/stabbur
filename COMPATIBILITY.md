# Compatibility

| Server | HTTP API  | SQLite migration set                     | Public client target                     |
| ------ | --------- | ---------------------------------------- | ---------------------------------------- |
| 0.0.1  | `/api/v1` | baseline + `0002_operator_workflows.sql` | `stabbur_client` 0.0.1                   |

The committed [OpenAPI document](docs/openapi.json) is the public contract. Internal
`/api/v1/internal/workers` lease primitives are versioned but deliberately excluded from the public
client contract. A worker binary is supported against the same server minor line; upgrade the
server before remote workers and rotate worker credentials when moving trust boundaries.

SQLite migrations are forward-only and embedded. The pre-release migration history was collapsed
into one complete baseline before v0.0.1. The operator migration upgrades that baseline in place,
preserving releases, workers and run evidence. Downgrading a database is unsupported; restore the
pre-upgrade coordinated backup instead.

Independent clients must record the exact server tag, normalized OpenAPI snapshot, container image
digest, and integration evidence in their own compatibility documentation.

Server [0.0.1](https://github.com/terjekv/stabbur/releases/tag/v0.0.1) is published for Linux amd64 at:

```text
ghcr.io/terjekv/stabbur-server@sha256:41aba0e2051de7d74df09a6e57487a4e1755706d587aca54603167953fd0748d
```

[Release evidence](docs/release-evidence.md) records the exact source, provenance, OpenAPI hash,
all four repository CI runs, client/CLI/browser image acceptance, and separate disposable macOS
installation and recovery acceptance. Later main-branch changes do not move the released tag or
replace its digest. A new publication requires a new package version.
