# Clients, packagers, workers, and AutoPkg control

The API is the control plane and workers are the execution plane. A packager is a worker with a
builder capability; it is not a remotely controlled shell or a separate server role.
The default host-by-host installation is documented in
[Linux server and macOS AutoPkg worker](linux-server-macos-worker.md); this guide defines the
underlying worker and builder contracts.

```text
CLI or public API client
       |
       | create immutable recipe/target, trigger, cancel, promote, inspect
       v
stabbur-server api ---> durable jobs, leases, logs, policy, audit, artifacts
       ^
       | outbound register, claim, heartbeat, upload, complete/fail
       |
stabbur-server worker on supervised Linux/macOS/Windows execution slots
```

The server controls *what* runs by creating a builder-neutral job, matching required capabilities,
granting an expiring lease, validating every result, and accepting publication only when policy
passes. systemd, launchd, a container runtime, or an orchestrator controls *whether the worker
process is alive*. The server never uses SSH, remote shell, or inbound worker connections.

Several workers may advertise overlapping capabilities. Any compatible worker can atomically
claim the next job, so adding independently supervised identities fans work out without assigning a
hostname in the recipe. At-least-once delivery is safe because every try has an attempt identity,
lease, maximum-attempt count, and idempotent transactional terminal result.

## Capability model

The runtime detects and advertises:

- `runtime.portable`, `builder.fake`, and one `os.*` capability on every supported platform;
- `builder.autopkg` on macOS only when `autopkg version` succeeds; and
- `tool.apple-xcode` when `xcodebuild -version` succeeds.

Worker provisioning sets a server-side ceiling. Registration fails if detection advertises a
capability outside that ceiling. An AutoPkg revision always adds `os.macos` and `builder.autopkg`
to its job requirements; `required_capabilities` may add stricter constraints such as Xcode.

Inspect the execution host before provisioning it:

```bash
stabbur-server worker --print-capabilities
```

For a reviewed nonstandard installation, add `--autopkg-program /absolute/path/to/autopkg`. The
path is a worker-host setting, never server-supplied; it must resolve to a regular executable that
is not group- or world-writable. Use the same option when starting the worker.

The JSON output contains the exact capability list the worker will advertise and detected tool
versions. Review it on the final worker account after installing or removing tools. Provision the
server-side ceiling with that complete list (or a deliberate superset); a partial list is rejected
as `worker_capability_escalation`. Re-run the inspection after tool upgrades and change the ceiling
through the administrative API before restarting a worker whose detected list grew.

Use one provisioned identity per independently supervised execution slot. v0.0.1 executes one job at
a time per worker process. Increase parallelism by provisioning more identities and processes,
preferably with separate private data directories.

## Prepare AutoPkg on macOS

`worker prepare` is a one-shot local provisioning command for unattended macOS image building or
MDM execution. It is separate from the unprivileged outbound worker loop: the control plane cannot
invoke it, and a claimed job cannot install or replace worker tools. The command never downloads a
package, enrolls a worker, or starts its supervisor.

### Default pinned fixture

The repository includes
[autopkg-2.9.0.json](../../tests/fixtures/autopkg-prepare/autopkg-2.9.0.json) as the default reviewed
fixture. It pins the official AutoPkg 2.9.0 release URL, 52,253,994-byte size, lowercase SHA-256,
package receipt, installed program, exact health output, and Stabbur installed-manifest digest. It
is a fixture descriptor; render its `manifest` object with the actual local package path before
calling the CLI:

```bash
fixture=tests/fixtures/autopkg-prepare/autopkg-2.9.0.json
package=/private/var/tmp/autopkg-2.9.0.pkg
manifest=/private/var/tmp/autopkg-2.9.0.stabbur.json

curl --fail --location --proto '=https' --proto-redir '=https' \
  --output "$package" \
  "$(jq -r '.release.url' "$fixture")"
chmod 0600 "$package"

test "$(stat -f '%z' "$package")" = "$(jq -r '.release.size' "$fixture")"
test "$(shasum -a 256 "$package" | awk '{print $1}')" = \
  "$(jq -r '.release.sha256' "$fixture")"

jq --arg package "$package" '.manifest | .package.path = $package' \
  "$fixture" >"$manifest"
chmod 0600 "$manifest"
```

