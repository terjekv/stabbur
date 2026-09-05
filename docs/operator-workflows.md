# Operator workflows

The control plane owns durable policy, the supported Rust client owns the public transport contract,
the CLI owns terminal operations, and `../stabbur-frontend` owns the independent management console.
The console logs in with the same Stabbur account as the CLI through its server-side session backend.

## Review and apply a catalog

Schema 2 adds build targets to managed software and immutable recipe revisions. Target references
resolve to the exact existing revision selected by a plan, or to the result of the planned append.
Reconciliation checks expected append sequences and resource revisions. It never orders opaque
software versions or automatically advances source pins.

```json
{
  "schema_version": 2,
  "software": [{"slug": "example", "name": "Example"}],
  "recipes": [{"name": "example-check", "revision": {
    "builder": "fake", "definition": {}, "required_capabilities": []
  }}],
  "targets": [{
    "name": "example-hourly", "software": "example", "recipe": "example-check",
    "parameters": {}, "schedule": {"kind": "interval", "every_seconds": 3600},
    "enabled": false
  }]
}
```

```sh
stabbur --json catalog plan --file catalog.json > reviewed-plan.json
stabbur --yes catalog sync --file catalog.json --plan-file reviewed-plan.json
```

Review execution policy before enabling a target. A saved plan is checked again before mutations;
resource preconditions still guard concurrent changes during application. Sync is additive and may
partially complete if a later mutation fails. Re-plan to resume. Schema 1 remains accepted without
targets. Resources omitted from a manifest remain unmanaged.

`stabbur catalog propose-source --file catalog.json --source-url https://example.org/recipes.git
--source-revision <full-lowercase-commit> --output proposed.json` creates a new validated manifest and
an exact before/after pin diff without network calls or server changes. Review the source commit and
AutoPkg parent trust changes separately, then plan/apply the proposed file. The tool never accepts
new parent trust automatically. See [AutoPkg parent trust guidance](https://github.com/autopkg/autopkg/wiki/AutoPkg-and-recipe-parent-trust-info).

## Observe and intervene

- `stabbur software status <slug>` shows publication and execution observations.
- `stabbur status` reports durable queue counts, oldest queued work, and draining workers.
- `stabbur worker drain <id> --revision <current>` pauses new claims while current work completes.
  `worker resume` re-enables claims. Disabling invalidates active attempts instead.
- `stabbur release withdraw <id> --revision <current> --reason 'Confirmed regression'` removes
  publication eligibility while retaining the stage reached and immutable historical evidence.
- List commands preserve `{items,next_cursor}` in JSON. `--all` follows cursors. Human output tells
  the operator when another page exists.
- Run waits/watches accept `--timeout-seconds`. Success exits 0, failed runs 2, cancelled runs 3,
  deadline expiry 124, and transport/protocol failures 1. Watch reconnects with the last delivered
  event sequence and suppresses replayed logs. EOF does not establish success.

## Inspect storage offline

Stop the server before local store maintenance; exclusive database access prevents racing live
uploads. These commands do not expose backend paths in their JSON reports.

```sh
stabbur-server admin store-inspect --data-dir /var/lib/stabbur \
  --digest <sha256> --maximum-bytes 1073741824
stabbur-server admin prune-uploads --data-dir /var/lib/stabbur \
  --older-than-seconds 86400 --limit 1000
```

Inspection reports free bytes and verified/missing/corrupt/budget-exceeded outcomes for up to 1000
selected objects. A non-verified result exits unsuccessfully. Cleanup examines a bounded number of
upload entries, ignores symlinks, and removes only old regular staging files. It never deletes CAS
objects or historical records. Repeat bounded passes when necessary.

## Export to Munki

`stabbur munki-export <software> --channel stable --platform macos --architecture arm64
--macos 15.0 --extension pkg --pkginfo-template reviewed-pkginfo.json --output export` resolves one
promoted installer, verifies a local SHA-256 download, and produces `pkgs/` and `pkgsinfo/` in a new
directory. The version remains opaque. Export refuses to replace an existing directory.

Provide a reviewed pkginfo JSON template (for example, converted from `makepkginfo` output), or store
Munki-compatible install and detection dictionaries in the software metadata. Detection must include
nonempty `installs` or `receipts`. The exporter owns name, exact version, SHA-256, rounded-up size in
KiB, relative installer location, channel catalog and platform constraints. It removes template URL
overrides. Installer behavior and detection metadata remain the packager's responsibility.
See [Munki's supported keys](https://github.com/munki/munki/wiki/Supported-Pkginfo-Keys).

Review the export, copy it into your existing protected Munki repository, run `makecatalogs`, add the
item to a test manifest, and exercise `managedsoftwareupdate --checkonly` followed by an explicitly
approved install on a disposable test Mac. Verify installed-state detection prevents repeat installs,
verify the downloaded installer hash, and record the source pin, run, release, channel revision and
client result. Stabbur credentials are never embedded in the export or shared with managed devices.

Exports are snapshots: subsequent withdrawal in Stabbur does not retract an already exported Munki
catalog. Remove or replace that item in the delivery repository and rebuild its catalogs as part of
incident response. No exporter can retroactively stop a device that already obtained installer bytes.

## Cross-repository checks

Run `python3 scripts/check-workspace-integration.py` with all four repositories as siblings. It builds
and checks a disposable local server, the independent client consumer, CLI pagination and catalog
schema 2, and console auth/origin/CSRF/roles/logout/revocation. The CI workflow checks these development
revisions together and records their exact commits. Repository publication and immutable server-image
acceptance remain separate release gates; local success is not a released compatibility claim.
