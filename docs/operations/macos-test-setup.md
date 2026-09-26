# A complete local macOS test installation

Use `scripts/install-test-macos.py` to create a persistent test setup containing the server API,
SQLite and artifact storage, a separate outbound worker, the CLI and the management frontend.
It creates a first administrator, logs the CLI in, provisions a worker credential, and verifies
that the worker registers. Browser login uses that same administrator through the frontend's
protected server-side session gateway.

Run it as your normal, logged-in macOS user with Python 3.11+, Rust 1.88+ and Xcode Command Line
Tools available. The Python script uses only the standard library. It builds native Rust binaries;
Docker, Node.js, a database service and a reverse proxy are unnecessary for this loopback test setup.

From a reviewed checkout of this repository:

```sh
python3 scripts/install-test-macos.py install
```

The default prefix is `~/Library/Application Support/Stabbur Test`, the API listens on
`http://127.0.0.1:8080`, and the management UI is at `http://127.0.0.1:3000`.
Open the exact Management UI address printed by the installer, including its hostname and port.
If login fails after replacing `127.0.0.1` with `localhost`, return to the printed address: those
are different browser origins, and the console requires its configured origin for sign-in.
The installer reports the location of an owner-only file containing a randomly generated
administrator password. Read that file locally and sign in as `admin`; the password is never
printed by the installer. Alternatively supply `--username` and an owner-only `--password-file`.
The temporary copy of a supplied password is removed after bootstrap and login; the original
file is untouched. The server enforces its password policy in both cases.

The source build uses this server checkout and downloads the client, CLI and frontend at the exact
commits pinned in the script. Fetches use HTTPS and verify the resulting commit IDs. All three
OpenAPI contracts must agree, and Cargo uses committed lockfiles. Compilation output goes to
`logs/build.log`; `provenance.json` records source revisions, dirty checkout state and installed
binary SHA-256 digests. Debug builds are the default for testing; use `--build-profile release`
for optimized binaries. Builds can take several minutes and require several gigabytes of disk.

## Choose directories and ports

Pass port options **after `install`**. For example, if the default ports are occupied:

```sh
python3 scripts/install-test-macos.py --prefix /tmp/stabbur install \
  --api-port 18080 --web-port 13000
```

The API will use `http://127.0.0.1:18080` and the UI `http://127.0.0.1:13000`.
Choose two different available ports between 1024 and 65535. Both values are saved with the
installation and reused by `start` and `restart`.
Run `python3 scripts/install-test-macos.py --help` to see global and installation options,
or `python3 scripts/install-test-macos.py install --help` for installation options alone.

Every destination must be a **new directory**. The installer refuses existing files, directories,
symlinks, overlapping destinations and unsafe parents. It never changes permissions on an existing
directory. Parent directories can already exist. Paths containing spaces are supported.

```sh
python3 scripts/install-test-macos.py --prefix "$HOME/stabbur-lab/app" install \
  --server-data "$HOME/stabbur-lab/server-state" \
  --worker-data "$HOME/stabbur-lab/worker-state" \
  --logs-dir "$HOME/stabbur-lab/logs" \
  --sources-dir "$HOME/stabbur-lab/sources" \
  --build-dir "$HOME/stabbur-lab/build" \
  --api-port 18080 \
  --web-port 13000 \
  --username lab-admin
```

The prefix contains `bin/`, `libexec/`, `launchd/`, `secrets/`, `home/` and `tmp/`.
Unspecified data, log, source and build directories also live under the prefix. Server and worker
state remain separate. The worker gets its own server-issued credential; it never accesses SQLite.
The generated CLI wrapper selects only this test server and this installation's private profile.

For coordinated local development, use existing sibling checkouts instead of fetching companions:

```sh
python3 scripts/install-test-macos.py --prefix "$HOME/stabbur-dev-test" install \
  --workspace "$HOME/projects/private"
```

That workspace must contain `stabbur`, `stabbur-client-rust`, `stabbur-cli` and `stabbur-frontend`.
The installer builds their current contents and records dirty state without resetting or modifying
the checkouts. As with any Cargo build, use source you trust.

CI and developers who already built the programs can skip compilation entirely:

```sh
python3 scripts/install-test-macos.py --prefix "$HOME/stabbur-binary-test" install \
  --server-binary target/debug/stabbur-server \
  --cli-binary ../stabbur-cli/target/debug/stabbur \
  --frontend-binary ../stabbur-frontend/target/debug/stabbur-frontend
```

All three binaries are required together and are copied into the private installation. Prebuilt
mode records their hashes; it cannot establish their source provenance or compare source contracts.

## Start, stop and use the setup

Installation starts all three services. For the default prefix:

