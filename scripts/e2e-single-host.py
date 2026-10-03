#!/usr/bin/env python3
"""Real single-host macOS delivery acceptance. All services and data are disposable.

Default mode never installs a package or changes Munki preferences. --install-fixture
requires an explicitly disposable host and runs the complete Munki install/check cycle.
"""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
from http.cookiejar import CookieJar
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import platform
import plistlib
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
import shutil

ROOT = Path(__file__).resolve().parents[1]
PASSWORD = 'Local-fixture-only-password-482!'
RECIPE_COMMIT = '6c092b47e9c6324aa48758832b2597a0f3ff932e'
PAYLOAD = b'Stabbur disposable delivery fixture 1.0\n'


def require(condition, label):
    if not condition:
        raise AssertionError(label)


def command(argv, **kwargs):
    # Never print argv/environment: some child commands receive fixture credentials.
    result = subprocess.run([str(arg) for arg in argv], capture_output=True, timeout=180, **kwargs)
    require(result.returncode == 0, f'{Path(argv[0]).name} exited {result.returncode}')
    return result.stdout


def source_evidence(directory):
    result = subprocess.run(['git', '-C', str(directory), 'rev-parse', 'HEAD'],
                            capture_output=True, timeout=10)
    revision = result.stdout.decode().strip() if result.returncode == 0 else None
    status = subprocess.run(['git', '-C', str(directory), 'status', '--porcelain'],
                            capture_output=True, timeout=10)
    return {'commit': revision, 'dirty': bool(status.stdout) or status.returncode != 0}


def free_port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


class Http:
    def __init__(self, origin, headers=None):
        self.origin = origin
        self.headers = headers or {}
        self.opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(CookieJar()))

    def call(self, path, data=None, *, method=None, expected=200, headers=None):
        request = urllib.request.Request(self.origin + path,
            data=None if data is None else json.dumps(data).encode(), method=method,
            headers={'content-type': 'application/json', **self.headers, **(headers or {})})
        try:
            with self.opener.open(request, timeout=20) as response:
                status, body, mime = response.status, response.read(), response.headers.get('content-type', '')
        except urllib.error.HTTPError as error:
            status, body, mime = error.code, error.read(), error.headers.get('content-type', '')
        require(status in (expected if isinstance(expected, tuple) else (expected,)),
                f'{request.get_method()} {path.split("?")[0]} returned {status}; expected {expected}')
        return json.loads(body) if body and 'json' in mime else body


def wait_for(predicate, label, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(.25)
    raise AssertionError(f'timed out: {label}')


class Processes:
    def __init__(self, work):
        self.work, self.children = work, []
        self.stopped = set()

    def start(self, name, argv, env):
        # A process group also owns AutoPkg/git children, even when a worker is killed.
        with (self.work / f'{name}.log').open('ab') as log:
            child = subprocess.Popen([str(arg) for arg in argv], env=env, stdout=log,
                                     stderr=log, start_new_session=True)
        self.children.append(child)
        return child

    def stop(self, child, hard=False):
        if child in self.stopped:
            return
        try:
            os.killpg(child.pid, signal.SIGKILL if hard else signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=8)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=5)
        self.stopped.add(child)

    def close(self):
        for child in reversed(self.children):
            self.stop(child)


def ready(http, child):
    def probe():
        require(child.poll() is None, 'service exited before readiness')
        try:
            return bool(http.call('/readyz'))
        except (OSError, AssertionError):
            return False
    wait_for(probe, 'server readiness', 30)


