# Linux server and macOS AutoPkg worker

This is the default v0.0.1 deployment: one standard Linux control-plane host with SQLite and a
private filesystem artifact store, plus one separately supervised native macOS AutoPkg worker.
The worker initiates outbound HTTPS; the Linux server never connects to the Mac or executes a
remote shell.

```text
administrators and clients
          |
          | HTTPS
          v
Linux: TLS proxy -> stabbur-server api -> SQLite + private filesystem CAS
                          ^
                          | outbound HTTPS claim/upload/complete
                          |
macOS: launchd -> stabbur-server worker -> AutoPkg
```

The repository is API-backed, not a directory export. Do not serve the CAS directory through a web
server, NFS, or SMB. Authenticated clients resolve software through Stabbur and download immutable
content from the returned API path.

## Before you start

Prepare:

- a Linux host with a dedicated `stabbur` system account, systemd, and a maintained TLS reverse
  proxy;
- a DNS name such as `stabbur.example.net` with a trusted TLS certificate;
- a Mac with a dedicated `_stabbur_worker` role account, launchd, Git, and access to reviewed recipe
  and package sources;
- the same supported `stabbur-server` minor line built for each host;
- `curl`, `jq`, and either `sha256sum` (Linux) or `shasum` (macOS) on the administrator host, and
  `jq` on the Mac while rendering the pinned AutoPkg preparation manifest; and
- protected channels for the first administrator password and the one-time worker credential.

Until immutable release artifacts are published, build on each target platform from the reviewed
checkout:

```bash
cargo build --release --locked
```

Do not copy the Linux executable to the Mac. Install the native binary produced on each platform.

## Install and bootstrap the Linux server

Stage the dedicated service account, native binary, private state directory, and validated systemd
unit with the noninteractive installer:

```bash
sudo scripts/install-server-linux.sh \
  --binary target/release/stabbur-server
```

The script is idempotent and performs atomic binary replacement. It does not accept passwords,
bootstrap an administrator, or start the service unless `--start` is explicit. Pass `--unit PATH`
when the release assets are installed outside the repository layout.

Supply the initial password in a regular mode-0600 file owned by `stabbur`. With every API process
stopped, initialize SQLite, the local primary store, and the first administrator:

```bash
sudo -u stabbur /usr/local/bin/stabbur-server admin bootstrap \
  --data-dir /var/lib/stabbur \
  --username admin \
  --password-file /run/secrets/stabbur-admin-password
sudo rm -f /run/secrets/stabbur-admin-password
```

Activate the installed unit. The same command is the explicit restart step for a later binary
upgrade:

```bash
sudo scripts/install-server-linux.sh \
  --binary target/release/stabbur-server \
  --start
```

Follow server logs with:

```bash
sudo journalctl --unit stabbur-server --follow
```

The service binds only to loopback. Its durable recovery unit is:

```text
/var/lib/stabbur/
  stabbur.db
  stabbur.db-wal
  stabbur.db-shm
  stabbur.db.lock
  store/
    objects/
    uploads/
```

The unit sets an owner-only umask, grants no Linux capabilities, and makes the host filesystem
read-only except for systemd's `/var/lib/stabbur` state directory. Review the hardening directives
against local monitoring, backup, and mandatory-access-control policy before deployment.

## Publish the HTTPS API