The official 2.9.0 package is not Apple-signed: `pkgutil --check-signature` reports exactly
`Status: no signature`. The fixture therefore makes the narrow policy explicit:

```json
"signature": {
  "policy": "unsigned"
}
```

This policy provides no publisher identity. The committed, reviewed SHA-256 is the package
identity and must be verified before privilege is granted. An unsigned policy succeeds only when
`pkgutil` reports the package as unsigned; it does not accept an arbitrary signature-check error.
For a signed installer, require its ten-character leaf Developer ID Installer team instead:

```json
"signature": {
  "policy": "developer_id",
  "team_id": "ABC123DE45"
}
```

The signature policy is mandatory and part of the canonical installed identity. Changing from
unsigned to signed bytes, changing the team, or changing any package identity requires an explicit
replacement even if the human-readable version stays the same.

### Verify and install

Validate the strict manifest, package SHA-256, and declared signature policy without privilege or
mutation:

```bash
stabbur-server worker prepare \
  --manifest /private/var/tmp/autopkg-2.9.0.stabbur.json \
  --check
```

The pinned default fixture returns this identity:

```json
{
  "status": "verified",
  "builder": "autopkg",
  "version": "2.9.0",
  "manifest_sha256": "7d829c7c6a60c5f39feff17dced34f8ed3de472e9a61e6ee0d38c076a8e7a847",
  "package_sha256": "b858161c4fe20429127a0429cdf1e6e1e2cca66b1b5ec2f81a2b98933b0a66f2"
}
```

Install through the native macOS package installer and run the exact `autopkg version` health
check:

```bash
sudo stabbur-server worker prepare \
  --manifest /private/var/tmp/autopkg-2.9.0.stabbur.json
```

The default receipt is `/var/db/stabbur/worker/autopkg-prepared.json`. After the fixture succeeds,
it has this non-secret but owner-only shape:

```json
{
  "schema_version": 1,
  "builder": "autopkg",
  "version": "2.9.0",
  "manifest_sha256": "7d829c7c6a60c5f39feff17dced34f8ed3de472e9a61e6ee0d38c076a8e7a847",
  "package_sha256": "b858161c4fe20429127a0429cdf1e6e1e2cca66b1b5ec2f81a2b98933b0a66f2",
  "package_identifier": "com.github.autopkg.autopkg",
  "signature": {
    "policy": "unsigned"
  },
  "expected_health_output": "2.9.0",
  "prepared_at": "2026-08-26T19:00:00Z"
}
```

Both the manifest and package must be absolute, regular non-symlink files that are not writable by
group or world. Installation uses a root-owned private staging copy, clears the subprocess
environment, verifies the digest and signature policy again after copying, invokes
`/usr/sbin/installer` without a shell, checks the expected native package receipt, and writes an
owner-only atomic receipt. An identical rerun with a healthy installed program returns
`already_prepared` without reinstalling. Changing the package staging path, fixture wrapper, or
JSON formatting does not change the installed-manifest identity.

### Repair and replace AutoPkg

The command distinguishes four mutating outcomes without inferring version order:

- `installed`: no Stabbur preparation receipt existed and the manifest did not request replacement;
- `already_prepared`: the exact installed identity, native package receipt, and health output agree;
- `repaired`: the exact installed identity was recorded, but its package receipt or health check
  failed, so the same pinned package was reinstalled; and
- `replaced`: a different installed identity existed and its exact digest authorized the change.

The JSON report always includes `version`, `package_sha256`, and `manifest_sha256`; repair and
replacement reports also include `previous_version`. Preserve `manifest_sha256`: it is the
optimistic precondition for the next replacement. The digest covers the builder, opaque version,
package digest, package identifier, signature policy, and health-check contract. It excludes JSON
formatting, the local staging path, and the replacement precondition itself.