```sh
"$HOME/Library/Application Support/Stabbur Test/bin/stabbur-test" status
"$HOME/Library/Application Support/Stabbur Test/bin/stabbur" status
"$HOME/Library/Application Support/Stabbur Test/bin/stabbur" worker list
"$HOME/Library/Application Support/Stabbur Test/bin/stabbur-test" restart
"$HOME/Library/Application Support/Stabbur Test/bin/stabbur-test" stop
"$HOME/Library/Application Support/Stabbur Test/bin/stabbur-test" start
```

`status` returns JSON and exits nonzero if a service is stopped or an HTTP readiness check fails.
Start and stop are repeatable. Restart preserves the database, artifacts, worker identity and CLI
profile. Frontend sessions are in memory, so sign in again after restarting it. CLI sessions also
expire; renew one with the wrapper's `auth login --username admin`, which prompts for the password.
After changing the initial administrator password, remove the initial password file when no longer
needed. It is a setup convenience, not a password vault or recovery mechanism.

Services use unique launchd labels derived from the installation prefix and the current user's
`gui/<uid>` domain. They restart after a crash while loaded. The installer does not add anything
to `~/Library/LaunchAgents` or system LaunchDaemons: after logout or reboot, run `start` from a
logged-in macOS session. A headless host without that user domain should use the
[production macOS runbook](single-host-macos.md).

Both listeners are fixed to IPv4 loopback. The frontend explicitly uses its local development
cookie mode. Service environments and the CLI wrapper discard inherited Stabbur settings,
credentials, proxies and loader variables. Services get an isolated home and temporary directory.
Installation files and logs are owner-only. This is a same-user testing boundary: it does not
isolate untrusted recipes from your macOS account. Only test recipes you trust. For remote access
and stronger account isolation, follow the production runbook and its TLS configuration.

Port conflicts fail without stopping the existing listener. Choose different ports at installation
time. Multiple installations require different prefixes, data directories and ports; the browser's
loopback development cookie is shared across ports, so use separate browser profiles for simultaneous
frontend logins.

## AutoPkg and Munki

An existing `/Library/AutoPkg/autopkg` is detected automatically. Select a different installation
with `--autopkg-program /absolute/path/to/autopkg`. The installer probes the executable before
provisioning the worker and rejects a selected tool that cannot advertise the AutoPkg capability.
Without AutoPkg, the complete management stack and deterministic fake builder still work.

To explicitly install the pinned AutoPkg 2.9.0 package on a Mac where AutoPkg is absent:

```sh
python3 scripts/install-test-macos.py --prefix "$HOME/stabbur-autopkg-test" install \
  --install-autopkg
```

This option downloads the committed fixture's exact package, checks its byte length and SHA-256,
and uses `stabbur-server worker prepare` to inspect and install it. This particular upstream
package is unsigned; the committed package digest is the trust pin. Only that preparation step
uses `sudo` and may prompt for your macOS password. It installs AutoPkg system-wide and refuses
to replace an existing AutoPkg directory or package receipt. Stopping or removing the test setup
does not uninstall AutoPkg. The default installer command never installs system packages.

Munki is optional delivery tooling and is not installed or configured by this script. The CLI's
`munki-export` can export approved artifacts into a directory you select. For a disposable test
that installs and detects a real package with Munki, use the separate
[single-host E2E harness](single-host-e2e.md); its system-changing fixture is intended for a
disposable Mac or GitHub runner.

## Failures, cleanup and CI

The installer never overwrites an existing installation or deletes its state, including after a
failed setup. Once service configuration exists, failures during bootstrap and startup stop that
setup's services. Earlier failures can leave private build files for diagnosis. Fix the reported
prerequisite and use a fresh prefix and destination directories; automatic in-place upgrades and
credential resets are deliberately outside this test installer.

Always run `bin/stabbur-test stop` before removing a setup. Then remove only the installation
prefix and any external directories you selected, after preserving data you want to keep.
There is no recursive uninstall command that could erase a mistakenly selected data directory.
Do not move a configured prefix: its launchd definitions and wrappers contain absolute paths.

Portable safety tests run in the normal static CI job:

```sh
python3 scripts/test-macos-test-setup.py
```

The cross-repository macOS job also exercises a real installation using its compiled binaries and
prepared AutoPkg. It verifies unusual/custom paths, private credentials, CLI and frontend login,
worker registration, durable state across restart, repeated start/stop, and occupied-port refusal.
It also checks supplied-password cleanup and rollback after a rejected bootstrap.
All services and temporary state are cleaned up. Run that acceptance locally without installing
system software:

```sh
python3 scripts/test-macos-test-setup.py --live \
  --cli target/debug/stabbur \
  --frontend target/debug/stabbur-frontend
```

Add `--autopkg /absolute/path/to/autopkg` to require and verify its worker capability too.
