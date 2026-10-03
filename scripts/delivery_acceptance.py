"""Managed Munki publication acceptance; only harness-owned state and disposable installs."""
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import time
import uuid


def exercise_delivery(args, work, api, gateway, console, password, first_release,
                      identifier, destination, build_upgrade, feed_origin):
    # Reuse the running harness without launching a second server or worker.
    from e2e_browser import publish_delivery, withdraw_release

    def require(condition, message):
        if not condition:
            raise AssertionError(message)

    def command(argv):
        result = subprocess.run(list(map(str, argv)), capture_output=True, timeout=180)
        require(result.returncode == 0, f'{Path(argv[0]).name} delivery check failed')
        return result.stdout

    def promote(software, release, channel):
        current = api.call(f'/api/v1/software/{software}/channels/{channel}', expected=(200, 404))
        revision = current.get('revision', 0)
        gateway.call('/api/operation/promote_channel', {'parameters': {'software': software, 'channel': channel},
            'revision': revision, 'body': {'release_id': release['id'], 'reason': 'Reviewed disposable delivery test'}})

    def publish(software, release, detection, kind='pkg', channel='testing', tested=False):
        spec = {'software': software, 'channel': channel, 'architecture': 'aarch64', 'macos': '15.0',
                'format': kind, 'detection': detection}
        before = gateway.call('/api/delivery')
        data = {'spec': spec, 'release': release['id'], 'expected_revision': before['revision'],
                'reviewed': True, 'test_confirmed': tested}
        if args.browser:
            publish_delivery(console, password, software, release['version'], detection, kind, channel, tested)
        else:
            gateway.call('/api/delivery/publish', data)
        # A second writer must re-review after a publication, never silently overwrite it.
        gateway.call('/api/delivery/publish', data, expected=409)
        after = gateway.call('/api/delivery')
        require(after['revision'] > before['revision'], 'publication advances delivery revision')
        return next(item for item in after['entries'] if item['release'] == release['id'] and item['spec']['channel'] == channel)

    def profile(software, channel='testing'):
        body = gateway.call('/api/delivery/profile', {'channel': channel, 'software': software})
        config = plistlib.loads(body)
        require(config['PayloadType'] == 'Configuration', 'download is a real macOS profile')
        return config['PayloadContent'][0]

    def client(preferences):
        from urllib.parse import urlsplit
        from urllib.request import Request, build_opener, ProxyHandler
        origin = preferences['SoftwareRepoURL']
        require(urlsplit(origin).hostname == '127.0.0.1', 'fixture repository must remain loopback')
        auth = preferences['AdditionalHttpHeaders'][0].split(': ', 1)[1]

        def read(path, expected=200):
            from urllib.error import HTTPError
            opener = build_opener(ProxyHandler({}))
            try:
                with opener.open(Request(origin + '/' + path, headers={'Authorization': auth}), timeout=20) as response:
                    status, body = response.status, response.read()
            except HTTPError as error:
                status, body = error.code, error.read()
            require(status == expected, f'Munki {path.split("/")[0]} returned {status}; expected {expected}')
            return body
        return read

    @contextmanager
    def test_mac(preferences, name):
        installed_preferences = Path('/Library/Preferences/ManagedInstalls.plist')
        managed = work / ('managed-' + name)
        if args.install_fixture:
            require(os.environ.get('STABBUR_DISPOSABLE_MACOS') == '1', 'installation requires disposable Mac')
            require(not installed_preferences.exists(), 'never replace existing Munki preferences')
            managed.mkdir()
            config = {key: preferences[key] for key in ['SoftwareRepoURL', 'ClientIdentifier', 'AdditionalHttpHeaders']}
            config.update(ManagedInstallDir=str(managed), InstallAppleSoftwareUpdates=False,
                          SuppressAutoInstall=True, LogFile=str(work / ('munki-' + name + '.log')))
            file = work / ('preferences-' + name + '.plist')
            file.write_bytes(plistlib.dumps(config))
            file.chmod(0o600)
            command(['sudo', '-n', '/usr/bin/install', '-m', '600', file, installed_preferences])
        try:
            yield managed
        finally:
            if args.install_fixture:
                command(['sudo', '-n', '/bin/rm', '-f', installed_preferences])
                command(['sudo', '-n', '/usr/sbin/chown', '-R', str(os.getuid()), managed])

    def install_check(managed, software, verify):
        if not args.install_fixture:
            return
        tool = args.munki_tools / 'managedsoftwareupdate'
        command(['sudo', '-n', tool, '--checkonly', '--munkipkgsonly'])
        before = plistlib.loads((managed / 'InstallInfo.plist').read_bytes())
        require(any(item['name'] == software for item in before.get('managed_installs', [])), 'Munki offers new or upgraded version')
        command(['sudo', '-n', tool, '--installonly', '--munkipkgsonly'])
        verify()
        command(['sudo', '-n', tool, '--checkonly', '--munkipkgsonly'])
        after = plistlib.loads((managed / 'InstallInfo.plist').read_bytes())
        require(not after.get('managed_installs'), 'installed-state detection prevents reinstall')

    def build(target, software):
        run = gateway.call('/api/operation/trigger_build_target', {'parameters': {'target': target}, 'idempotency_key': str(uuid.uuid4())})
        deadline = time.monotonic() + 240
        while time.monotonic() < deadline:
            result = api.call('/api/v1/runs/' + run['id'])
            require(result['state'] not in ('failed', 'cancelled'), 'real delivery build failed')
            if result['state'] == 'succeeded':
                return api.call(f'/api/v1/software/{software}/releases')['items'][0]
            time.sleep(.25)
        raise AssertionError('delivery build timed out')

    detection = {'kind': 'receipt', 'package_id': identifier}
    initial_revision = gateway.call('/api/delivery')['revision']
    gateway.call('/api/delivery/publish', {'spec': {'software':'delivery-fixture','channel':'stable',
        'architecture':'aarch64','macos':'15.0','format':'pkg','detection':detection},
        'release':first_release['id'],'expected_revision':initial_revision,'reviewed':True,'test_confirmed':False}, expected=400)
    promote('delivery-fixture', first_release, 'testing')
    first = publish('delivery-fixture', first_release, detection)
    preferences = profile('delivery-fixture')
    read = client(preferences)
    catalog = plistlib.loads(read('catalogs/testing'))
    require(catalog[0]['supported_architectures'] == ['arm64'], 'real repository maps architecture')
    require(hashlib.sha256(read('pkgs/' + first['digest'] + '.pkg')).hexdigest() == first['digest'], 'served package bytes verified')
    require(plistlib.loads(read('manifests/delivery-fixture'))['managed_installs'] == ['delivery-fixture'], 'application profile selects managed install')
    try:
        with test_mac(preferences, 'upgrade') as managed:
            def verify(version):
                require((destination / 'proof.txt').read_bytes() == f'Stabbur disposable delivery fixture {version}\n'.encode(), 'installed package payload matches version')
                receipt = plistlib.loads(command(['/usr/sbin/pkgutil', '--pkg-info-plist', identifier]))
                require(receipt['pkg-version'] == version, 'installed receipt matches version')
            install_check(managed, 'delivery-fixture', lambda: verify('1.0'))
            build_upgrade()
            second_release = build('delivery-build', 'delivery-fixture')
            require(second_release['version'] == '2.0', 'AutoPkg upgrade output version')
            promote('delivery-fixture', second_release, 'testing')
            second = publish('delivery-fixture', second_release, detection)
            updated = plistlib.loads(read('catalogs/testing'))
            require(len(updated) == 1 and updated[0]['version'] == '2.0', 'new generation replaces the selected version')
            read('pkgs/' + first['digest'] + '.pkg', expected=404)
            install_check(managed, 'delivery-fixture', lambda: verify('2.0'))
            if args.install_fixture:
                promote('delivery-fixture', second_release, 'stable')
                publish('delivery-fixture', second_release, detection, channel='stable', tested=True)
                # Preserve the original stable selection for the independent recovery/export drill.
                promote('delivery-fixture', first_release, 'stable')
            if args.browser:
                withdraw_release(console, password, second_release['id'])
            else:
                current = api.call('/api/v1/releases/' + second_release['id'])
                gateway.call('/api/operation/withdraw_release', {'parameters': {'release': current['id']}, 'revision': current['revision'], 'body': {'reason': 'delivery withdrawal test'}})
            require(not plistlib.loads(read('catalogs/testing')), 'withdrawal removes served catalog entry')
            read('pkgs/' + second['digest'] + '.pkg', expected=404)
    finally:
        if args.install_fixture:
            command(['sudo', '-n', '/bin/rm', '-rf', destination])
            command(['sudo', '-n', '/usr/sbin/pkgutil', '--forget', identifier])

    # A second actual AutoPkg build covers copy_from_dmg and application detection.
    unique = uuid.uuid4().hex
    app_name = 'StabburDeliveryTest-' + unique + '.app'
    app_id = 'org.stabbur.deliverytest.' + unique
    dmg_root = work / 'dmg-root'
    contents = dmg_root / app_name / 'Contents'
    (contents / 'MacOS').mkdir(parents=True)
    (contents / 'Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier': app_id,
        'CFBundleName': 'Stabbur delivery test', 'CFBundleShortVersionString': '1.0',
        'CFBundleVersion': '1.0', 'CFBundlePackageType': 'APPL', 'CFBundleExecutable': 'fixture'}))
    executable = contents / 'MacOS/fixture'
    executable.write_text('#!/bin/sh\nexit 0\n')
    executable.chmod(0o755)
    dmg = work / 'public/fixture.dmg'
    command(['/usr/bin/hdiutil', 'create', '-quiet', '-srcfolder', dmg_root, '-format', 'UDZO', dmg])
    (work / 'public/appcast.xml').write_text(
        '<?xml version="1.0"?><rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">'
        '<channel><item><title>1.0</title>'
        f'<enclosure url="{feed_origin}/fixture.dmg" sparkle:version="1.0" sparkle:shortVersionString="1.0" '
        f'length="{dmg.stat().st_size}" type="application/octet-stream"/></item></channel></rss>')
    manifest = json.loads((work / 'catalog.json').read_text())
    manifest['software'] = [{'slug': 'delivery-app', 'name': 'Delivery application'}]
    manifest['recipes'][0]['name'] = 'delivery-app-recipe'
    manifest['recipes'][0]['revision']['definition']['output']['variants'][0]['architecture'] = 'universal'
    manifest['targets'] = [{'name':'delivery-app-build','software':'delivery-app','recipe':'delivery-app-recipe','parameters':{},'schedule':{'kind':'manual'},'enabled':True}]
    plan = gateway.call('/api/catalog/plan', {'manifest': manifest})
    gateway.call('/api/catalog/apply', {'manifest': manifest, 'plan': plan})
    app_release = build('delivery-app-build', 'delivery-app')
    promote('delivery-app', app_release, 'testing')
    published = publish('delivery-app', app_release, {'kind':'application','name':app_name,'bundle_id':app_id}, kind='dmg_app')
    app_preferences = profile('delivery-app')
    app_read = client(app_preferences)
    app_catalog = plistlib.loads(app_read('catalogs/testing'))
    require(app_catalog[0]['installer_type'] == 'copy_from_dmg' and app_catalog[0]['supported_architectures'] == ['arm64','x86_64'], 'DMG and universal installer metadata')
    require(hashlib.sha256(app_read('pkgs/' + published['digest'] + '.dmg')).hexdigest() == published['digest'], 'served disk image verified')
    installed_app = Path('/Applications') / app_name
    require(not installed_app.exists(), 'never replace an existing application')
    try:
        with test_mac(app_preferences, 'dmg') as managed:
            def verify_app():
                require((installed_app / 'Contents/Info.plist').read_bytes() == (contents / 'Info.plist').read_bytes(), 'Munki copies the exact application')
            install_check(managed, 'delivery-app', verify_app)
    finally:
        if args.install_fixture:
            command(['sudo', '-n', '/bin/rm', '-rf', installed_app])
    return {'repository':'authenticated catalog, manifests, verified PKG and DMG bytes',
            'upgrade':'1.0 to 2.0; stale publication rejected; old installer URL removed',
            'withdrawal':'catalog and installer removed through browser withdrawal',
            'installation':'PKG install, upgrade, DMG copy and repeat detection passed' if args.install_fixture else 'intentionally skipped'}
