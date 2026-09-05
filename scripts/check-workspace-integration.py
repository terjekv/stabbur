#!/usr/bin/env python3
"""Local cross-repository contract and authentication integration; never release-image evidence."""
import argparse
from http.cookiejar import CookieJar
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]

def port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]

def http(origin, path, data=None, headers=None, opener=None):
    request = urllib.request.Request(origin + path, data=None if data is None else json.dumps(data).encode(), headers={'content-type': 'application/json', **(headers or {})})
    try:
        with (opener or urllib.request.build_opener(urllib.request.ProxyHandler({}))).open(request, timeout=15) as response:
            body = response.read()
            return response.status, response.headers, json.loads(body) if body and 'json' in response.headers.get('content-type', '') else body
    except urllib.error.HTTPError as error:
        return error.code, error.headers, error.read()

def ready(origin, path, process):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError('fixture process exited before readiness')
        try:
            if http(origin, path)[0] == 200:
                return
        except OSError:
            pass
        time.sleep(.1)
    raise RuntimeError('fixture readiness deadline exceeded')

def require(actual, expected, label):
    if actual != expected:
        raise AssertionError(f'{label}: expected {expected}, received {actual}')

def run(args):
    client = args.client.resolve()
    cli = args.cli.resolve()
    frontend = args.frontend.resolve()
    target = args.target.resolve()
    server_binary = ROOT / 'target/debug/stabbur-server'
    cli_binary = target / 'debug/stabbur'
    console_binary = target / 'debug/stabbur-frontend'
    env = {**os.environ, 'CARGO_TARGET_DIR': str(target)}
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked'], cwd=ROOT, check=True)
        for directory in [client, cli, frontend]:
            subprocess.run(['cargo', 'build', '--workspace', '--locked'], cwd=directory, env=env, check=True)
    documents = [json.loads(path.read_text()) for path in [ROOT / 'docs/openapi.json', client / 'openapi/openapi.json', frontend / 'contract/openapi.json']]
    assert documents[0] == documents[1] == documents[2], 'pinned contracts differ'
    with tempfile.TemporaryDirectory(prefix='stabbur-integration-') as temporary:
        work = Path(temporary)
        origin = f'http://127.0.0.1:{port()}'
        console = f'http://127.0.0.1:{port()}'
        processes = []
        with (work / 'processes.log').open('wb') as log:
            try:
                server = subprocess.Popen([str(server_binary), 'all', '--data-dir', str(work / 'data'), '--bind', origin.removeprefix('http://')], stdout=log, stderr=log)
                processes.append(server)
                ready(origin, '/readyz', server)
                password = 'Local-fixture-only-password-482!'
                secret = (work / 'data/bootstrap.secret').read_text().strip()
                require(http(origin, '/api/v1/auth/bootstrap', {'secret': secret, 'username': 'live-admin', 'password': password})[0], 201, 'bootstrap')
                status, _, authentication = http(origin, '/api/v1/auth/login', {'username': 'live-admin', 'password': password})
                require(status, 200, 'upstream login')
                authorization = {'authorization': 'Bearer ' + authentication['token']}
                consumer_env = {**env, 'STABBUR_E2E_SERVER_URL': origin, 'STABBUR_E2E_TOKEN': authentication['token']}
                subprocess.run([str(target / 'debug/e2e_client')], env=consumer_env, check=True, timeout=90)
                cli_env = {**env, 'STABBUR_TOKEN': authentication['token']}
                command = [str(cli_binary), '--server', origin, '--profile', str(work / 'no-profile'), '--json']
                output = subprocess.check_output(command + ['software', 'list', '--limit', '1', '--all'], env=cli_env, timeout=20)
                page = json.loads(output)
                assert len(page['items']) >= 2 and page['next_cursor'] is None
                manifest = {'schema_version': 2, 'software': [{'slug': 'workflow-app', 'name': 'Workflow App'}], 'recipes': [{'name': 'workflow-recipe', 'revision': {'builder': 'fake', 'definition': {}, 'required_capabilities': []}}], 'targets': [{'name': 'workflow-check', 'software': 'workflow-app', 'recipe': 'workflow-recipe', 'parameters': {}, 'schedule': {'kind': 'manual'}, 'enabled': True}]}
                file = work / 'catalog.json'
                file.write_text(json.dumps(manifest))
                plan = subprocess.check_output(command + ['catalog', 'plan', '--file', str(file)], env=cli_env, timeout=20)
                plan_file = work / 'plan.json'
                plan_file.write_bytes(plan)
                assert len(json.loads(plan)['actions']) == 4
                subprocess.run(command + ['--yes', 'catalog', 'sync', '--file', str(file), '--plan-file', str(plan_file)], env=cli_env, check=True, stdout=subprocess.DEVNULL, timeout=20)
                assert not json.loads(subprocess.check_output(command + ['catalog', 'plan', '--file', str(file)], env=cli_env, timeout=20))['actions']
                status, _, reader = http(origin, '/api/v1/auth/principals', {'name': 'console-reader', 'kind': 'human', 'roles': ['reader'], 'password': password}, authorization)
                require(status, 201, 'create reader')
                frontend_env = {**env, 'STABBUR_FRONTEND_DEVELOPMENT': '1', 'STABBUR_SERVER_ORIGIN': origin, 'STABBUR_FRONTEND_ORIGIN': console, 'STABBUR_FRONTEND_BIND': console.removeprefix('http://')}
                web = subprocess.Popen([str(console_binary)], env=frontend_env, stdout=log, stderr=log)
                processes.append(web)
                ready(console, '/', web)
                jar = CookieJar()
                browser = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(jar))
                login_headers = {'origin': console, 'x-stabbur-login': '1'}
                require(http(console, '/api/login', {'username': 'live-admin', 'password': password}, {'origin': 'https://untrusted.example', 'x-stabbur-login': '1'}, browser)[0], 403, 'foreign origin login')
                require(http(console, '/api/session', opener=browser)[0], 401, 'anonymous session')
                status, headers, session = http(console, '/api/login', {'username': 'live-admin', 'password': password}, login_headers, browser)
                require(status, 200, 'console login')
                assert 'HttpOnly' in headers['set-cookie'] and 'SameSite=Strict' in headers['set-cookie']
                assert authentication['token'] not in json.dumps(session)
                csrf_headers = {'origin': console, 'x-csrf-token': session['csrf']}
                require(http(console, '/api/operation/list_software', {}, {'origin': console}, browser)[0], 403, 'missing CSRF')
                require(http(console, '/api/operation/list_software', {}, {**csrf_headers, 'origin': 'https://untrusted.example'}, browser)[0], 403, 'foreign origin operation')
                require(http(console, '/api/operation/bootstrap', {}, csrf_headers, browser)[0], 400, 'blocked bootstrap gateway')
                require(http(console, '/api/operation/list_software', {'query': {'url': 'http://untrusted.example'}}, csrf_headers, browser)[0], 400, 'unknown query')
                status, headers, page = http(console, '/api/operation/list_software', {'query': {'limit': '1'}}, csrf_headers, browser)
                require(status, 200, 'console list')
                assert len(page['items']) == 1 and page['next_cursor']
                assert headers['cache-control'] == 'no-store' and "frame-ancestors 'none'" in headers['content-security-policy']
                status, _, malicious = http(console, '/api/operation/create_software', {'body': {'slug': 'malicious-title', 'name': '<img src=x onerror=alert(1)>'}, 'idempotency_key': 'console-malicious-title'}, csrf_headers, browser)
                require(status, 200, 'literal user content')
                require(http(console, '/api/logout', {}, csrf_headers, browser)[0], 204, 'logout')
                require(http(console, '/api/session', opener=browser)[0], 401, 'logout invalidates session')
                status, _, session = http(console, '/api/login', {'username': 'console-reader', 'password': password}, login_headers, browser)
                require(status, 200, 'reader login')
                csrf_headers['x-csrf-token'] = session['csrf']
                require(http(console, '/api/operation/create_software', {'body': {'slug': 'forbidden-create', 'name': 'Forbidden'}, 'idempotency_key': 'denied-create'}, csrf_headers, browser)[0], 403, 'upstream role enforcement')
                require(http(origin, f"/api/v1/auth/principals/{reader['id']}/revoke-sessions", {}, authorization)[0], 200, 'revoke reader sessions')
                require(http(console, '/api/session', opener=browser)[0], 401, 'upstream revocation')
                print('PASS: three pinned contracts, independent client, CLI pagination/catalog v2, console login/origin/CSRF/roles/logout/revocation')
                if args.keep_running:
                    # Fixed public fixture credentials are printed only for interactive local QA; never a token.
                    print(f'Local UI fixture: {console} (live-admin; password documented in this script)', flush=True)
                    while True:
                        time.sleep(1)
            finally:
                for process in reversed(processes):
                    process.terminate()
                for process in reversed(processes):
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--client', type=Path, default=ROOT.parent / 'stabbur-client-rust')
    parser.add_argument('--cli', type=Path, default=ROOT.parent / 'stabbur-cli')
    parser.add_argument('--frontend', type=Path, default=ROOT.parent / 'stabbur-frontend')
    parser.add_argument('--target', type=Path, default=ROOT / 'target/review-cli')
    parser.add_argument('--no-build', action='store_true')
    parser.add_argument('--keep-running', action='store_true')
    run(parser.parse_args())
