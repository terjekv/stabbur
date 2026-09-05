# Single-host macOS delivery acceptance

The cross-repository integration workflow runs one Linux job for public contracts and
authentication, and one `macos-15` job for the complete delivery workflow. Each job checks out
the server, supported client, CLI and frontend side by side. The Mac runs an API server, outbound
worker, frontend session backend, headless Chromium, and temporary fixture HTTP servers as
separate local processes. It does not use Docker or launchd.

One host still exercises real HTTP, worker credentials, leases, stream uploads, SQLite, the
filesystem CAS and the supported client. Multiple hosted-runner jobs have separate machines
and do not share localhost. They would need a reachable server plus networking and lifecycle
coordination; that is a separate deployment test, not a prerequisite for this acceptance suite.
See [GitHub's runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## What the Mac job checks

1. The existing independent client, CLI reconciliation/pagination and frontend authentication
   integration passes against identical pinned OpenAPI documents.
2. A tiny script-free package containing one text file is generated with Apple's `pkgbuild`.
   A local appcast advertises that exact package with version `1.0`.
3. The browser logs in, reviews and applies a catalog plan, and triggers an AutoPkg build target.
   AutoPkg 2.9.0 runs the XLD download recipe from the pinned upstream recipe commit with only
   `NAME` and `SPARKLE_FEED_URL` inputs changed. Its name is incidental: the recipe's appcast and
   download processors consume the fixture. No XLD application is downloaded or installed.
4. The worker uploads the output, the server verifies it and creates a candidate. Browser log
   inspection and promotion run through the frontend and supported client. A stale promotion
   revision is rejected. CLI export must preserve the exact generated package SHA-256 and map
   Stabbur's `aarch64` to Munki's `arm64`.
5. Munki 7.2.0 generates its catalog, schedules the fixture, installs it, and detects the exact
   payload and package receipt. A second check must schedule no reinstall.
6. The stopped server's complete database and CAS are copied and restored into a new directory.
   The original credential, channel revision and artifact bytes must remain usable.
7. A job is claimed but abandoned, the server receives `SIGKILL`, and a real restarted worker
   reclaims the naturally expired lease. Exactly two attempts are recorded and the old lease
   cannot heartbeat. The fixture does not edit database timestamps or bypass lease validation.
8. Browser withdrawal clears delivery selection and prevents another CLI export. The previously
   exported Munki snapshot remains: retraction of an already published external repository is
   an operator responsibility. Browser logout/login and simulated browser expiry are also checked.

The crash drill uses the deterministic fake builder for job mechanics; the preceding delivery
uses real AutoPkg. Mid-upload crash injection and separate-host network partitions are not covered.
Browser clock simulation checks UI expiry; backend expiry is covered by the frontend Rust tests.
Loopback HTTP deliberately does not establish production TLS or reverse-proxy compatibility.

## Run locally without installing software

Use an existing AutoPkg 2.9.0 executable and Munki tools, or supply paths to isolated extracted
tools. The normal harness never changes Munki preferences, installs software, or starts a
background service. Build all four repositories and install test tooling into a temporary venv:

```bash
python3 scripts/check-workspace-integration.py --target target
python3 -m venv /private/tmp/stabbur-e2e-python
/private/tmp/stabbur-e2e-python/bin/python -m pip install -r scripts/requirements-e2e.txt
/private/tmp/stabbur-e2e-python/bin/python -m playwright install chromium
/private/tmp/stabbur-e2e-python/bin/python scripts/e2e-single-host.py \
  --cli target/debug/stabbur --frontend target/debug/stabbur-frontend \
  --autopkg /Library/AutoPkg/autopkg --munki-tools /usr/local/munki \
  --browser --evidence target/single-host-evidence.json
```

Use Python 3.11 or newer. Omitting `--browser` exercises catalog, trigger, promotion and withdrawal
through the authenticated frontend HTTP gateway instead. That mode does not claim browser coverage.
Child commands and service readiness have deadlines; the workflow also has a job timeout. Process
groups and temporary directories are cleaned up on normal completion and exceptions.

## Disposable installation mode

On a fresh test VM only, prepare the pinned installers and enable the installation leg:

```bash
STABBUR_DISPOSABLE_MACOS=1 scripts/live-worker-prepare-autopkg.sh
STABBUR_DISPOSABLE_MACOS=1 sh scripts/e2e-prepare-munki.sh
STABBUR_DISPOSABLE_MACOS=1 /private/tmp/stabbur-e2e-python/bin/python \
  scripts/e2e-single-host.py --cli target/debug/stabbur \
  --frontend target/debug/stabbur-frontend --browser --install-fixture \
  --evidence target/single-host-evidence.json
```

These commands require passwordless `sudo`. Both tool installers are pinned by size and SHA-256.
Munki preparation installs only its core and administration packages, without app or launchd jobs.
The delivery harness refuses to replace existing Munki preferences. Its generated payload has a
unique destination under `/private/var/tmp` and a unique receipt identifier; both are removed
after the test. AutoPkg and Munki tooling remain installed until the VM is discarded.

## GitHub configuration and evidence

The workflow currently names repositories under `stabbur-dev`. Set these to the actual repository
owner if different. For private siblings, provide `STABBUR_REPOSITORIES_READ_TOKEN` with read access
to those repositories. Public siblings use the ordinary job token. Checkout credentials are not
persisted, and this token is never passed to workers or browser tests. Fork pull requests cannot
use a private-repository secret; run coordinated private checks from a trusted branch.

Manual dispatch accepts client, CLI and frontend refs. Use commit SHAs for repeatability; defaults
of `main` are development integration. The job records every checked revision. The final JSON
report records source revisions and dirty status, binary hashes, tool versions, the last stage,
and individual acceptance results. Only that report is uploaded, including on failure. Databases,
credentials, process logs, browser traces and authentication state are excluded from CI artifacts.
This is source-build evidence, not an immutable released-container compatibility claim.