To upgrade, downgrade, or otherwise replace AutoPkg, review another complete pinned fixture and add
the current receipt's digest to its rendered manifest:

```json
"replaces_manifest_sha256": "7d829c7c6a60c5f39feff17dced34f8ed3de472e9a61e6ee0d38c076a8e7a847"
```

If no receipt exists, a manifest containing `replaces_manifest_sha256` fails. If a different
receipt exists, an absent or mismatched precondition fails and reports the digest the operator must
review. An exact healthy rerun remains idempotent, and an exact unhealthy rerun may repair itself
without a replacement precondition.

Stabbur calls the operation `replaced`, not `upgraded`, because versions are opaque. A newer
release, an intentional rollback, or different bytes under the same version all use the same
exact-digest authorization. The new receipt is published only after the native installer, package
receipt, and exact health output succeed. A failed installer does not replace Stabbur's prior
receipt, although macOS package scripts are not transactionally rolled back by Stabbur.

An MDM or image builder should stage the reviewed package and rendered manifest, run `--check`, run
the privileged preparation, install the worker credential and launchd definition, and then start
the unprivileged worker. Preparation does not itself advertise a capability. The worker advertises
`builder.autopkg` only when its local AutoPkg version probe succeeds.

### Disposable macOS acceptance

The macOS ARM64 CI leg runs `scripts/live-worker-prepare-autopkg.sh` with the default fixture. The
script downloads and independently checks the pinned size and SHA-256, exercises `--check`, installs
through `worker prepare`, verifies the owner-only receipt, repeats the command to prove idempotency,
and verifies worker capability discovery. It refuses to run unless
`STABBUR_DISPOSABLE_MACOS=1` is explicit because the official installer changes `/Library`,
`/usr/local`, package receipts, and launchd state. Do not set that guard on a persistent workstation.

## Provision a remote worker