@contextmanager
def fileserver(directory):
    class Handler(SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=str(directory), **kwargs)

        def log_message(self, *_args):
            pass
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f'http://127.0.0.1:{server.server_port}'
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def fixture(work, feed_origin, version='1.0', identifier=None):
    identifier = identifier or 'dev.stabbur.e2e.' + uuid.uuid4().hex
    destination = '/private/var/tmp/' + identifier
    payload = work / 'payload'
    payload.mkdir(exist_ok=True)
    (payload / 'proof.txt').write_bytes(f'Stabbur disposable delivery fixture {version}\n'.encode())
    package = work / f'public/fixture-{version}.pkg'
    command(['/usr/bin/pkgbuild', '--root', payload, '--identifier', identifier,
             '--version', version, '--install-location', destination, package])
    (work / 'public/appcast.xml').write_text(
        '<?xml version="1.0"?><rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">'
        f'<channel><title>Stabbur CI</title><item><title>{version}</title>'
        f'<enclosure url="{feed_origin}/{package.name}" sparkle:version="{version}" '
        f'sparkle:shortVersionString="{version}" length="{package.stat().st_size}" '
        'type="application/octet-stream"/></item></channel></rss>')
    definition = {
        'sources': [{'url': 'https://github.com/autopkg/recipes.git', 'commit': RECIPE_COMMIT}],
        'entrypoint': 'com.github.autopkg.download.XLD',
        'inputs': {'NAME': 'StabburFixture', 'SPARKLE_FEED_URL': feed_origin + '/appcast.xml'},
        'output': {
            'version_pointer': '/stabbur/outputs/version',
            'recipe_trust_pointer': '/stabbur/recipe_trust_succeeded',
            'variants': [{'platform': 'mac_os', 'architecture': 'aarch64',
                          'minimum_macos': '13.0', 'maximum_macos': None, 'resolution_priority': 0,
                          'artifacts': [{'path_pointer': '/stabbur/outputs/pathname',
                                         'media_type': 'application/vnd.apple.installer+xml',
                                         'role': 'primary_installer'}]}],
            'verification': [{'name': 'recipe_trust', 'pointer': '/stabbur/recipe_trust_succeeded', 'required': True}]}}
    manifest = {'schema_version': 2,
        'software': [{'slug': 'delivery-fixture', 'name': 'Delivery fixture'}],
        'recipes': [{'name': 'delivery-recipe', 'revision': {'builder': 'autopkg',
                     'definition': definition, 'required_capabilities': []}}],
        'targets': [{'name': 'delivery-build', 'software': 'delivery-fixture',
                     'recipe': 'delivery-recipe', 'parameters': {}, 'schedule': {'kind': 'manual'}, 'enabled': False}]}
    (work / 'catalog.json').write_text(json.dumps(manifest))
    metadata = {'installs': [{'type': 'file', 'path': destination + '/proof.txt',
                             'md5checksum': hashlib.md5(PAYLOAD, usedforsecurity=False).hexdigest()}],
                'receipts': [{'packageid': identifier, 'version': '1.0'}],
                'unattended_install': True, 'uninstallable': False}
    (work / 'pkginfo.json').write_text(json.dumps(metadata))
    return manifest, package, identifier, Path(destination)


def munki_cycle(args, work, export, identifier, destination):
    tools = args.munki_tools.resolve()
    command([tools / 'makecatalogs', export])
    catalog = plistlib.loads((export / 'catalogs/stable').read_bytes())
    require(len(catalog) == 1 and catalog[0]['name'] == 'delivery-fixture', 'Munki catalog reads the export')
    if not args.install_fixture:
        return 'export-validated; installation intentionally skipped'
    require(os.environ.get('STABBUR_DISPOSABLE_MACOS') == '1', 'installation requires STABBUR_DISPOSABLE_MACOS=1')
    require(not destination.exists(), 'fixture destination must be absent')
    preferences = Path('/Library/Preferences/ManagedInstalls.plist')
    require(not preferences.exists(), 'refusing to replace existing Munki preferences')
    managed = work / 'managed'
    managed.mkdir()
    (export / 'manifests').mkdir()
    (export / 'manifests/stabbur-e2e').write_bytes(plistlib.dumps({
        'catalogs': ['stable'], 'managed_installs': ['delivery-fixture']}))
    with fileserver(export) as origin:
        config = work / 'ManagedInstalls.plist'
        config.write_bytes(plistlib.dumps({'ManagedInstallDir': str(managed),
            'SoftwareRepoURL': origin, 'ClientIdentifier': 'stabbur-e2e',
            'InstallAppleSoftwareUpdates': False, 'SuppressAutoInstall': True,
            'LogFile': str(work / 'munki.log')}))
        command(['sudo', '-n', '/usr/bin/defaults', 'import', preferences.with_suffix(''), config])
        try:
            command(['sudo', '-n', tools / 'managedsoftwareupdate', '--checkonly', '--munkipkgsonly'])
            before = plistlib.loads((managed / 'InstallInfo.plist').read_bytes())
            require(any(item['name'] == 'delivery-fixture' for item in before.get('managed_installs', [])), 'Munki schedules missing fixture')
            command(['sudo', '-n', tools / 'managedsoftwareupdate', '--installonly', '--munkipkgsonly'])
            require((destination / 'proof.txt').read_bytes() == PAYLOAD, 'Munki installed exact payload')
            receipt = plistlib.loads(command(['/usr/sbin/pkgutil', '--pkg-info-plist', identifier]))
            require(receipt['pkg-version'] == '1.0', 'installed receipt version')
            command(['sudo', '-n', tools / 'managedsoftwareupdate', '--checkonly', '--munkipkgsonly'])
            after = plistlib.loads((managed / 'InstallInfo.plist').read_bytes())
            require(not after.get('managed_installs'), 'Munki detects installed fixture; no reinstall')
        finally:
            command(['sudo', '-n', '/usr/bin/defaults', 'delete', preferences.with_suffix('')])
            command(['sudo', '-n', '/bin/rm', '-f', preferences])
            # Only the unique, script-generated fixture path and receipt are removed.
            command(['sudo', '-n', '/bin/rm', '-rf', destination])
            subprocess.run(['sudo', '-n', '/usr/sbin/pkgutil', '--forget', identifier], capture_output=True, timeout=30)
            command(['sudo', '-n', '/usr/sbin/chown', '-R', str(os.getuid()), managed])
    return 'installed-by-munki; receipt/payload verified; second check schedules no reinstall'


