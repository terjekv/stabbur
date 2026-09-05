# Compatibility

| Server | HTTP API  | SQLite migration set                     | Public client target                   |
| ------ | --------- | ---------------------------------------- | -------------------------------------- |
| 0.1.x  | `/api/v1` | baseline + `0002_operator_workflows.sql` | `stabbur_client` 0.1 release candidate |

The committed [OpenAPI document](docs/openapi.json) is the public contract. Internal
`/api/v1/internal/workers` lease primitives are versioned but deliberately excluded from the public
client contract. A worker binary is supported against the same server minor line; upgrade the
server before remote workers and rotate worker credentials when moving trust boundaries.

SQLite migrations are forward-only and embedded. The pre-release migration history was collapsed
into one complete baseline before v0.1. The operator migration upgrades that baseline in place,
preserving releases, workers and run evidence. Downgrading a database is unsupported; restore the
pre-upgrade coordinated backup instead.

Independent clients must record the exact server tag, normalized OpenAPI snapshot, container image
digest, and integration evidence in their own compatibility documentation.