The caller needs `worker:manage`. Keep its bearer token in a mode-0600 curl config rather than a
command argument. For the default macOS host layout, stage the native worker first with
`scripts/install-worker-macos.sh` as shown in the
[default deployment runbook](linux-server-macos-worker.md#install-the-worker-host-on-macos).

```text
header = "Authorization: Bearer ADMIN_TOKEN"
```

Provision a macOS AutoPkg slot. The response already has the exact credential-file shape expected
by the worker and the raw token is returned only once:

```bash
umask 077
curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --json '{"name":"mac-builder-01","allowed_capabilities":["runtime.portable","builder.fake","os.macos","builder.autopkg","tool.apple-xcode"]}' \
  https://stabbur.example.net/api/v1/workers \
  > /secure/mac-builder-01.credential.json
chmod 0600 /secure/mac-builder-01.credential.json
```

If the response is lost, do not repeat provision blindly: list workers, identify the created
record, and rotate its token. Deliver the response through the protected host channel, then install
it owner-only with `install-worker-macos.sh --credential-file PATH`. A different existing
credential requires explicit `--replace-credential`.

## Run and supervise the worker

```bash
stabbur-server worker \
  --server-url https://stabbur.example.net \
  --token-file /usr/local/var/lib/stabbur-worker/credential.json \
  --data-dir /usr/local/var/lib/stabbur-worker \
  --autopkg-program /Library/AutoPkg/autopkg \
  --catalog-manifest /usr/local/var/lib/stabbur-worker/catalog.json
```

Remote URLs must be HTTPS and cannot contain credentials, query, or fragment. Plain HTTP is
accepted only for loopback testing. The credential and data directory must remain private and
stable across restarts. The worker has no database URL, opens no listener, and needs no control
plane filesystem access.

At startup, the process detects tools, registers capabilities, and polls for compatible work. A
claim has a 60-second lease renewed every 20 seconds. Heartbeats continue through execution, log
flush, output selection, hashing, and upload. The job envelope sets an execution deadline, six
hours by default and validated within 60 seconds to 24 hours. Lease loss aborts acceptance and lets
the server retry; a local supervisor restarts a crashed process.

On macOS, install AutoPkg, Git, and any required Apple command-line tools before starting the
worker. Give the worker a disposable account without login keychains, control-plane credentials,
or unrelated host secrets. Restrict outbound network access to the API, reviewed Git hosts, and
required package sources.

## Publish observed recipes

`--catalog-manifest` is optional. It accepts at most 1 MiB of canonical builder-neutral JSON and
publishes it after authenticated capability registration. The producer must match an advertised
`builder.*` capability. Repeated startup with identical content replays the original snapshot
without appending duplicate audit history.

AutoPkg, AutoCfg, or a repository-review pipeline may generate this file. That adapter owns source
inspection; the server receives no worker cache paths or third-party types. Entries, parents, and
diagnostics must be sorted and unique. For example:

```json
{
  "schema_version": 1,
  "producer": "autopkg",
  "source": {
    "locator": "https://github.com/autopkg/recipes.git",
    "revision": "6c092b47e9c6324aa48758832b2597a0f3ff932e"
  },
  "recipes": [
    {
      "identifier": "Firefox.pkg.recipe",
      "builder": "autopkg",
      "parents": ["com.github.autopkg.download.Firefox_EN"],
      "required_capabilities": ["builder.autopkg", "os.macos"]
    }
  ],
  "diagnostics": []
}
```

Generate the same contract directly for a pinned AutoPkg repository:

```bash
stabbur-server catalog generate-autopkg \
  --source-url https://github.com/example/recipes.git \
  --source-revision 0123456789abcdef0123456789abcdef01234567 \
  --output catalog.json
```

For unattended observation, request a durable scan through the public API or supported CLI:

```bash
stabbur catalog scan request \
  --source-url https://github.com/example/recipes.git \
  --source-revision 0123456789abcdef0123456789abcdef01234567 \
  --idempotency-key recipes-0123456789abcdef
stabbur catalog scan show SCAN_ID
```

Only a registered macOS worker advertising `builder.autopkg` and `os.macos` can claim this job.
The worker materializes the exact commit, emits no native path in the result, and atomically links
a successful scan to its immutable snapshot. `scan list`, `scan show`, and `stabbur --yes catalog
scan cancel` make the workflow automatable. AutoCfg should call this API or consume/produce the
neutral manifest rather than exposing its cache or report types to the control plane.

Call `GET /api/v1/recipe-catalogs` for immutable snapshot metadata and
`GET /api/v1/recipe-catalogs/{snapshot}` for a complete manifest. Resolve exact presence only in
the newest observation for every producer/source pair:

```bash
curl --fail --silent --show-error --get \
  --config /secure/stabbur-admin.curl \
  --data-urlencode 'identifier=Firefox.pkg.recipe' \
  https://stabbur.example.net/api/v1/recipe-catalog-entries
```

An observation is evidence for review, not desired state. It never creates recipe metadata or an
immutable revision, changes a target, or schedules execution.

## Create an AutoPkg revision

The public revision contract is builder-neutral: it carries a stable `builder`, an opaque
`definition` object validated by that adapter, and additional `required_capabilities`. The server
currently accepts the production `autopkg` adapter and deterministic portable `fake` adapter. The
command sent to a worker is structured immutable data, never a command line.

The server catalog is explicit: it does not discover or build every recipe visible in an AutoPkg
repository. Operators bind reviewed revisions to persisted build targets; the full desired-state
boundary is documented in
[catalog discovery and build scheduling](../architecture/catalog-and-scheduling.md).

Create software and recipe metadata, then an AutoPkg revision pinned to one or more full Git
commits. Selectors are reviewed JSON pointers into AutoPkg's report representation:

```bash
software_id=$(curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --json '{"slug":"firefox","name":"Firefox"}' \
  https://stabbur.example.net/api/v1/software | jq -r .id)

recipe_id=$(curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --json '{"name":"firefox-autopkg"}' \
  https://stabbur.example.net/api/v1/recipes | jq -r .id)

revision_id=$(curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --json @firefox-revision.json \
  "https://stabbur.example.net/api/v1/recipes/${recipe_id}/revisions" | jq -r .id)
```

Example `firefox-revision.json`:

```json
{
  "builder": "autopkg",
  "definition": {
    "sources": [
      {
        "url": "https://recipes.example.net/autopkg.git",
        "commit": "0123456789abcdef0123456789abcdef01234567"
      }
    ],
    "entrypoint": "com.example.autopkg.pkg.Firefox",
    "inputs": {
      "LOCALE": "en-US"
    },
    "output": {
      "version_pointer": "/stabbur/receipts/0/1/Output/version",
      "recipe_trust_pointer": "/stabbur/recipe_trust_succeeded",
      "variants": [
        {
          "platform": "mac_os",
          "architecture": "universal",
          "minimum_macos": "13.0",
          "maximum_macos": null,
          "resolution_priority": 0,
          "artifacts": [
            {
              "path_pointer": "/stabbur/receipts/0/2/Output/pkg_path",
              "media_type": "application/vnd.apple.installer+xml",
              "role": "primary_installer"
            }
          ]
        }
      ],
      "verification": [
        {
          "name": "recipe_trust",
          "pointer": "/stabbur/recipe_trust_succeeded",
          "required": true
        },
        {
          "name": "signature",
          "pointer": "/stabbur/receipts/0/3/Output/signature_valid",
          "required": true
        }
      ]
    }
  },
  "required_capabilities": [
    "tool.apple-xcode"
  ]
}
```

Sources must be absolute HTTPS URLs without embedded credentials, query, or fragment. Input and run
parameter names containing password, token, secret, or credential are rejected. Every variant must
select exactly one `primary_installer`. A selected path must resolve to a regular, non-symlink file
inside the isolated attempt directory.

The adapter resolves the exact entrypoint in a bounded, non-symlink recipe-tree scan. A base
recipe is trusted by its materialized full source commits and records trust method `pinned_source`.
An override with `ParentRecipe` must additionally pass AutoPkg `verify-trust-info`, recorded as
`autopkg_parent_trust`; failure aborts the attempt. `recipe_trust_pointer` must be exactly
`/stabbur/recipe_trust_succeeded` and cannot point at recipe-authored data.

AutoPkg's bounded processor receipts are added under `/stabbur/receipts`, which provides the
version, transformations, and retained output paths that the ordinary report plist often omits.
Receipt indices and fields are part of the reviewed immutable selector contract. The combined raw
report is capped at 1 MiB. Attempt-root paths become relative and other absolute paths are redacted;
streaming stdout/stderr replaces the private attempt root even when it crosses read boundaries.

## Persist unattended build policy

A build target binds software, one exact immutable recipe revision, non-secret parameters, and a
manual or fixed-interval policy. This example is eligible immediately and then once per day:

```bash
target_id=$(curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --json "{
    \"name\":\"firefox-en-us-daily\",
    \"software\":\"${software_id}\",
    \"recipe_revision\":\"${revision_id}\",
    \"parameters\":{},
    \"schedule\":{\"kind\":\"interval\",\"every_seconds\":86400}
  }" \
  https://stabbur.example.net/api/v1/build-targets | jq -r .id)
```

The API process polls due cursors every five seconds by default. Configure
`--scheduler-poll-seconds`/`STABBUR_SCHEDULER_POLL_SECONDS`, or use
`--disable-scheduler`/`STABBUR_DISABLE_SCHEDULER=true` for a maintenance or API-only replica.
Multiple scheduler-enabled replicas are safe: only one transaction can consume a target revision
and cursor. Missed intervals advance to the first cursor in the future instead of generating a
catch-up storm.

Read the target ETag before changing its revision, parameters, status, schedule, or cursor. Both
operator changes and successful cursor consumption advance the ETag. Disabling preserves all
target configuration and run history while preventing manual and automatic triggers. Target runs
are available from `GET /api/v1/build-targets/{target}/runs`.

An enabled target can also be triggered explicitly without changing its recurring cursor:

```bash
run_id=$(curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --header 'Idempotency-Key: firefox-en-us-operator-check-1' \
  --request POST \
  "https://stabbur.example.net/api/v1/build-targets/${target_id}/runs" | jq -r .id)
```

## Queue, watch, and cancel a run

Run creation requires a stable idempotency key:

```bash
run_id=$(curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --header 'Idempotency-Key: firefox-en-us-2026-08-26' \
  --json "{\"software\":\"${software_id}\",\"recipe_revision\":\"${revision_id}\",\"parameters\":{}}" \
  https://stabbur.example.net/api/v1/runs | jq -r .id)
```

The server derives capabilities from the revision and any compatible worker can claim it. Inspect
durable state and replayable exact-byte logs:

```bash
curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  "https://stabbur.example.net/api/v1/runs/${run_id}"

curl --no-buffer --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  "https://stabbur.example.net/api/v1/runs/${run_id}/events"
```

SSE accepts an opaque `cursor` or numeric `Last-Event-ID`, emits keepalives while active, and closes
after a terminal `complete` event. The database is the replay source, so API restarts do not lose
logs. Each entry carries an attempt identity and standard-base64 exact bytes.

Cancel queued or running work with another stable key:

```bash
curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --header "Idempotency-Key: cancel-${run_id}" \
  --request POST \
  "https://stabbur.example.net/api/v1/runs/${run_id}/cancel"
```

Cancellation terminalizes the run, invalidates its active lease, and prevents a late worker result
from publishing.

## Result validation and publication

After AutoPkg exits successfully, the worker parses `--report-plist`, applies immutable selectors,
hashes selected files in 64 KiB chunks, uploads them under the active attempt, and submits
builder-neutral provenance and verification evidence. The server independently checks:

1. schema and run/attempt binding;
2. immutable selectors against the raw report;
3. uploaded digest, size, media type, and artifact role;
4. full source commit/tool/worker provenance;
5. recipe trust and every required verification result; and
6. a verified `present` location in the primary store.

Only then does one transaction decide publication and finish the attempt, job, and run. An absent
opaque `(software, version)` creates the release and variants, binds artifact roles, appends
lifecycle/provenance/verification/audit records, and moves the `candidate` channel when its gate
passes. Testing and stable remain explicit promotions; candidate may move directly to stable.

If that opaque version already exists, the server compares its immutable variant/artifact graph,
reproducibility provenance, and verification evidence. Identical evidence produces `no_change`,
links the successful run to the existing release, and leaves lifecycle and channels untouched.
Different evidence produces `version_content_conflict`, links and terminalizes the run as failed,
and also leaves the existing release and channels untouched. Provenance capture timestamps are
per-run observations and are intentionally excluded from equality. Internal worker completion
returns the disposition and release identity; the public run result retains the same publication
record for operators and automation.

## Rotate, disable, and drain workers

Read the current worker ETag, then rotate the credential. Transform the one-time rotation response
back into the two-field worker credential file in a protected transfer location:

```bash
umask 077
worker_id=$(sudo jq -r .worker_id \
  /usr/local/var/lib/stabbur-worker/credential.json)
etag=$(curl --fail --silent --show-error --dump-header - \
  --config /secure/stabbur-admin.curl \
  "https://stabbur.example.net/api/v1/workers/${worker_id}" \
  --output /dev/null | awk 'tolower($1)=="etag:" {gsub("\\r", "", $2); print $2}')

curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --header "If-Match: ${etag}" \
  --request POST \
  "https://stabbur.example.net/api/v1/workers/${worker_id}/rotate-token" \
  > /secure/mac-builder-01.rotation.json
jq '{worker_id: .worker.id, token: .token}' \
  /secure/mac-builder-01.rotation.json \
  > /secure/mac-builder-01.credential.next
rm -f /secure/mac-builder-01.rotation.json
chmod 0600 /secure/mac-builder-01.credential.next

sudo scripts/install-worker-macos.sh \
  --binary target/release/stabbur-server \
  --server-url https://stabbur.example.net \
  --credential-file /secure/mac-builder-01.credential.next \
  --replace-credential \
  --start
rm -f /secure/mac-builder-01.credential.next
```

The old token stops authenticating immediately. The installer atomically replaces the file and
restarts the local launchd job after validating the host.

To drain, stop the local process first and let its current lease expire or cancel the run. To
disable immediately, PATCH `enabled` with the current ETag. Disabling or reducing the capability
ceiling invalidates active attempts and requeues them when retries remain:

```bash
curl --fail --silent --show-error \
  --config /secure/stabbur-admin.curl \
  --header "If-Match: ${etag}" \
  --request PATCH \
  --json '{"enabled":false}' \
  "https://stabbur.example.net/api/v1/workers/${worker_id}"
```

Investigate repeated retries through run logs, job state, and audit events. Do not delete attempt
directories while a worker is running. After a terminal run and required retention period, local
attempt cleanup is an operator task in v0.1.

## Run the deterministic macOS server/worker E2E

The fast live E2E starts a fresh loopback API and a separate outbound worker process, then drives
the public bootstrap, login, software, recipe, fake-revision, worker-provisioning, and build-target
APIs. It requires the complete detected capability set to register, waits for the scheduler to
create the target run and the worker to complete it, and verifies the succeeded run, job payload,
one issued attempt, and audit creation events. It also queues a durable AutoPkg catalog scan and
verifies that the worker claims it and persists the expected typed failure for a deliberately
unreachable loopback HTTPS source, without relying on public network access:

```bash
cargo build --locked
scripts/live-worker-e2e.sh
```

The script requires macOS, `curl`, and `jq`; CI runs it on the macOS ARM64 test leg. It uses only a
temporary SQLite database, store, credentials, and worker directory, and removes them on exit. Set
`STABBUR_SERVER_BIN` to test a packaged binary, `STABBUR_E2E_BIND` to select another loopback bind,
or `STABBUR_E2E_KEEP_TMP=1` to retain logs after a diagnostic run. This fixture validates the
durable target/scheduler/run/job process and protocol boundary without network build inputs or
artifact production.

## Run the full live AutoPkg acceptance fixture

Before a server release, run the complete API-to-worker-to-candidate path on a disposable macOS
host with AutoPkg installed. Supply a revision JSON whose pinned source and selectors resolve to a
reviewed fixture:

```bash
cargo build --release --locked
scripts/live-autopkg-acceptance.sh \
  tests/fixtures/autopkg-live/autopkg-release.json \
  /secure/evidence/stabbur-autopkg-v0.1.json
```

The full script uses a temporary loopback API and fresh SQLite/store/worker directories. It bootstraps
an ephemeral administrator, provisions a one-time worker credential without placing it in process
arguments, creates the immutable revision and run, waits up to two hours, and requires exactly one
primary installer plus a candidate channel. The output records the server and tool versions, pinned
revision, terminal result/provenance, release, variants/artifacts, candidate, and logs. Temporary
credentials and data are removed on exit. Review the evidence for secrets before retaining it.

Set `STABBUR_AUTOPKG_PROGRAM` to an absolute path when testing a nonstandard reviewed runtime. The
committed acceptance input pins an official upstream recipe commit; production revisions should
use an operator-reviewed HTTPS repository at a full commit. Run acceptance in the same egress
policy intended for production; it is deliberately opt-in and not part of the cross-platform
deterministic test suite.

## Public clients

The user-facing CLI is an independent executable using only the supported public client; it never
calls internal claim/heartbeat/result routes. Its intended operator flow is:

```bash
stabbur auth login --server https://stabbur.example.net --username operator
stabbur software list
stabbur run watch RUN_ID
stabbur artifact download SHA256 --output Firefox.pkg --resume
```

Clients create intent, inspect status, cancel, resolve, promote, reject, and download. They do not
run packagers or control worker host processes. Credentials come from owner-only profiles/files,
environment secrets, or interactive prompts and never from password/token command arguments.