def discover_import(api, gateway, manifest, work):
    """Build the delivery definition from a real pinned worker scan and reviewed import."""
    scan = api.call('/api/v1/recipe-catalog-scans', {
        'producer': 'autopkg',
        'source': {'locator': 'https://github.com/autopkg/recipes.git', 'revision': RECIPE_COMMIT}},
        headers={'idempotency-key': 'delivery-discovery'}, expected=201)

    def scanned():
        result = api.call('/api/v1/recipe-catalog-scans/' + scan['id'])
        require(result['state'] not in ('failed', 'cancelled'), 'pinned catalog scan failed')
        return result if result['state'] == 'succeeded' else None

    completed = wait_for(scanned, 'pinned worker recipe discovery', 240)
    imported = gateway.call('/api/recipe-import', {
        'snapshot': completed['snapshot_id'],
        'selections': [{'identifier': 'com.github.autopkg.download.XLD', 'slug': 'delivery-fixture',
                        'name': 'Delivery fixture', 'architecture': 'aarch64', 'minimum_macos': '13.0',
                        'version_variable': 'version', 'artifact_variable': 'pathname',
                        'media_type': 'application/vnd.apple.installer+xml'}]})
    require(imported['targets'][0]['enabled'] is False and
            imported['targets'][0]['schedule'] == {'kind': 'manual'}, 'import starts disabled and manual')
    require(not api.call('/api/v1/runs')['items'], 'discovery and import never start builds')
    definition = imported['recipes'][0]['revision']['definition']
    require(definition['sources'] == manifest['recipes'][0]['revision']['definition']['sources'],
            'import preserves the exact reviewed source pin')
    # The operator-reviewed fixture changes only NAME and the loopback feed. This keeps actual
    # downloads and installation restricted to the disposable package generated by this harness.
    definition['inputs'] = manifest['recipes'][0]['revision']['definition']['inputs']
    manifest['recipes'][0]['revision'] = imported['recipes'][0]['revision']
    (work / 'catalog.json').write_text(json.dumps(manifest))


