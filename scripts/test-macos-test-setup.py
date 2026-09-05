#!/usr/bin/env python3
"""Portable installer safety checks, plus opt-in real macOS launchd acceptance."""

import argparse
from dataclasses import asdict
import http.cookiejar
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
import urllib.request


SCRIPT = Path(__file__).with_name('install-test-macos.py').resolve()
spec = importlib.util.spec_from_file_location('test_setup_installer', SCRIPT)
installer = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = installer
spec.loader.exec_module(installer)


class SafetyTests(unittest.TestCase):
    def test_existing_paths_and_symlinks_are_never_replaced(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            target = base / 'existing'
            target.mkdir(mode=0o755)
            sentinel = target / 'keep'
            sentinel.write_text('original')
            alias = base / 'alias'
            alias.symlink_to(target, target_is_directory=True)
            dangling = base / 'dangling'
            dangling.symlink_to(base / 'missing')
            for path in (target, alias, dangling):
                with self.subTest(path=path.name), self.assertRaises(installer.SetupError):
                    installer.new_directory(path)
            self.assertEqual(sentinel.read_text(), 'original')
            self.assertEqual(target.stat().st_mode & 0o777, 0o755)

    def test_private_files_reject_symlinks_and_shared_permissions(self):
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / 'password'
            installer.write_new(target, 'not-a-real-password')
            installer.private_path(target)
            with self.assertRaises(FileExistsError):
                installer.write_new(target, 'replacement')
            alias = Path(temporary) / 'alias'
            alias.symlink_to(target)
            with self.assertRaises(installer.SetupError):
                installer.private_path(alias)
            target.chmod(0o640)
            with self.assertRaises(installer.SetupError):
                installer.private_path(target)

    def test_overlaps_and_untrusted_ancestors_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            for paths in ([base], [base.parent], [base / 'data', base / 'data/jobs']):
                with self.subTest(paths=paths), self.assertRaises(installer.SetupError):
                    installer.distinct_directories(base, paths)
            parent = base / 'shared'
            parent.mkdir()
            parent.chmod(0o777)
            with self.assertRaises(installer.SetupError):
                installer.new_directory(parent / 'child')

    def test_ports_are_bounded_and_occupied_listeners_are_untouched(self):
        for value in (0, 80, 65536, True, '1e4', '-3000', '3000.0', ' 3000'):
            with self.subTest(value=value), self.assertRaises(installer.SetupError):
                installer.port_value(value)
        self.assertEqual(installer.port_value('3000'), 3000)
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            port = listener.getsockname()[1]
            with self.assertRaises(installer.SetupError):
                installer.reserve_ports([port])
            with socket.create_connection(('127.0.0.1', port), timeout=2):
                pass

    def test_saved_configuration_validates_before_service_operations(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            for name in ('server', 'worker', 'logs', 'bin', 'libexec', 'launchd', 'secrets', 'home', 'tmp'):
                (base / name).mkdir(mode=0o700)
            config = installer.Installation(base, base / 'server', base / 'worker', base / 'logs',
                                            8080, 3000, 'admin', None)
            value = {'schema_version': 1, **asdict(config)}
            path = base / 'installation.json'
            installer.write_new(path, json.dumps(value, default=str))
            loaded = installer.Installation.load(base)
            self.assertEqual(loaded, config)
            for key, replacement in [('schema_version', 2), ('schema_version', True), ('api_port', True),
                                     ('web_port', 8080), ('username', 'a\nb'), ('prefix', str(base / 'worker'))]:
                path.write_text(json.dumps({**value, key: replacement}, default=str))
                with self.subTest(key=key), self.assertRaises(installer.SetupError):
                    installer.Installation.load(base)
            env = loaded.environment()
            self.assertEqual(set(env), {'HOME', 'PATH', 'TMPDIR', 'RUST_LOG'})
            for component in installer.COMPONENTS:
                args = loaded.service(component)
                self.assertEqual(args[:2], ['/usr/bin/env', '-i'])
                self.assertNotIn('0.0.0.0', ' '.join(args))


def free_ports():
    with socket.socket() as first, socket.socket() as second:
        first.bind(('127.0.0.1', 0))
        second.bind(('127.0.0.1', 0))
        return first.getsockname()[1], second.getsockname()[1]


def invoke(args, env=None, success=True):
    result = subprocess.run(list(map(str, args)), env=env, capture_output=True, timeout=180, check=False)
    if (result.returncode == 0) != success:
        # Never include captured output, credentials, cookies or child tracebacks.
        if (str(SCRIPT) in list(map(str, args)) or Path(args[0]).name == 'stabbur-test') and result.stderr.startswith(b'Error: '):
            raise AssertionError(result.stderr.decode().strip())
        raise AssertionError('Installer acceptance command returned an unexpected exit status.')
    return result.stdout


def live(args):
    if sys.platform != 'darwin':
        raise AssertionError('Live acceptance requires macOS.')
    with tempfile.TemporaryDirectory(prefix='stabbur-installer-') as temporary:
        base = Path(temporary).resolve()
        prefix = base / "setup with spaces '$(touch DO-NOT-CREATE)' &"
        api_port, web_port = free_ports()
        env = {**os.environ, 'STABBUR_DATABASE_URL': str(base / 'do-not-open.db'),
               'STABBUR_DATA_DIR': str(base / 'do-not-use'),
               'STABBUR_SERVER_URL': 'http://127.0.0.1:1', 'STABBUR_TOKEN': 'do-not-use',
               'STABBUR_FRONTEND_BIND': '0.0.0.0:1', 'RUST_LOG': 'trace',
               'HTTP_PROXY': 'http://127.0.0.1:1', 'HTTPS_PROXY': 'http://127.0.0.1:1'}
        setup = [sys.executable, SCRIPT, '--prefix', prefix, 'install', '--api-port', api_port,
                 '--web-port', web_port, '--username', 'live-admin',
                 '--server-binary', args.server, '--cli-binary', args.cli, '--frontend-binary', args.frontend,
                 '--server-data', base / 'external server', '--worker-data', base / 'external worker',
                 '--logs-dir', base / 'external logs']
        if args.autopkg:
            setup += ['--autopkg-program', args.autopkg]
        manager = prefix / 'bin/stabbur-test'
        try:
            output = invoke(setup, env)
            password = (prefix / 'secrets/admin-password').read_text().strip()
            assert password.encode() not in output
            credential = installer.read_json(prefix / 'secrets/worker.json')
            assert credential['token'].encode() not in output
            invoke([manager, 'status'], env)
            invoke(setup, env, success=False)
            invoke([manager, 'start'], env)  # Idempotent start, no duplicate processes.
            cli = prefix / 'bin/stabbur'
            worker = json.loads(invoke([cli, '--json', 'worker', 'show', credential['worker_id']], env))
            assert worker['last_seen_at'] is not None
            if args.autopkg:
                assert 'builder.autopkg' in worker['advertised_capabilities']
            # Real browser authentication protocol: credentials stay in the frontend backend.
            jar = http.cookiejar.CookieJar()
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                                                urllib.request.HTTPCookieProcessor(jar), installer.NoRedirect())
            origin = f'http://127.0.0.1:{web_port}'
            request = urllib.request.Request(origin + '/api/login',
                data=json.dumps({'username': 'live-admin', 'password': password}).encode(),
                headers={'Content-Type': 'application/json', 'Origin': origin, 'x-stabbur-login': '1'})
            with opener.open(request, timeout=15) as response:
                session = json.load(response)
                assert response.status == 200 and 'csrf' in session and 'token' not in session
            with opener.open(origin + '/api/session', timeout=15) as response:
                assert response.status == 200
            # Exercise a durable write and confirm it survives a full service restart.
            invoke([cli, 'software', 'create', '--slug', 'installer-proof', '--name', 'Installer Proof'], env)
            invoke([manager, 'restart'], env)
            after = json.loads(invoke([cli, '--json', 'software', 'list'], env))
            assert any(item['slug'] == 'installer-proof' for item in after['items'])
            invoke([manager, 'stop'], env)
            invoke([manager, 'stop'], env)
            invoke([manager, 'status'], env, success=False)
            # Port collisions must leave the other process alive and our stack stopped.
            with socket.socket() as listener:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind(('127.0.0.1', api_port))
                listener.listen()
                invoke([manager, 'start'], env, success=False)
                with socket.create_connection(('127.0.0.1', api_port), timeout=2):
                    pass
            invoke([manager, 'start'], env)
            invoke([manager, 'status'], env)
            for path in (prefix / 'secrets').iterdir():
                installer.private_path(path)
            for path in (base / 'external logs').iterdir():
                contents = path.read_bytes()
                assert password.encode() not in contents and credential['token'].encode() not in contents
            assert not (base / 'do-not-open.db').exists()
            assert not (base / 'do-not-use').exists()
            assert not (prefix / 'home/DO-NOT-CREATE').exists()
        finally:
            if manager.exists():
                invoke([manager, 'stop'])
        # Supplied credentials are consumed privately; a rejected bootstrap leaves no jobs.
        for acceptable in (False, True):
            supplied_prefix = base / ('supplied-password' if acceptable else 'rejected-password')
            password_file = base / ('good-password' if acceptable else 'bad-password')
            value = installer.secrets.token_urlsafe(32) if acceptable else 'short'
            installer.write_new(password_file, value + '\n')
            api_port, web_port = free_ports()
            supplied_manager = supplied_prefix / 'bin/stabbur-test'
            try:
                output = invoke([sys.executable, SCRIPT, '--prefix', supplied_prefix, 'install',
                                 '--api-port', api_port, '--web-port', web_port,
                                 '--password-file', password_file,
                                 '--server-binary', args.server, '--cli-binary', args.cli,
                                 '--frontend-binary', args.frontend], env, success=acceptable)
                assert value.encode() not in output
                assert password_file.read_text() == value + '\n'
                assert not (supplied_prefix / 'secrets/admin-password').exists()
                config = installer.Installation.load(supplied_prefix)
                if acceptable:
                    invoke([supplied_manager, 'status'], env)
                else:
                    assert not (supplied_prefix / 'installed').exists()
                    assert all(installer.service_state(config, component) == 'stopped'
                               for component in installer.COMPONENTS)
            finally:
                if supplied_manager.exists():
                    invoke([supplied_manager, 'stop'])
        print('Real macOS installer acceptance passed: custom paths, private credentials, CLI and frontend login,')
        print('worker registration, durable state across restart, idempotent lifecycle, occupied-port rejection,')
        print('supplied password cleanup and failed-bootstrap rollback.')


if __name__ == '__main__':
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument('--live', action='store_true')
    options.add_argument('--server', type=Path, default=Path('target/debug/stabbur-server').resolve())
    options.add_argument('--cli', type=Path, default=Path('target/debug/stabbur').resolve())
    options.add_argument('--frontend', type=Path, default=Path('target/debug/stabbur-frontend').resolve())
    options.add_argument('--autopkg', type=Path)
    arguments = options.parse_args()
    if arguments.live:
        live(arguments)
    else:
        unittest.main(argv=[sys.argv[0]])
