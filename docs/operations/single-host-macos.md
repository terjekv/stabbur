# Single-host macOS repository

The default deployment uses a Linux server and a separate Mac worker. This variant places the
control plane, private filesystem CAS, and AutoPkg worker on one Mac while preserving separate
non-login accounts and processes. Use it for a small repository, lab, or evaluation where the
single host is an acceptable failure and trust boundary.

Read [Linux server and macOS AutoPkg worker](linux-server-macos-worker.md) first. Its authentication,
worker provisioning, reviewed recipe, build, promotion, resolve, and backup policies still apply.
Only service placement and durable paths differ here.

## Install the native binary and server role account

Build and install one native macOS binary:

```bash
cargo build --release --locked
sudo install -o root -g wheel -m 0755 \
  target/release/stabbur-server \
  /usr/local/bin/stabbur-server
```

Inspect allocated role-account IDs and choose one unused value in Apple's 450-499 range:

```bash
dscl . -list /Users UniqueID | awk '$2 >= 450 && $2 <= 499'
SERVER_UID=450
```

Do not reuse an ID printed by the first command. Create the non-login server role account:

```bash
sudo sysadminctl -addUser _stabbur \
  -fullName "Stabbur Server" \
  -UID "$SERVER_UID" \
  -GID 20 \
  -shell /usr/bin/false \
  -home /var/empty \
  -roleAccount
id _stabbur
```

Create the owner-only server state and log directories:

```bash
sudo install -d -o _stabbur -g staff -m 0700 \
  /usr/local/var/lib/stabbur \
  /usr/local/var/log/stabbur-server
```

## Bootstrap and supervise the server

Supply an owner-only initial password file, then initialize the repository before loading the
server daemon:

```bash
sudo -u _stabbur /usr/local/bin/stabbur-server admin bootstrap \
  --data-dir /usr/local/var/lib/stabbur \
  --username admin \
  --password-file /private/var/run/stabbur-admin-password
sudo rm -f /private/var/run/stabbur-admin-password
```

Install and load the supplied launchd server definition:

```bash
plutil -lint docs/examples/launchd/com.stabbur.server.plist
sudo install -o root -g wheel -m 0644 \
  docs/examples/launchd/com.stabbur.server.plist \
  /Library/LaunchDaemons/com.stabbur.server.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.stabbur.server.plist
sudo launchctl enable system/com.stabbur.server
sudo launchctl kickstart -k system/com.stabbur.server
curl --fail --silent --show-error http://127.0.0.1:8080/readyz
```

For a host-local repository, use this origin during setup:

```bash
STABBUR_ORIGIN=http://127.0.0.1:8080
export STABBUR_ORIGIN
```

If any administrator or consumer connects over the network, place the loopback listener behind a
maintained TLS reverse proxy and change `STABBUR_ORIGIN` to its HTTPS origin before issuing
credentials.

## Add the worker

Stage the worker with the loopback origin. Its installer creates the separate worker role account
and private directories:

```bash
sudo scripts/install-worker-macos.sh \
  --binary target/release/stabbur-server \
  --server-url "$STABBUR_ORIGIN"
```

Continue with these sections of the default guide:

1. [Create an owner-only administrator API profile](linux-server-macos-worker.md#create-an-owner-only-administrator-api-profile).
2. [Prepare AutoPkg and inspect capabilities](linux-server-macos-worker.md#prepare-autopkg-and-inspect-capabilities).
3. [Provision the worker credential](linux-server-macos-worker.md#provision-the-worker-credential).

For this topology, perform the provisioning step from the Mac's protected administrator login, not
from `_stabbur_worker`.

Install the issued credential and load the worker:

```bash
sudo scripts/install-worker-macos.sh \
  --binary target/release/stabbur-server \
  --server-url "$STABBUR_ORIGIN" \
  --credential-file /secure/incoming/mac-builder-01.credential.json \
  --start
sudo rm -f /secure/incoming/mac-builder-01.credential.json
sudo launchctl print system/com.stabbur.worker
```

Run the default guide's
[known-good AutoPkg release example](linux-server-macos-worker.md#publish-the-known-good-autopkg-release-example)
to create the first artifact, candidate, stable channel, resolution, and verified download.

## Operate the single recovery unit

The durable repository is `/usr/local/var/lib/stabbur`. The worker directory contains execution
state and its bearer credential, not artifact metadata. Back it up separately if credential
continuity matters, or provision a new worker identity after recovery. Stop both launchd jobs
before taking the safest v0.0.1 backup:

```bash
sudo launchctl bootout system/com.stabbur.worker
sudo launchctl bootout system/com.stabbur.server
sudo -u _stabbur /usr/local/bin/stabbur-server admin doctor \
  --data-dir /usr/local/var/lib/stabbur
```

Copy the complete server directory with ownership and modes preserved, then reload the server before
the worker:

```bash
sudo launchctl bootstrap system /Library/LaunchDaemons/com.stabbur.server.plist
curl --fail --silent --show-error http://127.0.0.1:8080/readyz
sudo launchctl bootstrap system /Library/LaunchDaemons/com.stabbur.worker.plist
```

Do not back up or restore SQLite separately from `store/`. Configure rotation for the two launchd
log directories and monitor the shared host for disk pressure: a full volume affects both
control-plane persistence and AutoPkg execution.
