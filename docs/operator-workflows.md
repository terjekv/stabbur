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
4. For a disabled manual target, choose **Review and build**, check its source pins and worker,
   then **Enable and start build**. Existing enabled targets use **Build now**. The console opens the run's progress,
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

The console now also offers **Munki delivery**. With its private delivery storage configured,
promote a release to testing, open its software page and continue to Munki delivery. Review the
installer format and installed-state detection, then publish. The console supplies a protected
repository URL, catalogs, manifests and verified installer bytes. Download a configuration profile
for a test Mac with Munki installed; the application-specific profile requests that application's
installation. Use Managed Software Center to install and check again. Confirm both checks before
publishing to stable. A local loopback installation is reachable only from that same Mac.

The console records reviewed publication snapshots separately from control-plane channels.
Console withdrawal also removes its served copies; external API/CLI withdrawal requires explicit
removal in Munki delivery. Already downloaded copies cannot be recalled. For repository deployment
and credential rotation, see the independent frontend README. The CLI workflow below remains
available for existing repositories.

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

Open **Add software from recipes**. Choose an inventory, or use **Use reviewed starter recipes** and scan
its displayed exact commit. Scan progress refreshes automatically. The picker groups recipes by
software and shows Recommended, Artifact recipes, Needs setup, and All discovered views. Purpose is
observed from known processors across the parent chain; it is neither a suffix guess nor an execution
safety guarantee. Install and publish workflows are excluded from guided artifact import.

Configure each selected installer separately. Confirm architecture from the artifact, not the worker,
and supply its version and installer output variables. The exact pinned FirefoxSignedPkg preset
suggests `version` and `pathname`; Thunderbird and VLC have additional reviewed presets at that
same source pin. Other recipes require reviewed mappings. Old snapshots without
purpose metadata remain readable but must be rescanned for guided import. Parent trust requirements,
missing dependencies and incomplete source closures prevent guided import in both UI and client.

**Review sources and import plan** shows software, immutable recipe revisions and disabled manual
targets. Inspect every source pin, selector and existing resource being updated before applying.
Review and enable a target, build once, inspect its artifact and verification results, then promote
or schedule. A failed trust check is shown above the run logs. Log streams are reconstructed separately
per attempt so partial stderr and stdout chunks cannot corrupt one another's messages.

A recipe with parents but no trust information first needs a reviewed AutoPkg override committed
and published at an exact revision. Import never accepts trust or queues a build automatically.

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

To discover recipes without a configured local AutoPkg inventory, expand **Scan a recipe repository**, enter an HTTPS URL and a full lowercase 40-character commit, and request a scan. An available
AutoPkg worker performs the scan. The console opens the completed snapshot automatically. Cross-repository
parents require a worker inventory containing those parent repositories, or a manually reviewed
catalog manifest. Older snapshots without import source closures must be refreshed by an updated worker.

Snapshots and history are immutable and publication is idempotent. Discovery is bounded to 60 seconds
per worker cycle, 8 MiB of local AutoPkg listing output and 1 MiB per published manifest. Discovery
failure does not prevent the worker from claiming builds; the worker retries on the next cycle.

Imported selectors use `/stabbur/outputs/<variable>`, the final value of each output variable across
one isolated run receipt. Ambiguous multiple receipts provide no normalized outputs and the build
fails validation instead of guessing an installer. Existing explicit receipt selectors remain supported.