Terminate TLS in a maintained reverse proxy on the Linux host and proxy to
`http://127.0.0.1:8080`. Follow the complete requirements in
[deployment and recovery](deployment.md#tls-reverse-proxy). In particular, do not buffer complete
artifact uploads, downloads, or SSE streams; retain range requests; bound headers and idle time;
rate-limit login and bootstrap; replace forwarding headers; and never log `Authorization`.

An nginx starting point is supplied at [stabbur.conf](../examples/nginx/stabbur.conf). Replace its
example DNS name and certificate paths, review its 2 GiB request ceiling and timeouts against local
policy, install it in the distribution's nginx `http` configuration, and validate before reload:

```bash
sudo nginx -t
sudo systemctl reload nginx
```

Set the public origin used below:

```bash
STABBUR_ORIGIN=https://stabbur.example.net
export STABBUR_ORIGIN
curl --fail --silent --show-error "$STABBUR_ORIGIN/readyz"
```

Do not continue until the Mac can validate this origin with the system trust store.

## Create an owner-only administrator API profile

The independently released `stabbur` CLI should normally create and protect its own profile. The
following raw-API procedure is available while assembling a pre-release deployment.

Create a private administrator directory and a temporary login request. Populate the JSON password
value through an editor or secret manager, not a command-line argument:

```bash
ADMIN_STATE="$HOME/.config/stabbur-repository-admin"
install -d -m 0700 "$ADMIN_STATE"
install -m 0600 /dev/null "$ADMIN_STATE/login-request.json"
${EDITOR:?set EDITOR} "$ADMIN_STATE/login-request.json"
```

The request has this shape:

```json
{
  "username": "admin",
  "password": "INITIAL_PASSWORD"
}
```

Exchange it for a short session without putting either credential in process arguments:

```bash
curl --fail --silent --show-error \
  --json @"$ADMIN_STATE/login-request.json" \
  "$STABBUR_ORIGIN/api/v1/auth/login" \
  >"$ADMIN_STATE/login-response.json"
jq -er '.token | "header = \"Authorization: Bearer \(.)\""' \
  "$ADMIN_STATE/login-response.json" \
  >"$ADMIN_STATE/session.curl"
chmod 0600 "$ADMIN_STATE/session.curl"
rm -f "$ADMIN_STATE/login-request.json" "$ADMIN_STATE/login-response.json"
```

Create a named API token and replace the short session profile:

```bash
curl --fail --silent --show-error \
  --config "$ADMIN_STATE/session.curl" \
  --json '{"name":"repository-operator"}' \
  "$STABBUR_ORIGIN/api/v1/auth/principals/admin/tokens" \
  >"$ADMIN_STATE/api-token.json"
jq -er '.secret | "header = \"Authorization: Bearer \(.)\""' \
  "$ADMIN_STATE/api-token.json" \
  >"$ADMIN_STATE/admin.curl"
chmod 0600 "$ADMIN_STATE/admin.curl"
rm -f "$ADMIN_STATE/session.curl" "$ADMIN_STATE/api-token.json"
```

Set a non-secret pointer to that curl profile:

```bash
STABBUR_AUTH_CURL="$ADMIN_STATE/admin.curl"
export STABBUR_AUTH_CURL
```

Use a narrower service principal for routine catalog reconciliation after proving the deployment;
the first administrator profile is used here to keep the initial walkthrough finite.

## Install the worker host on macOS

Set the public origin in the Mac administrator's shell, then stage the native binary, role account,
private directories, and rendered launchd definition:

```bash
STABBUR_ORIGIN=https://stabbur.example.net
export STABBUR_ORIGIN
sudo scripts/install-worker-macos.sh \
  --binary target/release/stabbur-server \
  --server-url "$STABBUR_ORIGIN"
```

The installer selects the first unused Apple role-account UID in 450-499. Use `--uid UID` to make
that allocation deterministic in managed inventory. It validates and preserves an existing
`_stabbur_worker` account on subsequent runs and performs atomic binary and plist replacement. It
does not download AutoPkg, provision a server identity, or load the launchd job without `--start`.
Pass `--plist PATH` when the release assets are installed outside the repository layout.

If the organization provisions role accounts through MDM or directory policy, use that mechanism
and retain the `_stabbur_worker` name expected by the supplied launchd definition.

## Prepare AutoPkg and inspect capabilities

Install the pinned AutoPkg runtime using the fixture and procedure in
[Prepare AutoPkg on macOS](workers.md#prepare-autopkg-on-macos). Always run the non-mutating check
before the privileged operation:

```bash
/usr/local/bin/stabbur-server worker prepare \
  --manifest /private/var/tmp/autopkg-2.9.0.stabbur.json \
  --check
sudo /usr/local/bin/stabbur-server worker prepare \
  --manifest /private/var/tmp/autopkg-2.9.0.stabbur.json
```

Install Git and any required Apple command-line tools, then inspect capabilities as the final worker
account:

```bash
sudo -u _stabbur_worker /usr/local/bin/stabbur-server worker \
  --print-capabilities \
  --autopkg-program /Library/AutoPkg/autopkg
```

Record the exact returned capability set. The provisioning ceiling must contain every advertised
capability.

## Provision the worker credential

Run this from a protected administrator account, not from the worker account. In the split-host
topology, do not stage the credential on the Mac until the one-time protected delivery step:

```bash
umask 077
curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  --json '{"name":"mac-builder-01","allowed_capabilities":["runtime.portable","builder.fake","os.macos","builder.autopkg","tool.apple-xcode"]}' \
  "$STABBUR_ORIGIN/api/v1/workers" \
  >"$ADMIN_STATE/mac-builder-01.credential.json"
chmod 0600 "$ADMIN_STATE/mac-builder-01.credential.json"
```

Deliver that file once through the organization's protected MDM or secret-transfer channel. On the
Mac, install it and activate the already-prepared worker without changing its two-field JSON shape:

```bash
sudo scripts/install-worker-macos.sh \
  --binary target/release/stabbur-server \
  --server-url "$STABBUR_ORIGIN" \
  --credential-file /secure/incoming/mac-builder-01.credential.json \
  --start
sudo rm -f /secure/incoming/mac-builder-01.credential.json
```

Delete the administrator-host transfer copy after delivery. If delivery status is uncertain, do
not provision another worker blindly; inspect the existing record and rotate its credential. The
installer refuses to overwrite a different credential unless credential rotation explicitly uses
`--replace-credential`.

Inspect the loaded job and registration:

```bash
sudo launchctl print system/com.stabbur.worker
```

The worker opens no listener. Its logs are under `/usr/local/var/log/stabbur-worker`; configure the
host's normal log rotation policy for both files. On the Linux server, confirm that the worker
record reports the expected capabilities and a recent registration time before scheduling work.

## Publish the known-good AutoPkg release example

The example at
[autopkg-release-revision.json](../examples/autopkg/autopkg-release-revision.json) is the exact
definition exercised by the committed live acceptance. It pins the upstream AutoPkg recipes
repository to commit `6c092b47e9c6324aa48758832b2597a0f3ff932e`, executes
`com.github.autopkg.download.AutoPkg-Release`, and selects one universal macOS installer.

This is an end-to-end deployment proof, not a universal package policy. The upstream AutoPkg
installer is unsigned, the recipe contacts the release service at execution time, and the example's
only required verification is pinned-recipe trust. Review and strengthen verification for every
production software definition.

Create software and recipe metadata, then post the reviewed immutable revision:

```bash
software_id=$(curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  --json '{"slug":"autopkg","name":"AutoPkg"}' \
  "$STABBUR_ORIGIN/api/v1/software" | jq -er .id)

recipe_id=$(curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  --json '{"name":"autopkg-release"}' \
  "$STABBUR_ORIGIN/api/v1/recipes" | jq -er .id)

revision_id=$(curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  --json @docs/examples/autopkg/autopkg-release-revision.json \
  "$STABBUR_ORIGIN/api/v1/recipes/$recipe_id/revisions" | jq -er .id)
```

Create a manual build target and trigger it with a stable idempotency key:

```bash
target_id=$(curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  --json "{
    \"name\":\"autopkg-release-manual\",
    \"software\":\"$software_id\",
    \"recipe_revision\":\"$revision_id\",
    \"parameters\":{},
    \"schedule\":{\"kind\":\"manual\"}
  }" \
  "$STABBUR_ORIGIN/api/v1/build-targets" | jq -er .id)

run_id=$(curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  --header 'Idempotency-Key: first-autopkg-release-build' \
  --request POST \
  "$STABBUR_ORIGIN/api/v1/build-targets/$target_id/runs" | jq -er .id)
```

Poll durable state until the run succeeds or terminates:

```bash
while :; do
  curl --fail --silent --show-error \
    --config "$STABBUR_AUTH_CURL" \
    "$STABBUR_ORIGIN/api/v1/runs/$run_id" \
    >"$ADMIN_STATE/run.json"
  state=$(jq -er .state "$ADMIN_STATE/run.json")
  case "$state" in
    succeeded) break ;;
    failed|cancelled)
      jq . "$ADMIN_STATE/run.json" >&2
      exit 1
      ;;
  esac
  sleep 10
done
```

The worker hashes and uploads the selected package. The server independently rehashes it, stores it
under `/var/lib/stabbur/store`, creates the opaque release, and moves `candidate` only after required
policy passes. Verify the release and candidate binding:

```bash
release_id=$(curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  "$STABBUR_ORIGIN/api/v1/software/$software_id/releases" | jq -er '.items[0].id')

curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  "$STABBUR_ORIGIN/api/v1/software/$software_id/channels/candidate" \
  | jq -e --arg release "$release_id" '.release_id == $release'
```

For this fresh software identity, promote directly to stable with the channel-creation ETag:

```bash
curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  --request PUT \
  --header 'If-Match: "rev-0"' \
  --json "{\"release_id\":\"$release_id\",\"reason\":\"initial repository acceptance\"}" \
  "$STABBUR_ORIGIN/api/v1/software/$software_id/channels/stable" \
  >"$ADMIN_STATE/stable.json"
```

Resolve for an Apple Silicon Mac, download through the authenticated immutable content path, and
rehash the result:

```bash
curl --fail --silent --show-error --get \
  --config "$STABBUR_AUTH_CURL" \
  --data-urlencode 'channel=stable' \
  --data-urlencode 'platform=mac_os' \
  --data-urlencode 'architecture=aarch64' \
  --data-urlencode 'macos=15.0' \
  "$STABBUR_ORIGIN/api/v1/software/autopkg/resolve" \
  >"$ADMIN_STATE/resolution.json"

artifact_digest=$(jq -er .artifact_digest "$ADMIN_STATE/resolution.json")
content_path=$(jq -er .content_path "$ADMIN_STATE/resolution.json")
curl --fail --silent --show-error \
  --config "$STABBUR_AUTH_CURL" \
  --output "$ADMIN_STATE/AutoPkg.pkg" \
  "$STABBUR_ORIGIN$content_path"
if command -v sha256sum >/dev/null 2>&1; then
  actual_digest=$(sha256sum "$ADMIN_STATE/AutoPkg.pkg" | awk '{print $1}')
else
  actual_digest=$(shasum -a 256 "$ADMIN_STATE/AutoPkg.pkg" | awk '{print $1}')
fi
test "$actual_digest" = "$artifact_digest"
```

You now have a filesystem-backed Stabbur repository with a real macOS artifact, immutable release
history, a stable channel, compatibility resolution, and authenticated range-capable delivery.

## Turn the proof into an operated repository

For each additional product:

1. observe an exact pinned recipe source through a catalog scan;
2. review and allowlist the intended entrypoint;
3. create software, recipe metadata, and an immutable builder definition with product-specific
   selectors and verification;
4. bind it to a manual or fixed-interval target;
5. review candidate output and provenance before stable promotion; and
6. give consumers narrow read/resolve credentials rather than an administrator token.

Back up the stopped Linux server's complete `/var/lib/stabbur` directory as one recovery unit.
SQLite without the CAS, or the CAS without SQLite, is not a valid backup. Follow
[coordinated backup](deployment.md#coordinated-backup), monitor free space and WAL growth, and test
authenticated resolve plus an artifact range after every restore.

To upgrade either host, rerun its installer with the new native binary and `--start`. The installer
atomically replaces files and restarts an already-active service only when activation was explicit.
Check the server health endpoint and worker registration after either operation.

v0.0.1 does not provide manual package import, direct directory serving, Munki export, replication,
or automatic store repair. Packages enter the primary store only through a validated worker result.
