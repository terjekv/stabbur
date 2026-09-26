# Operator workflows

The control plane owns durable policy, the supported Rust client owns the public transport contract,
the CLI owns terminal operations, and `../stabbur-frontend` owns the independent management console.
The console logs in with the same Stabbur account as the CLI through its server-side session backend.

## From installation to first delivery

1. Choose the [local macOS test installer](operations/macos-test-setup.md) or the
   [Linux server and macOS worker deployment](operations/linux-server-macos-worker.md). Complete
   first-administrator bootstrap, then sign into the CLI and console with that account.
2. Check `stabbur status` and `stabbur worker list`. In the console, inspect Workers. Confirm that
   the intended worker is enabled, recently observed, and advertises the capabilities required
   by the exact recipe revision. A successful login does not establish worker readiness.
3. Add software and a reviewed recipe revision, or review and apply a catalog plan. Open the
   software page and choose **Add build target**. Select the software, recipe and exact revision.
   Start with a manual schedule. New console targets default to disabled.
4. Review and enable the target, then select **Build now**. The console opens the run's progress,
   replayed logs and verification result. The CLI equivalent is
   `stabbur target trigger NAME --idempotency-key UNIQUE_KEY --watch`.
5. Follow **Review resulting release**. Check the exact version, verification evidence and
   platform variants, then promote to testing. The promotion preview shows the current and
   proposed channel selections. Validate installation and detection on a test device before
   promoting to stable.
6. Resolve or export the stable installer using the reviewed delivery workflow below. Browser
   publication alone does not install software on a device or update an existing Munki export.

Resource pages have shareable fragment URLs. Refresh and Back retain the selected resource.
Search and status filters apply to the items loaded in the current view; **Load more** extends
that set. Build targets show their own run history; global runs show software names and full IDs.
The console follows durable logs in bounded pages and can pause or resume updates. CLI watches
remain the preferred option for unattended operation and exact exit-status handling.

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
  Failed jobs and expired attempts are historical totals, not counts of unresolved incidents.
- `stabbur worker drain <id> --revision <current>` pauses new claims while current work completes.
  `worker resume` re-enables claims. Disabling invalidates active attempts instead.
- `stabbur release withdraw <id> --revision <current> --reason 'Confirmed regression'` removes
  publication eligibility while retaining the stage reached and immutable historical evidence.
- List commands preserve `{items,next_cursor}` in JSON. `--all` follows cursors. Human output tells
  the operator when another page exists.
- Interactive CLI promotion reads the channel's revision when it is omitted. Automation retains
  revision 0 as the default for channel creation; use an explicit `--revision` or opt into
  `--current-revision`. Both still submit a concurrency precondition and reject stale state.
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
With the dependencies in `scripts/requirements-e2e.txt` and Chromium installed, add `--browser`
to exercise form errors, edit/refresh, navigation, catalog review, named target creation, paginated
logs, publication, stale promotion, session expiry, literal text rendering and mobile reflow. The
publication fixture uses synthetic bytes through the leased worker protocol; actual AutoPkg builds
and installation remain covered by the separate macOS acceptance workflow.

## Discover and import AutoPkg recipes

Open **Recipes → Import recipes** (also available from Workers). Choose a worker inventory,
filter identifiers, select recipes, and review their parent relationships and exact Git sources.
Supply software names, artifact architecture, minimum macOS when known, and the output variables.
The default `pathname` selects a downloaded installer; use `pkg_path` for a generated package.
The version variable defaults to `version`. Confirm these against the recipe before building.

**Review import plan** shows the proposed software, immutable recipe revisions and targets.
Review the source pins, selectors and any existing resources the plan would update before applying.
Every imported target starts disabled with a manual schedule. Parent trust is never accepted by
importing, and no build is queued. Review the recipe's verification policy before enabling a target.
A recipe with parents but no trust information should first get a committed, reviewed AutoPkg override.

Enable local discovery on the worker account that owns the AutoPkg profile:

```sh
stabbur-server worker --server-url https://stabbur.example.net \
  --token-file /path/to/worker-credential.json --data-dir /path/to/private-worker-state \
  --discover-autopkg
```

Optionally add `--autopkg-prefs /path/to/preferences.plist`. The equivalent environment variables
are `STABBUR_DISCOVER_AUTOPKG=true` and `STABBUR_AUTOPKG_PREFS`. Discovery runs after registration
and every five minutes while idle, using `autopkg list-recipes --plist --show-all`. Builds remain
isolated from that account's ambient preferences. A worker busy building publishes its next
observation after finishing the build. Refresh discovery in the console to see published snapshots.

Discovery only reads recipes and Git metadata. It does not run recipes, update repositories,
accept trust, or upload local paths or Input values. Committed overrides keep their original
contents in pinned Git sources, along with their parent repository pins. Recipes must be tracked
and unchanged in clean repositories with credential-free HTTPS origins. Publish those exact
commits so other workers can fetch them. Local-only, dirty, or untracked overrides remain visible
with an import blocker. Duplicate identifiers, missing parents, cycles, conflicting source pins,
and external processor dependencies also require attention. External processors currently need a
manually reviewed catalog manifest with their complete source dependencies.

To discover recipes without a configured local AutoPkg inventory, expand **Import from a repository
URL**, enter an HTTPS URL and a full lowercase 40-character commit, and request a scan. An available
AutoPkg worker performs the scan. Use **Check scan** to open the completed snapshot. Cross-repository
parents require a worker inventory containing those parent repositories, or a manually reviewed
catalog manifest. Older snapshots without import source closures must be refreshed by an updated worker.

Snapshots and history are immutable and publication is idempotent. Discovery is bounded to 60 seconds
per worker cycle, 8 MiB of local AutoPkg listing output and 1 MiB per published manifest. Discovery
failure does not prevent the worker from claiming builds; the worker retries on the next cycle.

Imported selectors use `/stabbur/outputs/<variable>`, the final value of each output variable across
one isolated run receipt. Ambiguous multiple receipts provide no normalized outputs and the build
fails validation instead of guessing an installer. Existing explicit receipt selectors remain supported.
