#!/usr/bin/env python3
"""Cross-repository acceptance against a local server or an immutable released image."""
import argparse
from http.cookiejar import CookieJar
import json
import os
from pathlib import Path
import re
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
    env = {key: value for key, value in os.environ.items() if not key.startswith('STABBUR_')}
    env['CARGO_TARGET_DIR'] = str(target)
    if not args.no_build:
        if not args.server_image:
            subprocess.run(['cargo', 'build', '--locked'], cwd=ROOT, check=True)
        for directory in [client, cli, frontend]:
            subprocess.run(['cargo', 'build', '--workspace', '--locked'], cwd=directory, env=env, check=True)
    documents = [json.loads(path.read_text()) for path in [ROOT / 'docs/openapi.json', client / 'openapi/openapi.json', frontend / 'contract/openapi.json']]
    assert documents[0] == documents[1] == documents[2], 'pinned contracts differ'
    if args.server_image:
        if not re.fullmatch(r'ghcr\.io/terjekv/stabbur-server@sha256:[a-f0-9]{64}', args.server_image):
            raise ValueError('server image must be the Stabbur GHCR repository pinned by SHA-256')
        subprocess.run(['docker', 'pull', args.server_image], check=True, timeout=300)
    with tempfile.TemporaryDirectory(prefix='stabbur-integration-') as temporary:
        work = Path(temporary)
        origin = f'http://127.0.0.1:{port()}'
        console = f'http://127.0.0.1:{port()}'
        processes = []
        container = None
        with (work / 'processes.log').open('wb') as log:
            try:
                if args.server_image:
                    container = work.name
                    server_command = ['docker', 'run', '--rm', '--name', container, '--publish',
                                      origin.removeprefix('http://') + ':8080', args.server_image]
                else:
                    server_command = [str(server_binary), 'all', '--data-dir', str(work / 'data'), '--bind', origin.removeprefix('http://')]
                server = subprocess.Popen(server_command, env=env, stdout=log, stderr=log)
                processes.append(server)
                ready(origin, '/readyz', server)
                status, _, served_contract = http(origin, '/api/v1/openapi.json')
                require(status, 200, 'served OpenAPI contract')
                assert served_contract == documents[0], 'running server contract differs from the pinned document'
                password = 'Local-fixture-only-password-482!'
                secret = (subprocess.check_output(['docker', 'exec', container, 'cat', '/var/lib/stabbur/bootstrap.secret'], timeout=15).decode().strip()
                          if container else (work / 'data/bootstrap.secret').read_text().strip())
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
                frontend_env = {**env, 'STABBUR_FRONTEND_DEVELOPMENT': '1', 'STABBUR_SERVER_ORIGIN': origin, 'STABBUR_FRONTEND_ORIGIN': console, 'STABBUR_FRONTEND_BIND': console.removeprefix('http://'), 'STABBUR_FRONTEND_DATA_DIR': str(work / 'delivery')}
                web = subprocess.Popen([str(console_binary)], env=frontend_env, stdout=log, stderr=log)
                processes.append(web)
                ready(console, '/', web)
                jar = CookieJar()
                browser = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(jar))
                login_headers = {'origin': console, 'x-stabbur-login': '1'}
                require(http(console, '/api/login', {'username': 'live-admin', 'password': password}, {'origin': 'https://untrusted.example', 'x-stabbur-login': '1'}, browser)[0], 403, 'foreign origin login')
                require(http(console, '/api/session', opener=browser)[0], 401, 'anonymous session')
                import_body = {'snapshot':'11111111-1111-7111-8111-111111111111', 'selections':[]}
                require(http(console, '/api/recipe-import', import_body, {'origin':console}, browser)[0], 401, 'anonymous recipe import')
                status, headers, session = http(console, '/api/login', {'username': 'live-admin', 'password': password}, login_headers, browser)
                require(status, 200, 'console login')
                assert 'HttpOnly' in headers['set-cookie'] and 'SameSite=Strict' in headers['set-cookie']
                assert authentication['token'] not in json.dumps(session)
                csrf_headers = {'origin': console, 'x-csrf-token': session['csrf']}
                require(http(console, '/api/delivery/profile', {'channel':'testing'}, {'origin':console}, browser)[0], 403, 'delivery profile requires CSRF')
                require(http(console, '/api/delivery/profile', {'channel':'testing'}, {**csrf_headers,'origin':'https://untrusted.example'}, browser)[0], 403, 'delivery profile requires exact origin')
                profile_status, profile_headers, profile_bytes = http(console, '/api/delivery/profile', {'channel':'testing'}, csrf_headers, browser)
                require(profile_status, 200, 'administrator can download device profile')
                require(profile_headers['content-disposition'].startswith('attachment;'), True, 'profile is a credential attachment')
                import plistlib
                profile = plistlib.loads(profile_bytes)
                require(profile['PayloadScope'], 'System', 'profile is system-wide')
                require(profile['PayloadContent'][0]['PayloadType'], 'ManagedInstalls', 'profile targets Munki preferences')
                require(profile['PayloadContent'][0]['SoftwareRepoURL'], console + '/munki/testing', 'profile selects exact repository channel')
                require(http(console, '/munki/testing/catalogs/testing')[0], 401, 'repository requires separate device credential')
                require(http(console, '/api/operation/list_software', {}, {'origin': console}, browser)[0], 403, 'missing CSRF')
                require(http(console, '/api/recipe-import', import_body, {'origin':console}, browser)[0], 403, 'import missing CSRF')
                require(http(console, '/api/recipe-import', import_body, {**csrf_headers, 'origin':'https://untrusted.example'}, browser)[0], 403, 'foreign origin import')
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
                require(http(console, '/api/delivery/profile', {'channel':'testing'}, csrf_headers, browser)[0], 403, 'reader cannot issue repository profiles')
                require(http(console, '/api/operation/create_software', {'body': {'slug': 'forbidden-create', 'name': 'Forbidden'}, 'idempotency_key': 'denied-create'}, csrf_headers, browser)[0], 403, 'upstream role enforcement')
                require(http(origin, f"/api/v1/auth/principals/{reader['id']}/revoke-sessions", {}, authorization)[0], 200, 'revoke reader sessions')
                require(http(console, '/api/session', opener=browser)[0], 401, 'upstream revocation')
                print('PASS: three pinned contracts, independent client, CLI pagination/catalog v2, console login/origin/CSRF/roles/logout/revocation')
                if args.browser:
                    from e2e_browser import operator_workflows, saved_export_workflow, console_page, library_workflow
                    from console_fixture import publish_fixture
                    published_run = publish_fixture(origin, authorization, http)
                    snapshots = json.loads(subprocess.check_output(command + ['catalog', 'snapshots', '--all'], env=cli_env, timeout=20))
                    inventory = next(item for item in snapshots['items'] if item['source']['revision'] == 'browser-fixture')
                    selection_file = work / 'import-selections.json'
                    selection_file.write_text(json.dumps([{'identifier':'example.download.ImportedApp','slug':'cli-imported-app','name':'CLI imported app','architecture':'aarch64','minimum_macos':'13','version_variable':'version','artifact_variable':'pathname','media_type':'application/octet-stream'}]))
                    imported_file = work / 'imported-catalog.json'
                    import_command = command + ['catalog','import','--snapshot',inventory['id'],'--selections',str(selection_file),'--output',str(imported_file)]
                    subprocess.run(import_command, env=cli_env, check=True, stdout=subprocess.DEVNULL, timeout=20)
                    desired = json.loads(imported_file.read_text())
                    assert desired['targets'][0]['enabled'] is False and desired['targets'][0]['schedule']['kind'] == 'manual'
                    assert desired['recipes'][0]['revision']['definition']['sources'][0]['commit'] == 'a' * 40
                    assert subprocess.run(import_command, env=cli_env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20).returncode != 0
                    assert json.loads(imported_file.read_text()) == desired
                    operator_workflows(console, password, published_run)
                    export_id,export_cookies=saved_export_workflow(console,password)
                    with console_page(console,password) as reused:
                        assert reused.context.cookies()==export_cookies, 'browser helpers must reuse the fixture session without another login'
                    saved=json.loads(subprocess.check_output(command+['exports','show',export_id],env=cli_env,timeout=20))
                    assert saved['definition']['name']=='Staff Macs' and saved['generation']==1
                    export_plan=work/'export-plan.json'
                    subprocess.run(command+['exports','plan',export_id,'--output',str(export_plan)],env=cli_env,check=True,stdout=subprocess.DEVNULL,timeout=20)
                    preview=json.loads(export_plan.read_text());assert preview['ready'] and preview['changes'][0]['action']=='unchanged'
                    subprocess.run(command+['--yes','exports','apply','--plan-file',str(export_plan)],env=cli_env,check=True,stdout=subprocess.DEVNULL,timeout=20)
                    assert subprocess.run(command+['--yes','exports','apply','--plan-file',str(export_plan)],env=cli_env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=20).returncode!=0
                    download=work/'downloaded-export'
                    subprocess.run(command+['exports','download',export_id,'--output',str(download)],env=cli_env,check=True,stdout=subprocess.DEVNULL,timeout=20)
                    catalog=plistlib.loads((download/'repository/catalogs/production').read_bytes());assert len(catalog)==1
                    import hashlib
                    assert hashlib.sha256((download/'repository/pkgs'/catalog[0]['installer_item_location']).read_bytes()).hexdigest()==catalog[0]['installer_item_hash']
                    assert not (download/'repository/manifests').exists()
                    assert subprocess.run(command+['exports','download',export_id,'--output',str(download)],env=cli_env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=20).returncode!=0
                    # Reuse the authenticated browser session; do not defeat the login rate limit.
                    from http.cookiejar import Cookie
                    for cookie in export_cookies:
                        jar.set_cookie(Cookie(0,cookie['name'],cookie['value'],None,False,cookie['domain'],False,False,cookie['path'],True,cookie['secure'],None,True,None,None,{}))
                    status,_,session=http(console,'/api/session',opener=browser)
                    require(status,200,'export administrator session')
                    export_headers={'origin':console,'x-csrf-token':session['csrf']}
                    profile_path='/api/exports/'+export_id+'/profile'
                    require(http(console,profile_path,{'reviewed':True},{'origin':console},browser)[0],403,'saved profile requires CSRF')
                    require(http(console,profile_path,{'reviewed':True},{**export_headers,'origin':'https://untrusted.example'},browser)[0],403,'saved profile requires exact origin')
                    require(http(console,profile_path,{'reviewed':False},export_headers,browser)[0],400,'saved profile requires review')
                    status,headers,body=http(console,profile_path,{'reviewed':True},export_headers,browser)
                    require(status,200,'saved profile download')
                    assert headers['content-disposition'].startswith('attachment;')
                    preferences=plistlib.loads(body)['PayloadContent'][0]
                    assert preferences['PayloadType']=='ManagedInstalls' and preferences['ClientIdentifier']=='site_default'
                    repo_path='/munki/exports/'+export_id+'/catalogs/production'
                    require(http(console,repo_path)[0],401,'anonymous saved repository denied')
                    device_auth={'authorization':preferences['AdditionalHttpHeaders'][0].split(': ',1)[1]}
                    status,_,body=http(console,repo_path,headers=device_auth)
                    require(status,200,'saved repository device authentication')
                    assert len(plistlib.loads(body))==1
                    status,_,archive=http(console,'/api/exports/'+export_id+'/snapshots/2/download',opener=browser)
                    require(status,200,'snapshot tar download')
                    import io,tarfile
                    with tarfile.open(fileobj=io.BytesIO(archive),mode='r:') as bundle:
                        names=bundle.getnames();assert 'repository/catalogs/production' in names
                        assert not any(name.startswith('repository/manifests') for name in names)
                    subprocess.run(command+['--yes','exports','revoke-profiles',export_id],env=cli_env,check=True,stdout=subprocess.DEVNULL,timeout=20)
                    require(http(console,repo_path,headers=device_auth)[0],401,'CLI revocation invalidates browser-issued device profile')
                    print('PASS: browser saved selection and blocker preview; CLI reads, reviews, applies and downloads the same export; stale plans and existing destinations rejected')

                    for index in range(240):
                        require(http(origin, '/api/v1/software', {'slug':f'scale-{index:03}', 'name':f'Scale application {index:03}'}, authorization)[0], 201, 'seed scale library')
                    result = json.loads(subprocess.check_output(command+['software','search','Scale application 239','--view','not_built'],env=cli_env,timeout=20))
                    assert len(result['items'])==1 and result['items'][0]['slug']=='scale-239'
                    library_workflow(console,password)
                    print('PASS: 240-application search, no per-row status reads, selection across pages and queries, paginated export picker, shared views, CLI search, mobile layout')
                    print('PASS: console navigation, validation, catalog review, named builds, paginated logs, publication, stale promotion, expiry, literal text and mobile reflow')
                if args.keep_running:
                    # Fixed public fixture credentials are printed only for interactive local QA; never a token.
                    print(f'Local UI fixture: {console} (live-admin; password documented in this script)', flush=True)
                    while True:
                        time.sleep(1)
            finally:
                if container:
                    subprocess.run(['docker', 'rm', '--force', container], stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, timeout=30, check=False)
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
    parser.add_argument('--browser', action='store_true', help='Run headless operator workflow regressions with Playwright installed')
    parser.add_argument('--server-image', help='Exercise the exact immutable GHCR server image instead of a local server binary')
    run(parser.parse_args())
