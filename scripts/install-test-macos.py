#!/usr/bin/env python3
"""Install and manage a private, persistent, single-user macOS test stack.

Python 3.11+; standard library only. See docs/operations/macos-test-setup.md.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
from dataclasses import asdict, dataclass
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import secrets
import shlex
import shutil
import socket
import stat
import subprocess
import sys
import time
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
COMPANIONS = {
    'stabbur-client-rust': 'b6754e85505388b82cd76a053b73b0956549ee5e',
    'stabbur-cli': '36ec320fd4ebd643431c663d3af277dc11060300',
    'stabbur-frontend': '418e09a53956cdd97d4b3f7ec40163520f60dd8a',
}
COMPONENTS = ('server', 'worker', 'frontend')
SAFE_PATH = '/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin'


class SetupError(Exception):
    """An operator-facing error that contains no subprocess output or secrets."""


class InstallerParser(argparse.ArgumentParser):
    """Expose installation options from both the main and install help commands."""

    install_parser = None

    def format_help(self):
        text = super().format_help()
        if self.install_parser is not None:
            text += '\nInstallation options (place after install):\n\n'
            text += self.install_parser.format_help()
        return text


def path_value(value):
    value = os.fspath(value)
    if not value or any(ord(char) < 32 or ord(char) == 127 for char in value):
        raise SetupError('Paths must be nonempty and contain no control characters.')
    return Path(value).expanduser().absolute()


def private_path(path, directory=False):
    """Validate owner, permissions and object kind without following a leaf symlink."""
    metadata = path.lstat()
    kind = stat.S_ISDIR if directory else stat.S_ISREG
    if not kind(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
        raise SetupError('Installation paths and credentials must be real, owner-only objects.')
    return path


def safe_ancestors(path):
    # Resolve macOS aliases such as /tmp first, but reject untrusted writable parents.
    for ancestor in (path, *path.parents):
        if not ancestor.exists():
            continue
        metadata = ancestor.stat()
        sticky_root = metadata.st_uid == 0 and metadata.st_mode & stat.S_ISVTX
        if (not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid not in (0, os.getuid())
                or (metadata.st_mode & 0o022 and not sticky_root)):
            raise SetupError('An installation directory has an untrusted or writable parent.')


def new_directory(value):
    raw = path_value(value)
    if raw.exists() or raw.is_symlink():
        raise SetupError('Installation directories must be new; existing paths are never replaced.')
    resolved = raw.resolve()
    safe_ancestors(resolved.parent)
    return resolved


def distinct_directories(prefix, directories):
    for path in directories:
        if path == prefix or path in prefix.parents:
            raise SetupError('Data, log and build directories must not contain the installation prefix.')
    for index, left in enumerate(directories):
        for right in directories[index + 1:]:
            if left == right or left in right.parents or right in left.parents:
                raise SetupError('Data, log and build directories must not overlap.')


def port_value(value):
    if isinstance(value, bool) or not re.fullmatch(r'[0-9]{4,5}', str(value)):
        raise SetupError('Ports must be integers between 1024 and 65535.')
    value = int(value)
    if not 1024 <= value <= 65535:
        raise SetupError('Ports must be integers between 1024 and 65535.')
    return value


def reserve_ports(ports):
    """Check all listeners together; never stop whatever already owns a port."""
    sockets = []
    try:
        for port in ports:
            listener = socket.socket()
            sockets.append(listener)
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(('127.0.0.1', port))
    except OSError as error:
        reason = 'occupied' if error.errno == errno.EADDRINUSE else 'unavailable'
        raise SetupError(
            f'Loopback port {port} is {reason}. For a new installation, use '
            'install --api-port PORT --web-port PORT to choose two different ports '
            'between 1024 and 65535.') from error
    finally:
        for listener in sockets:
            listener.close()


def write_new(path, data, mode=0o600):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    with os.fdopen(descriptor, 'wb') as stream:
        stream.write(data.encode() if isinstance(data, str) else data)
        stream.flush()
        os.fsync(stream.fileno())


def read_json(path):
    private_path(path)
    if path.stat().st_size > 65536:
        raise SetupError('An installation control file exceeds its size limit.')
    return json.loads(path.read_text())


@dataclass(frozen=True)
class Installation:
    prefix: Path
    server_data: Path
    worker_data: Path
    logs: Path
    api_port: int
    web_port: int
    username: str
    autopkg: str | None

    @classmethod
    def load(cls, prefix):
        prefix = private_path(path_value(prefix), directory=True).resolve()
        value = read_json(prefix / 'installation.json')
        if not isinstance(value, dict) or type(value.get('schema_version')) is not int or value.pop('schema_version') != 1:
            raise SetupError('Unsupported installation configuration.')
        if set(value) != set(cls.__dataclass_fields__):
            raise SetupError('Unexpected installation configuration fields.')
        for field in ('prefix', 'server_data', 'worker_data', 'logs'):
            path = path_value(value[field])
            safe_ancestors(path.resolve().parent)
            value[field] = private_path(path, directory=True).resolve()
        if value['prefix'] != prefix:
            raise SetupError('This installation was moved; its launchd paths would be stale.')
        distinct_directories(prefix, [value[key] for key in ('server_data', 'worker_data', 'logs')])
        value['api_port'] = port_value(value['api_port'])
        value['web_port'] = port_value(value['web_port'])
        if value['api_port'] == value['web_port']:
            raise SetupError('API and frontend ports must differ.')
        validate_username(value['username'])
        if value['autopkg'] is not None:
            if not Path(value['autopkg']).is_absolute():
                raise SetupError('The saved AutoPkg executable must be an absolute path.')
            value['autopkg'] = str(path_value(value['autopkg']))
        for name in ('bin', 'libexec', 'launchd', 'secrets', 'home', 'tmp'):
            private_path(prefix / name, directory=True)
        return cls(**value)

    @property
    def api(self):
        return f'http://127.0.0.1:{self.api_port}'

    @property
    def web(self):
        return f'http://127.0.0.1:{self.web_port}'

    @property
    def domain(self):
        return f'gui/{os.getuid()}'

    def label(self, component):
        assert component in COMPONENTS
        identity = hashlib.sha256(os.fsencode(self.prefix)).hexdigest()[:24]
        return f'no.stabbur.test.{identity}.{component}'

    def environment(self):
        return {'PATH': SAFE_PATH, 'HOME': str(self.prefix / 'home'),
                'TMPDIR': str(self.prefix / 'tmp') + '/', 'RUST_LOG': 'warn'}

    def cli(self, *arguments):
        return [self.prefix / 'libexec/stabbur', '--server', self.api,
                '--profile', self.prefix / 'secrets/profile.json', *arguments]

    def service(self, component):
        binary = self.prefix / 'libexec/stabbur-server'
        env = self.environment()
        if component == 'server':
            args = [binary, 'api', '--data-dir', self.server_data,
                    '--bind', f'127.0.0.1:{self.api_port}']
        elif component == 'worker':
            args = [binary, 'worker', '--server-url', self.api, '--data-dir', self.worker_data,
                    '--token-file', self.prefix / 'secrets/worker.json']
            if self.autopkg:
                args += ['--autopkg-program', self.autopkg]
        else:
            assert component == 'frontend'
            args = [self.prefix / 'libexec/stabbur-frontend']
            env.update(STABBUR_FRONTEND_DEVELOPMENT='1', STABBUR_SERVER_ORIGIN=self.api,
                       STABBUR_FRONTEND_ORIGIN=self.web,
                       STABBUR_FRONTEND_BIND=f'127.0.0.1:{self.web_port}')
        # env -i also clears launchd's ambient application, proxy and loader settings.
        return ['/usr/bin/env', '-i', *[f'{key}={value}' for key, value in env.items()],
                *map(str, args)]


def validate_username(value):
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,63}', value):
        raise SetupError('Administrator names must be 1–64 ASCII letters, digits, dots, dashes or underscores.')


def executable(value):
    path = path_value(value).resolve(strict=True)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise SetupError('A requested executable is unavailable.')
    return path


def command(args, *, env=None, label='Command', timeout=60, log=None):
    # Do not echo command lines or captured authentication output on errors.
    result = subprocess.run(list(map(str, args)), env=env, stdin=subprocess.DEVNULL,
                            stdout=log or subprocess.PIPE, stderr=log or subprocess.PIPE,
                            timeout=timeout, check=False)
    if result.returncode:
        raise SetupError(f'{label} failed (exit {result.returncode}).')
    return result.stdout


def launchctl(config, action, component=None, check=True):
    target = config.domain + '/' + config.label(component) if component else config.domain
    args = ['/bin/launchctl', action, target]
    if action == 'bootstrap':
        args = ['/bin/launchctl', action, config.domain, config.prefix / 'launchd' / f'{component}.plist']
    result = subprocess.run(list(map(str, args)), capture_output=True, timeout=30, check=False)
    if check and result.returncode:
        raise SetupError(f'launchctl {action} failed; use this setup from your macOS user session.')
    return result


def service_state(config, component):
    result = launchctl(config, 'print', component, check=False)
    if result.returncode:
        return 'stopped'
    return 'running' if re.search(rb'\n\s*state = running\s*\n', result.stdout) else 'waiting'


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def reachable(origin, route):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        with opener.open(origin + route, timeout=2) as response:
            return response.status == 200
    except (OSError, urllib.error.URLError):
        return False


def wait_for(predicate, label, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.25)
    raise SetupError(f'{label} did not become ready; inspect the private service logs.')


def start_component(config, component):
    if service_state(config, component) == 'stopped':
        private_path(config.prefix / 'launchd' / f'{component}.plist')
        launchctl(config, 'bootstrap', component)
    route = (config.api, '/readyz') if component == 'server' else (config.web, '/')
    wait_for(lambda: service_state(config, component) == 'running'
             and (component == 'worker' or reachable(*route)), component)


def start(config):
    private_path(config.prefix / 'installed')
    launchctl(config, 'print')  # Distinguish an unavailable user domain from absent services.
    absent_ports = []
    for component, port in [('server', config.api_port), ('frontend', config.web_port)]:
        if service_state(config, component) == 'stopped':
            absent_ports.append(port)
    reserve_ports(absent_ports)
    private_path(config.prefix / 'secrets/worker.json')
    if config.autopkg:
        executable(config.autopkg)
    for component in COMPONENTS:
        start_component(config, component)


def stop(config):
    launchctl(config, 'print')
    ports = []
    for component in reversed(COMPONENTS):
        if service_state(config, component) != 'stopped':
            if component in ('server', 'frontend'):
                ports.append(config.api_port if component == 'server' else config.web_port)
            launchctl(config, 'bootout', component)

    # bootout is asynchronous. A still-unloading service must disappear before its label
    # can safely be bootstrapped again; otherwise launchd can remove the replacement job.
    wait_for(lambda: all(service_state(config, component) == 'stopped' for component in COMPONENTS),
             'launchd service removal', timeout=30)

    def released():
        try:
            reserve_ports(ports)
            return True
        except SetupError:
            return False

    wait_for(released, 'Service shutdown', timeout=30)


@contextmanager
def locked(prefix):
    path = prefix / '.control.lock'
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'r+') as stream:
        private_path(path)
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise SetupError('Another installation or lifecycle command is in progress.') from error
        yield


def build_binaries(args, sources, build, logs):
    supplied = [args.server_binary, args.cli_binary, args.frontend_binary]
    if all(supplied):
        return dict(zip(('stabbur-server', 'stabbur', 'stabbur-frontend'), map(executable, supplied))), {}
    if not shutil.which('cargo') or not shutil.which('git'):
        raise SetupError('Install Rust 1.88+ and Xcode Command Line Tools, or supply all three binaries.')
    env = {key: value for key, value in os.environ.items() if not key.startswith('STABBUR_')}
    env.update(CARGO_TARGET_DIR=str(build), CARGO_INCREMENTAL='0', CARGO_PROFILE_DEV_DEBUG='0',
               GIT_TERMINAL_PROMPT='0')
    with (logs / 'build.log').open('xb') as log:
        if args.workspace:
            workspace = path_value(args.workspace).resolve(strict=True)
            checkouts = {name: workspace / name for name in ('stabbur', *COMPANIONS)}
        else:
            checkouts = {'stabbur': ROOT}
            for name, revision in COMPANIONS.items():
                destination = sources / name
                print(f'Fetching pinned {name} sources…', flush=True)
                command(['git', 'init', destination], env=env, log=log, label='Source initialization')
                command(['git', '-C', destination, 'fetch', '--depth=1',
                         f'https://github.com/terjekv/{name}.git', revision],
                        env=env, log=log, label='Pinned source fetch', timeout=300)
                command(['git', '-C', destination, 'checkout', '--detach', 'FETCH_HEAD'],
                        env=env, log=log, label='Source checkout')
                actual = command(['git', '-C', destination, 'rev-parse', 'HEAD'], env=env).decode().strip()
                if actual != revision:
                    raise SetupError('Fetched source revision does not match its pin.')
                checkouts[name] = destination
        contracts = [checkouts['stabbur'] / 'docs/openapi.json',
                     checkouts['stabbur-client-rust'] / 'openapi/openapi.json',
                     checkouts['stabbur-frontend'] / 'contract/openapi.json']
        if not all(json.loads(path.read_text()) == json.loads(contracts[0].read_text()) for path in contracts[1:]):
            raise SetupError('Server, client and frontend API contracts differ.')
        evidence = {}
        for name, checkout in checkouts.items():
            evidence[name] = {
                'commit': command(['git', '-C', checkout, 'rev-parse', 'HEAD'], env=env).decode().strip(),
                'dirty': bool(command(['git', '-C', checkout, 'status', '--porcelain'], env=env)),
            }
        for name in ('stabbur', 'stabbur-cli', 'stabbur-frontend'):
            print(f'Building {name} ({args.build_profile}); compiler output is in the private build log…', flush=True)
            flags = ['--release'] if args.build_profile == 'release' else []
            command(['cargo', 'build', '--locked', '--manifest-path', checkouts[name] / 'Cargo.toml', *flags],
                    env=env, log=log, label=f'{name} build', timeout=3600)
    return {name: build / args.build_profile / name
            for name in ('stabbur-server', 'stabbur', 'stabbur-frontend')}, evidence


def prepare_autopkg(args, prefix, server, env):
    if args.autopkg_program:
        return str(executable(args.autopkg_program))
    standard = Path('/Library/AutoPkg/autopkg')
    if not args.install_autopkg:
        return str(executable(standard)) if standard.exists() else None
    # This is the only privileged option. Do not overwrite an existing installation.
    receipt = subprocess.run(['/usr/sbin/pkgutil', '--pkg-info', 'com.github.autopkg.autopkg'],
                             capture_output=True, timeout=30, check=False)
    if standard.parent.exists() or receipt.returncode == 0:
        raise SetupError('AutoPkg is already present; omit --install-autopkg and select the existing tool.')
    fixture = json.loads((ROOT / 'tests/fixtures/autopkg-prepare/autopkg-2.9.0.json').read_text())
    package = prefix / 'autopkg.pkg'
    print('Downloading the pinned AutoPkg 2.9.0 package; its system installation requires sudo…', flush=True)
    command(['/usr/bin/curl', '--fail', '--silent', '--show-error', '--location',
             '--proto', '=https', '--proto-redir', '=https', '--max-time', '300',
             '--max-filesize', str(fixture['release']['size']), '--output', package,
             fixture['release']['url']], env=env, label='AutoPkg download', timeout=310)
    if package.stat().st_size != fixture['release']['size'] or digest(package) != fixture['release']['sha256']:
        raise SetupError('AutoPkg package bytes do not match the pinned size and SHA-256.')
    manifest = fixture['manifest']
    manifest['package']['path'] = str(package)
    manifest_path = prefix / 'autopkg-manifest.json'
    write_new(manifest_path, json.dumps(manifest))
    prepare = [server, 'worker', 'prepare', '--manifest', manifest_path,
               '--receipt', prefix / 'autopkg-prepared.json']
    command([*prepare, '--check'], env=env, label='AutoPkg package inspection', timeout=180)
    # sudo's own terminal prompt is visible; package/process output remains private.
    result = subprocess.run(['/usr/bin/sudo', '/usr/bin/env', '-i', f'PATH={SAFE_PATH}',
                             *map(str, prepare)], stdout=subprocess.DEVNULL, timeout=1800, check=False)
    if result.returncode:
        raise SetupError('Privileged AutoPkg preparation failed; inspect its installation before retrying.')
    package.unlink()
    return str(standard)


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def install(args):
    if any((args.server_binary, args.cli_binary, args.frontend_binary)) and not all(
            (args.server_binary, args.cli_binary, args.frontend_binary)):
        raise SetupError('Supply --server-binary, --cli-binary and --frontend-binary together.')
    if args.workspace and args.server_binary:
        raise SetupError('--workspace cannot be combined with prebuilt binaries.')
    validate_username(args.username)
    api_port, web_port = port_value(args.api_port), port_value(args.web_port)
    if api_port == web_port:
        raise SetupError('API and frontend ports must differ.')
    reserve_ports([api_port, web_port])
    command(['/bin/launchctl', 'print', f'gui/{os.getuid()}'], label='Logged-in macOS launchd domain check')
    prefix = new_directory(args.prefix)
    directories = {name: new_directory(value or prefix / default) for name, value, default in (
        ('server_data', args.server_data, 'server'), ('worker_data', args.worker_data, 'worker'),
        ('logs', args.logs_dir, 'logs'), ('sources', args.sources_dir, 'sources'),
        ('build', args.build_dir, 'build'))}
    reserved = [prefix / name for name in ('bin', 'libexec', 'launchd', 'secrets', 'home', 'tmp')]
    control_files = [prefix / name for name in ('.control.lock', 'installation.json', 'provenance.json',
                     'installed', 'autopkg.pkg', 'autopkg-manifest.json', 'autopkg-prepared.json')]
    distinct_directories(prefix, [*directories.values(), *reserved, *control_files])
    password = None
    if args.password_file:
        password_path = private_path(path_value(args.password_file))
        if password_path.stat().st_size > 1024:
            raise SetupError('The password file is too large.')
        password = password_path.read_text().rstrip('\r\n')
        if not password or '\n' in password or '\r' in password:
            raise SetupError('The password file must contain one nonempty line.')
    prefix.mkdir(mode=0o700, parents=True)
    with locked(prefix):
        for path in [*directories.values(), *reserved]:
            path.mkdir(mode=0o700, parents=True)
        print('Preparing a private macOS test installation…', flush=True)
        binaries, sources = build_binaries(args, directories['sources'], directories['build'], directories['logs'])
        for name, source in binaries.items():
            source = executable(source)
            destination = prefix / 'libexec' / name
            with source.open('rb') as reader, destination.open('xb') as writer:
                shutil.copyfileobj(reader, writer)
            destination.chmod(0o700)
        env = {'PATH': SAFE_PATH, 'HOME': str(prefix / 'home'), 'TMPDIR': str(prefix / 'tmp') + '/', 'RUST_LOG': 'warn'}
        server = prefix / 'libexec/stabbur-server'
        autopkg = prepare_autopkg(args, prefix, server, env)
        config = Installation(prefix, directories['server_data'], directories['worker_data'],
                              directories['logs'], api_port, web_port, args.username, autopkg)
        capability_args = [server, 'worker', '--print-capabilities']
        if autopkg:
            capability_args += ['--autopkg-program', autopkg]
        capabilities = json.loads(command(capability_args, env=env, label='Worker capability detection'))
        if autopkg and 'builder.autopkg' not in capabilities['capabilities']:
            raise SetupError('The selected AutoPkg executable did not pass capability detection.')
        write_new(prefix / 'installation.json', json.dumps({'schema_version': 1, **asdict(config)}, default=str, indent=2) + '\n')
        write_new(prefix / 'provenance.json', json.dumps({'sources': sources, 'binaries': {
            name: digest(prefix / 'libexec' / name) for name in binaries}}, indent=2) + '\n')
        write_new(prefix / 'libexec/install-test-macos.py', Path(__file__).read_bytes())
        manager = [sys.executable, prefix / 'libexec/install-test-macos.py', '--prefix', prefix]
        write_new(prefix / 'bin/stabbur-test', '#!/bin/sh\nexec ' + shlex.join(list(map(str, manager))) + ' "$@"\n', 0o700)
        write_new(prefix / 'bin/stabbur', '#!/bin/sh\nexec ' + shlex.join(
                  ['/usr/bin/env', '-i', *[f'{key}={value}' for key, value in env.items()],
                   *map(str, config.cli())]) + ' "$@"\n', 0o700)
        for component in COMPONENTS:
            log_path = directories['logs'] / f'{component}.log'
            write_new(log_path, '')
            plist = {'Label': config.label(component), 'ProgramArguments': config.service(component),
                     'WorkingDirectory': str(prefix / 'home'), 'RunAtLoad': True, 'KeepAlive': True,
                     'ThrottleInterval': 10, 'Umask': 0o077, 'ExitTimeOut': 20,
                     'StandardOutPath': str(log_path), 'StandardErrorPath': str(log_path)}
            write_new(prefix / 'launchd' / f'{component}.plist', plistlib.dumps(plist))
        initial_password = prefix / 'secrets/admin-password'
        write_new(initial_password, (password if password is not None else secrets.token_urlsafe(32)) + '\n')
        try:
            command([server, 'admin', 'bootstrap', '--data-dir', config.server_data,
                     '--username', config.username, '--password-file', initial_password],
                    env=env, label='Local administrator bootstrap')
            start_component(config, 'server')
            command(config.cli('auth', 'login', '--username', config.username, '--password-file', initial_password),
                    env=env, label='CLI login')
            provision = config.cli('worker', 'provision', '--name', 'local-test-worker',
                                   '--output-token-file', prefix / 'secrets/worker.json')
            for capability in capabilities['capabilities']:
                provision += ['--capability', capability]
            command(provision, env=env, label='Worker provisioning')
            start_component(config, 'worker')
            worker_id = read_json(prefix / 'secrets/worker.json')['worker_id']

            def registered():
                record = json.loads(command(config.cli('--json', 'worker', 'show', worker_id), env=env,
                                            label='Worker registration check'))
                return record.get('last_seen_at') is not None

            wait_for(registered, 'Authenticated worker registration')
            start_component(config, 'frontend')
            write_new(prefix / 'installed', '1\n')
        except BaseException:
            stop(config)
            raise
        finally:
            if args.password_file:
                initial_password.unlink(missing_ok=True)
        print(f'Installed. Management UI: {config.web}\nAPI: {config.api}\nAdministrator: {config.username}')
        print('Open the exact Management UI address above; localhost and 127.0.0.1 are different login origins.')
        if args.password_file:
            print('Administrator password: supplied password file (temporary copy removed).')
        else:
            print(f'Initial password file (owner-only): {initial_password}')
        print(f'CLI: {shlex.quote(str(prefix / "bin/stabbur"))} status')
        print(f'Manage: {shlex.quote(str(prefix / "bin/stabbur-test"))} status|start|stop|restart')
        print('AutoPkg: ' + ('available' if autopkg else 'not installed; fake builder is available for test jobs'))


def parser():
    result = InstallerParser(
        description=__doc__,
        epilog='Custom ports: %(prog)s --prefix /tmp/stabbur install '
               '--api-port 18080 --web-port 13000')
    result.add_argument('--prefix', default='~/Library/Application Support/Stabbur Test',
                        help='new private installation directory (also used by lifecycle commands)')
    commands = result.add_subparsers(dest='action', required=True)
    setup = commands.add_parser('install', help='create and start a new test installation; never overwrite one')
    result.install_parser = setup
    for flag, help_text in (
        ('server-data', 'new SQLite and immutable artifact directory'),
        ('worker-data', 'new worker state directory'), ('logs-dir', 'new private logs directory'),
        ('sources-dir', 'new directory for pinned companion source checkouts'),
        ('build-dir', 'new Cargo build directory'),
        ('workspace', 'use existing sibling stabbur, stabbur-client-rust, stabbur-cli and stabbur-frontend checkouts'),
        ('server-binary', 'use an existing server/worker binary (requires both client binaries)'),
        ('cli-binary', 'use an existing CLI binary'), ('frontend-binary', 'use an existing frontend binary'),
        ('password-file', 'read an owner-only administrator password file; otherwise generate a private password')):
        setup.add_argument('--' + flag, help=help_text)
    setup.add_argument('--username', default='admin')
    setup.add_argument('--api-port', metavar='PORT', default=8080, help='loopback API port, 1024–65535 (default: 8080)')
    setup.add_argument('--web-port', metavar='PORT', default=3000, help='loopback frontend port, 1024–65535 (default: 3000)')
    setup.add_argument('--build-profile', choices=('debug', 'release'), default='debug')
    autopkg = setup.add_mutually_exclusive_group()
    autopkg.add_argument('--autopkg-program', help='existing absolute AutoPkg executable (default: detect /Library/AutoPkg/autopkg)')
    autopkg.add_argument('--install-autopkg', action='store_true',
                         help='explicitly install pinned AutoPkg 2.9.0 system-wide using sudo; refuses existing AutoPkg')
    for name in ('start', 'stop', 'restart', 'status'):
        commands.add_parser(name)
    return result


def main():
    os.umask(0o077)
    args = parser().parse_args()
    if sys.platform != 'darwin' or sys.version_info < (3, 11) or os.getuid() == 0:
        raise SetupError('Run as a normal macOS user with Python 3.11+; do not use sudo for this script.')
    if args.action == 'install':
        install(args)
        return
    config = Installation.load(args.prefix)
    with locked(config.prefix):
        if args.action in ('stop', 'restart'):
            stop(config)
        if args.action in ('start', 'restart'):
            start(config)
        if args.action == 'status':
            launchctl(config, 'print')
            states = {component: service_state(config, component) for component in COMPONENTS}
            health = {'api': reachable(config.api, '/readyz'), 'frontend': reachable(config.web, '/')}
            print(json.dumps({'services': states, 'ready': health, 'api': config.api, 'frontend': config.web}, indent=2))
            if not all(state == 'running' for state in states.values()) or not all(health.values()):
                sys.exit(1)
        else:
            print(f'Test setup {args.action} completed.')


if __name__ == '__main__':
    try:
        main()
    except (SetupError, OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired) as failure:
        # Python tracebacks, subprocess output and config values may contain sensitive data.
        message = str(failure) if isinstance(failure, SetupError) else 'An installation operation failed; check paths, prerequisites and private logs.'
        print(f'Error: {message}', file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print('Interrupted; private installation files have been retained.', file=sys.stderr)
        sys.exit(130)
