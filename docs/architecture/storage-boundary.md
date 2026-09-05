# Relational storage boundary

Stabbur treats the relational database as an internal adapter, not as part of the domain or HTTP
application model. SQLite, PostgreSQL, and a possible MySQL implementation must preserve the same
domain identities, transactions, authorization inputs, audit behavior, job leases, and
idempotency semantics.

The boundary is complete and backend-neutral: missing mandatory behavior is a compile-time error,
native database mechanics stay inside the adapter, and shared semantic tests decide whether a
backend is selectable.

## Ownership

```text
HTTP handlers / workers / admin commands
                  |
                  v
       operation-shaped storage traits
                  |
                  v
       complete `Storage` aggregate
                  |
                  v
       opaque runtime composition
                  |
        +---------+----------+
        |                    |
      SQLite          PostgreSQL / MySQL
   rows, SQLx, WAL       native driver, SQL,
   and migrations       pools and migrations
```

- `stabbur-domain`, `stabbur-auth-core`, and `stabbur-jobs-core` own validated portable values and
  policy. They do not depend on SQLx or a storage adapter.
- `stabbur-storage-core` owns backend-neutral request/result values, bounded errors, explicit
  transactions, and the complete persistence contract.
- `stabbur-storage-sqlite` owns SQLite rows, queries, migrations, pragmas, pooling, locking, and
  native tests.
- `stabbur-storage-runtime` owns static adapter registration and returns only an opaque
  `StorageHandle` containing `Arc<dyn Storage>`.
- `stabbur-storage-conformance` owns reusable semantic acceptance checks shared by every adapter.
- The root server owns application configuration and composition but imports no concrete database
  adapter, SQL library, row, pool, migration, or query type.

Artifact byte storage is a separate boundary in `stabbur-store-core`; changing a relational
adapter never changes content-addressed artifact identity or placement semantics.

## Complete contract

`Storage` is a method-free aggregate over these required operation traits:

- `TransactionalStorage`;
- `BootstrapStorage`;
- `CredentialStorage`;
- `IdentityAdminStorage`;
- `SoftwareStorage`;
- `ArtifactStorage`;
- `WorkerStorage`;
- `RecipeStorage`;
- `RunStorage`;
- `BuildStorage`;
- `CatalogStorage`;
- `RunLogStorage`;
- `JobStorage`;
- `AuditStorage`; and
- `OperationalStorage`.

There is deliberately no blanket `Storage` implementation. A production adapter implements every
operation trait and explicitly writes `impl Storage for Adapter {}`. There are no default no-ops
and no generic `UnsupportedOperation` error. A focused test double may implement only the trait a
service consumes, but it cannot be registered as a complete backend.

Portable transactions expose only `StorageTransaction`. Connections, SQL query builders, and
driver transactions never cross the adapter boundary. Aggregate operations that require richer
atomic behavior remain backend-neutral methods on the corresponding storage capability. Mutations
that require durable audit evidence write the resource and audit event in one transaction.

## Adding an adapter

1. Create an adapter crate such as `stabbur-storage-postgres` or `stabbur-storage-mysql`.
2. Keep its driver features, migrations, rows, native errors, pools, TLS, locking, and diagnostic
   providers inside that crate.
3. Implement every operation trait and explicitly implement the complete `Storage` aggregate.
4. Run `stabbur-storage-conformance` against a fresh migrated database.
5. Add adapter-native migration, concurrency, lease-race, restart, recovery, and query-plan tests.
6. Register the adapter exhaustively in `stabbur-storage-runtime` and add its CI service fixture.

Steps 1-5 do not change the server or storage-core crates. Registration changes only the internal
composition crate. The core contract changes only when Stabbur needs genuinely new portable
behavior that every selectable backend must implement.

Backend registration is statically linked Rust composition, not a dynamic plugin ABI. This keeps
startup failures deterministic and makes omitted operations compile-time failures. Dynamic
plugins, runtime capability negotiation, and independently versioned adapter SDKs are separate
future decisions.

## Configuration

SQLite remains the default:

```text
stabbur-server all --data-dir /var/lib/stabbur
```

An explicit SQLite URL is supported without exposing it in debug output:

```text
STABBUR_STORAGE_BACKEND=sqlite
STABBUR_DATABASE_URL=sqlite:///var/lib/stabbur/stabbur.db
stabbur-server all --data-dir /var/lib/stabbur
```

PostgreSQL and MySQL names fail closed until their adapters are linked and registered. Database
URLs are treated as secrets because they may contain credentials; they must never appear in logs,
errors, diagnostics, or command output.