def run(args, work, evidence):
    processes = Processes(work)
    # Explicitly exclude application configuration and credentials inherited from a developer shell.
    env = {key: value for key, value in os.environ.items() if not key.startswith('STABBUR_')}
    env['RUST_LOG'] = 'warn'
    origin = f'http://127.0.0.1:{free_port()}'
    console = f'http://127.0.0.1:{free_port()}'
    api = Http(origin)
    server_args = [args.server, 'api', '--data-dir', work / 'data', '--bind', origin.removeprefix('http://')]
    try:
        evidence['stage'] = 'start-services'
        server = processes.start('server', server_args, env)
        ready(api, server)
        secret = (work / 'data/bootstrap.secret').read_text().strip()
        api.call('/api/v1/auth/bootstrap', {'secret': secret, 'username': 'live-admin', 'password': PASSWORD}, expected=201)
        auth = api.call('/api/v1/auth/login', {'username': 'live-admin', 'password': PASSWORD})
        api.headers['authorization'] = 'Bearer ' + auth['token']
        caps = json.loads(command([args.server, 'worker', '--print-capabilities', '--autopkg-program', args.autopkg], env=env))
        require('builder.autopkg' in caps['capabilities'], 'real AutoPkg capability detected')
        evidence['autopkg'] = caps['tools']['autopkg']
        require(evidence['autopkg'] == '2.9.0', 'acceptance requires pinned AutoPkg 2.9.0')
        credential = api.call('/api/v1/workers', {'name': 'delivery-worker', 'allowed_capabilities': caps['capabilities']}, expected=201)
        token_file = work / 'worker.json'
        token_file.write_text(json.dumps(credential))
        token_file.chmod(0o600)
        worker_args = [args.server, 'worker', '--server-url', origin, '--token-file', token_file,
                       '--data-dir', work / 'worker', '--autopkg-program', args.autopkg]
        worker = processes.start('worker', worker_args, env)
        frontend_env = {**env, 'STABBUR_FRONTEND_DEVELOPMENT': '1', 'STABBUR_SERVER_ORIGIN': origin,
            'STABBUR_FRONTEND_DATA_DIR': str(work / 'delivery'),
            'STABBUR_FRONTEND_ORIGIN': console, 'STABBUR_FRONTEND_BIND': console.removeprefix('http://')}
        web = processes.start('frontend', [args.frontend], frontend_env)
        gateway = Http(console, {'origin': console, 'x-stabbur-login': '1'})
        def web_ready():
            require(web.poll() is None, 'console exited')
            try:
                return bool(gateway.call('/'))
            except OSError:
                return False
        wait_for(web_ready, 'console readiness', 30)
        session = gateway.call('/api/login', {'username': 'live-admin', 'password': PASSWORD})
        gateway.headers['x-csrf-token'] = session['csrf']
        (work / 'public').mkdir()
        with fileserver(work / 'public') as feed_origin:
            evidence['stage'] = 'catalog-and-build'
            manifest, package, identifier, destination = fixture(work, feed_origin)
            discover_import(api, gateway, manifest, work)
            evidence['recipe_import'] = 'pinned worker scan, disabled manual import, reviewed fixture inputs'
            if args.browser:
                from e2e_browser import start_build
                run_id = start_build(console, PASSWORD, work / 'catalog.json')
                evidence['browser'] = 'catalog review/apply, target trigger, login/logout passed'
            else:
                plan = gateway.call('/api/catalog/plan', {'manifest': manifest})
                require(len(plan['actions']) == 4, 'reviewed plan includes software, recipe, revision, target')
                gateway.call('/api/catalog/apply', {'manifest': manifest, 'plan': plan})
                target = api.call('/api/v1/build-targets/delivery-build')
                gateway.call('/api/operation/update_build_target', {'parameters': {'target': target['id']},
                    'revision': target['revision'], 'body': {'enabled': True}})
                run_id = gateway.call('/api/operation/trigger_build_target', {
                    'parameters': {'target': 'delivery-build'}, 'idempotency_key': 'delivery-first'})['id']
                evidence['browser'] = 'not requested; authenticated BFF exercised'
            def completed():
                value = api.call('/api/v1/runs/' + run_id)
                require(value['state'] not in ('failed', 'cancelled'), 'AutoPkg run failed or cancelled')
                return value if value['state'] == 'succeeded' else None
            completed_run = wait_for(completed, 'real AutoPkg build', 240)
            logs = gateway.call('/api/operation/list_run_logs', {'parameters': {'run': run_id}, 'query': {'limit': '200'}})
            require(bool(logs['items']), 'real build has persisted logs')
            release = api.call('/api/v1/releases/' + completed_run['result']['publication']['release_id'])
            require(release['version'] == '1.0' and release['state'] == 'candidate', 'verified candidate release')
            evidence['stage'] = 'promotion-and-export'
            if args.browser:
                from e2e_browser import promote_release
                promote_release(console, PASSWORD, run_id, release['id'])
                channel = api.call('/api/v1/software/delivery-fixture/channels/stable')
            else:
                channel = gateway.call('/api/operation/promote_channel', {
                    'parameters': {'software': 'delivery-fixture', 'channel': 'stable'},
                    'revision': 0, 'body': {'release_id': release['id'], 'reason': 'CI reviewed fixture'}})
            gateway.call('/api/operation/promote_channel', {
                'parameters': {'software': 'delivery-fixture', 'channel': 'stable'},
                'revision': 0, 'body': {'release_id': release['id']}}, expected=412)
            export = work / 'export'
            cli_env = {**env, 'STABBUR_TOKEN': auth['token']}
            cli_args = [args.cli, '--server', origin, '--profile', work / 'no-profile', '--json',
                        'munki-export', 'delivery-fixture', '--platform', 'mac_os', '--architecture', 'aarch64',
                        '--macos', '15.0', '--extension', 'pkg', '--pkginfo-template', work / 'pkginfo.json']
            exported = json.loads(command(cli_args + ['--output', export], env=cli_env))
            digest = hashlib.sha256(package.read_bytes()).hexdigest()
            require(exported['digest'] == digest, 'AutoPkg delivered exact locally generated package bytes')
            require(hashlib.sha256((export / f'pkgs/{digest}.pkg').read_bytes()).hexdigest() == digest, 'export hash')
            pkginfo = plistlib.loads((export / f'pkgsinfo/{digest}.plist').read_bytes())
            require(pkginfo['supported_architectures'] == ['arm64'], 'server aarch64 maps to Munki arm64')
            evidence['stage'] = 'munki-delivery'
            evidence['munki_version'] = command([args.munki_tools / 'managedsoftwareupdate', '--version']).decode().strip()
            evidence['munki'] = munki_cycle(args, work, export, identifier, destination)
            evidence['delivery'] = {'version': release['version'], 'sha256': digest,
                                    'recipe_commit': RECIPE_COMMIT, 'logs_present': True}

            from delivery_acceptance import exercise_delivery
            evidence['managed_delivery'] = exercise_delivery(
                args, work, api, gateway, console, PASSWORD, release, identifier, destination,
                lambda: fixture(work, feed_origin, '2.0', identifier), feed_origin)
            channel = api.call('/api/v1/software/delivery-fixture/channels/stable')

            # A stopped consistent backup includes DB, CAS and identity state. Restore elsewhere.
            evidence['stage'] = 'backup-restore'
            processes.stop(worker)
            processes.stop(server)
            shutil.copytree(work / 'data', work / 'backup')
            shutil.copytree(work / 'backup', work / 'restored')
            restored_args = [args.server, 'api', '--data-dir', work / 'restored', '--bind', origin.removeprefix('http://')]
            server = processes.start('restored-server', restored_args, env)
            ready(api, server)
            require(api.call('/api/v1/software/delivery-fixture/channels/stable') == channel, 'restored channel and credentials')
            content = api.call(f'/api/v1/artifacts/{digest}/content')
            require(hashlib.sha256(content).hexdigest() == digest, 'restored CAS is readable and unchanged')

            # Claim a fake run without executing it, kill the server, then let a real worker reclaim.
            evidence['stage'] = 'crash-and-lease-recovery'
            recipe = api.call('/api/v1/recipes', {'name': 'recovery-recipe'}, expected=201)
            revision = api.call(f"/api/v1/recipes/{recipe['id']}/revisions", {'builder': 'fake', 'definition': {}, 'required_capabilities': []}, expected=201)
            abandoned = api.call('/api/v1/runs', {'software': 'delivery-fixture', 'recipe_revision': revision['id'], 'parameters': {}},
                headers={'idempotency-key': 'recover-abandoned'}, expected=201)
            worker_http = Http(origin, {'authorization': 'Bearer ' + credential['token']})
            prefix = '/api/v1/internal/workers/' + credential['worker_id']
            claimed = worker_http.call(prefix + '/claim', {'lease_seconds': 30})
            claimed_job = api.call('/api/v1/jobs/' + claimed['job']['id'])
            require(claimed_job['run_id'] == abandoned['id'], 'claimed intended recovery job')
            processes.stop(server, hard=True)
            server = processes.start('restarted-server', restored_args, env)
            ready(api, server)
            worker = processes.start('restarted-worker', worker_args, env)
            def recovered():
                value = api.call('/api/v1/runs/' + abandoned['id'])
                return value if value['state'] == 'succeeded' else None
            wait_for(recovered, 'expired lease reclaimed after crash', 100)
            job = api.call('/api/v1/jobs/' + claimed['job']['id'])
            require(job['attempt_count'] == 2, 'exactly one replacement attempt')
            worker_http.call(prefix + '/heartbeat', {'lease': claimed['lease'], 'lease_seconds': 30}, expected=409)
            evidence['recovery'] = 'consistent backup restore, server SIGKILL, expired lease reclaim, stale lease fencing passed'

            release = api.call('/api/v1/releases/' + release['id'])
            evidence['stage'] = 'withdrawal'
            if args.browser:
                from e2e_browser import withdraw_release
                withdraw_release(console, PASSWORD, release['id'])
                evidence['browser'] = 'catalog review/apply, trigger, logs, promotion, withdrawal, logout/login, expiry passed'
            else:
                gateway.call('/api/operation/withdraw_release', {'parameters': {'release': release['id']},
                    'revision': release['revision'], 'body': {'reason': 'CI withdrawal drill'}})
            require(api.call('/api/v1/releases/' + release['id'])['availability']['kind'] == 'withdrawn', 'release withdrawn')
            api.call('/api/v1/software/delivery-fixture/channels/stable', expected=404)
            failed = subprocess.run([str(arg) for arg in cli_args + ['--output', work / 'withdrawn-export']],
                env=cli_env, capture_output=True, timeout=30)
            require(failed.returncode != 0 and not (work / 'withdrawn-export').exists(), 'withdrawn release cannot be exported')
            require((export / f'pkgs/{digest}.pkg').exists(), 'previous export is explicitly a retained snapshot')
            evidence['withdrawal'] = 'future resolution/export denied; prior delivery snapshot retained'
        gateway.call('/api/logout', {}, expected=204)
        gateway.call('/api/session', expected=401)
        evidence['result'] = 'passed'
        evidence['stage'] = 'complete'
    finally:
        processes.close()


def main():
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', type=Path, default=ROOT / 'target/debug/stabbur-server')
    parser.add_argument('--cli', type=Path, default=ROOT / 'target/review-cli/debug/stabbur')
    parser.add_argument('--frontend', type=Path, default=ROOT / 'target/review-cli/debug/stabbur-frontend')
    parser.add_argument('--autopkg', type=Path, default=Path('/Library/AutoPkg/autopkg'))
    parser.add_argument('--munki-tools', type=Path, default=Path('/usr/local/munki'))
    parser.add_argument('--browser', action='store_true')
    parser.add_argument('--install-fixture', action='store_true')
    parser.add_argument('--evidence', type=Path, required=True)
    args = parser.parse_args()
    require(platform.system() == 'Darwin', 'single-host delivery acceptance requires macOS')
    require(not args.install_fixture or os.environ.get('STABBUR_DISPOSABLE_MACOS') == '1',
            'package installation requires explicit disposable-host mode')
    for name in ('server', 'cli', 'frontend', 'autopkg'):
        path = getattr(args, name).resolve()
        require(path.is_file() and os.access(path, os.X_OK), f'{name} executable required')
        setattr(args, name, path)
    evidence = {'schema_version': 1, 'result': 'failed', 'recorded_at': datetime.now(timezone.utc).isoformat(),
                'host': platform.machine(), 'macos': platform.mac_ver()[0], 'installation_requested': args.install_fixture}
    evidence['sources'] = {name: source_evidence(ROOT.parent / name) for name in (
        'stabbur', 'stabbur-client-rust', 'stabbur-cli', 'stabbur-frontend')}
    evidence['binaries'] = {}
    for name in ('server', 'cli', 'frontend'):
        with getattr(args, name).open('rb') as binary:
            evidence['binaries'][name] = hashlib.file_digest(binary, 'sha256').hexdigest()
    try:
        with tempfile.TemporaryDirectory(prefix='stabbur-single-host-') as temporary:
            run(args, Path(temporary), evidence)
    finally:
        args.evidence.parent.mkdir(parents=True, exist_ok=True)
        # This allowlisted report contains no credentials, database, process logs, or browser trace.
        args.evidence.write_text(json.dumps(evidence, indent=2) + '\n')
    print('PASS: real single-host macOS delivery and recovery acceptance')


if __name__ == '__main__':
    main()
