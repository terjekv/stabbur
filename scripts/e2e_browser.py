"""Headless browser acceptance against only a harness-owned loopback console.

No saved authentication state, HAR, trace, video or credential downloads are collected.
"""
import re
from pathlib import Path
from contextlib import contextmanager
from urllib.parse import urlsplit

from playwright.sync_api import expect, sync_playwright


@contextmanager
def console_page(origin, password, loopback_alias=False):
    assert urlsplit(origin).hostname == '127.0.0.1'
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            context = browser.new_context(viewport={'width': 1280, 'height': 900})
            alias = origin.replace('://127.0.0.1:', '://localhost:')
            allowed = (origin + '/', alias + '/') if loopback_alias else (origin + '/',)
            context.route('**/*', lambda route: route.continue_()
                          if route.request.url.startswith(allowed) else route.abort())
            page = context.new_page()
            errors = []
            page.on('pageerror', lambda _error: errors.append('uncaught browser error'))
            page.set_default_timeout(15000)
            page.clock.install()
            if loopback_alias:
                page.goto(alias + '/#/software')
                expect(page).to_have_url(origin + '/#/software')
            else:
                page.goto(origin)
            login(page, password)
            try:
                yield page
            except Exception:
                print('Console headings:', page.locator('main h1, main h2').all_text_contents())
                print('Console status labels:', page.locator('.badge').all_text_contents())
                print('Console feedback:', page.locator('.feedback').all_text_contents())
                raise
            assert not errors, 'browser application raised an uncaught error'
        finally:
            browser.close()


def login(page, password):
    page.get_by_label('Username', exact=True).fill('live-admin')
    page.get_by_label('Password', exact=True).fill(password)
    page.get_by_role('button', name='Sign in', exact=True).click()
    expect(page.get_by_role('navigation', name='Management')).to_be_visible()


def publish_delivery(origin, password, software, version, detection, kind, channel, tested):
    with console_page(origin, password) as page:
        page.goto(origin + '/#/delivery/' + software)
        page.get_by_role('button', name=re.compile('^Publish .* to ' + channel + '$')).click()
        dialog = page.get_by_role('dialog')
        expect(dialog.get_by_role('heading', name=re.compile(re.escape(version) + ' → ' + channel))).to_be_visible()
        dialog.get_by_label('Test Mac macOS version', exact=True).fill('15.0')
        dialog.get_by_label('Installer format', exact=True).select_option(kind)
        dialog.get_by_label('Detect installed software using', exact=True).select_option(detection['kind'])
        if detection['kind'] == 'receipt':
            dialog.get_by_label('Package identifier', exact=True).fill(detection['package_id'])
        else:
            dialog.get_by_label('Application filename', exact=True).fill(detection['name'])
            dialog.get_by_label('Bundle identifier', exact=True).fill(detection['bundle_id'])
        if tested:
            dialog.get_by_role('checkbox', name='I verified installation and confirmed that a second update check does not offer this version again.', exact=True).check()
        dialog.get_by_role('checkbox', name='I reviewed this exact release, installer format, architecture and detection settings.', exact=True).check()
        dialog.get_by_role('button', name='Publish to ' + channel, exact=True).click()
        expect(dialog).not_to_be_visible(timeout=60000)
        expect(page.get_by_role('heading', name='Published versions', exact=True)).to_be_visible()
        page.set_viewport_size({'width':320,'height':800})
        assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth'), 'Munki delivery overflows at 320px'


def start_build(origin, password, manifest):
    with console_page(origin, password) as page:
        page.get_by_role('link', name='Catalog plans', exact=True).click()
        page.get_by_label('Catalog file', exact=True).set_input_files(str(manifest))
        page.get_by_role('button', name='Generate plan', exact=True).click()
        apply = page.get_by_role('button', name='Apply reviewed plan', exact=True)
        expect(apply).to_be_enabled()
        page.get_by_role('checkbox').check()
        apply.click()
        dialog = page.get_by_role('dialog')
        expect(dialog.get_by_role('heading', name='Catalog applied', exact=True)).to_be_visible()
        dialog.get_by_role('button', name='Close', exact=True).click()
        page.get_by_role('link', name='Build targets', exact=True).click()
        page.get_by_role('button', name='delivery-build', exact=True).click()
        page.get_by_role('button', name='Review and build', exact=True).click()
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Enable and start build', exact=True).click()
        expect(page).to_have_url(re.compile(r'/#/runs/[0-9a-f-]+$'))
        run_id = page.url.rsplit('/', 1)[-1]
        page.get_by_role('button', name='Sign out', exact=True).click()
        expect(page.get_by_role('button', name='Sign in', exact=True)).to_be_visible()
        page.reload()
        login(page, password)
        expect(page).to_have_url(re.compile(r'/#/runs/' + run_id + '$'))
        expect(page.get_by_role('heading', name='Build progress', exact=True)).to_be_visible()
        page.clock.fast_forward(31 * 60 * 1000)
        expect(page.get_by_role('button', name='Sign in', exact=True)).to_be_visible()
        return run_id


def promote_release(origin, password, run_id, release_id, channel='stable'):
    with console_page(origin, password) as page:
        page.goto(origin + '/#/runs/' + run_id)
        expect(page.locator('pre.logs')).not_to_be_empty()
        page.get_by_role('link', name='Review resulting release', exact=True).click()
        expect(page).to_have_url(re.compile(r'/#/releases/' + release_id + '$'))
        page.get_by_role('button', name='Promote release', exact=True).click()
        dialog = page.get_by_role('dialog')
        dialog.get_by_label('Channel *', exact=True).select_option(channel)
        expect(dialog.get_by_label('Variant', exact=True)).to_be_enabled()
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Promote release', exact=True).click()
        expect(page.get_by_role('heading', name='Delivery and builds', exact=True)).to_be_visible()
        expect(page.locator('main')).to_contain_text(channel + ':')


def withdraw_release(origin, password, release_id):
    with console_page(origin, password) as page:
        page.goto(origin + '/#/releases/' + release_id)
        page.get_by_role('button', name='Withdraw release', exact=True).click()
        dialog = page.get_by_role('dialog')
        dialog.get_by_label('Reason *', exact=True).fill('CI withdrawal drill')
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Apply withdraw release', exact=True).click()
        expect(dialog.locator('dl.properties').first).to_be_visible()
        expect(dialog.get_by_role('status')).to_have_text('Action completed.')


def operator_workflows(origin, password, published_run):
    """Regression coverage for navigation, form feedback, named builds and small screens."""
    import json
    import uuid
    from datetime import datetime, timezone
    slug = 'browser-' + uuid.uuid4().hex[:10]
    name = 'Browser <img src=x onerror=window.fixtureInjected=true> ' + slug
    with console_page(origin, password, loopback_alias=True) as page:
        expect(page.get_by_role('button', name='Workflow App', exact=True)).to_be_visible()
        page.get_by_role('button', name='＋ Software', exact=True).click()
        dialog = page.get_by_role('dialog')
        dialog.get_by_label('Name *', exact=True).fill(name)
        dialog.get_by_label('Slug *', exact=True).fill('Invalid slug!')
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Apply create software', exact=True).click()
        expect(dialog.get_by_role('alert')).to_be_visible()
        expect(dialog.get_by_role('alert')).to_contain_text('slug')
        expect(dialog.get_by_label('Slug *', exact=True)).to_have_attribute('aria-invalid', 'true')
        dialog.get_by_label('Slug *', exact=True).fill(slug)
        dialog.get_by_role('button', name='Apply create software', exact=True).click()
        expect(dialog.get_by_role('status')).to_have_text('Action completed.')
        dialog.get_by_role('button', name='Close', exact=True).click()
        expect(page.get_by_role('button', name=name, exact=True)).to_be_visible()
        assert page.locator('main img[src=x]').count() == 0, 'server text became markup'
        assert page.evaluate('window.fixtureInjected === undefined'), 'server text executed'
        page.get_by_role('button', name=name, exact=True).click()
        page.get_by_role('button', name='Edit software', exact=True).click()
        expect(dialog.get_by_label('Name', exact=True)).to_have_value(name)
        dialog.get_by_label('Name', exact=True).fill(name + ' updated')
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Apply update software', exact=True).click()
        expect(dialog.get_by_role('status')).to_have_text('Action completed.')
        dialog.get_by_role('button', name='Close', exact=True).click()
        expect(page.locator('main h1')).to_have_text(name + ' updated')
        saved_url = page.url
        page.reload()
        expect(page.locator('main h1')).to_have_text(name + ' updated')
        assert page.url == saved_url
        page.get_by_role('link', name='Runs', exact=True).first.click()
        expect(page.locator('main h1')).to_have_text('Runs')
        page.reload()
        expect(page.locator('main h1')).to_have_text('Runs')
        page.go_back()
        expect(page.locator('main h1')).to_have_text(name + ' updated')
        page.get_by_role('link', name='Catalog plans', exact=True).click()
        expect(page.get_by_role('link', name='Catalog plans', exact=True)).to_have_attribute('aria-current','page')
        desired = {'schema_version':2,'software':[{'slug':'review-' + slug,'name':'Review fixture'}],
                   'recipes':[], 'targets':[]}
        page.get_by_label('Catalog file', exact=True).set_input_files({'name':'catalog.json','mimeType':'application/json','buffer':json.dumps(desired).encode()})
        page.get_by_role('button', name='Generate plan', exact=True).click()
        expect(page.get_by_role('heading', name='1 proposed change', exact=True)).to_be_visible()
        expect(page.locator('.plan-change')).to_contain_text('Review fixture')
        page.get_by_role('link', name='Build targets', exact=True).click()
        page.get_by_role('button', name='＋ Build Target', exact=True).click()
        dialog.get_by_label('Name *', exact=True).fill('target-' + slug)
        software = dialog.get_by_label('Software *', exact=True)
        expect(software).to_be_visible()
        software.select_option(label=name + ' updated (' + slug + ')')
        recipe = dialog.get_by_label('Recipe *', exact=True)
        expect(recipe).to_be_visible()
        recipe.select_option(label='workflow-recipe')
        revision = dialog.get_by_label('Recipe revision *', exact=True)
        expect(revision.locator('option')).to_have_count(2)
        revision.select_option(index=1)
        dialog.get_by_label('Schedule *', exact=True).select_option('interval')
        dialog.get_by_label('Every', exact=True).fill('2')
        dialog.get_by_label('Unit', exact=True).select_option('3600')
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Save build target', exact=True).click()
        expect(page.locator('main h1')).to_have_text('target-' + slug)
        expect(page.locator('main')).to_contain_text('Every 2 hours')
        expect(page.get_by_role('button', name='Build now', exact=True)).to_be_disabled()
        # Disabled controls and labels stay readable under narrow viewport/reflow.
        page.set_viewport_size({'width':320,'height':800})
        for title in ['Software','Build targets','Runs','Recipes','Workers','Storage','Access','Audit history','Catalog plans']:
            expect(page.get_by_role('navigation').get_by_role('link',name=title,exact=True)).to_be_in_viewport()
        assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth'), 'page overflows at 320px'
        page.keyboard.press('Tab')
        assert page.evaluate('document.activeElement !== document.body'), 'keyboard focus was lost'
        page.set_viewport_size({'width':1280,'height':900})
        page.get_by_role('button', name='Edit build target', exact=True).click()
        dialog.get_by_label('Schedule *', exact=True).select_option('manual')
        dialog.get_by_label('Triggering', exact=True).select_option('true')
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Save build target', exact=True).click()
        expect(page.get_by_role('button', name='Build now', exact=True)).to_be_enabled()
        page.get_by_role('button', name='Build now', exact=True).click()
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Start build', exact=True).click()
        expect(page).to_have_url(re.compile(r'/#/runs/[0-9a-f-]+$'))
        expect(page.get_by_role('button', name='Logs complete', exact=True)).to_be_visible(timeout=20000)
        expect(page.locator('.badge').first).to_have_text('Succeeded')
        # The portable fake build intentionally has no artifacts or logs. A leased publication
        # fixture exercises replay across multiple pages and the actual server publication policy.
        page.goto(origin + '/#/recipes')
        page.get_by_role('button', name='Add software from recipes', exact=True).click()
        page.get_by_role('heading', name='Add software', exact=True).wait_for()
        ready = page.get_by_role('checkbox', name='Select example.download.ImportedApp', exact=True)
        ready.wait_for()
        assert page.get_by_role('checkbox', name='Select example.override.Uncommitted', exact=True).is_disabled()
        ready.check()
        page.set_viewport_size({'width':320,'height':800})
        assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth'), 'recipe import overflows at 320px'
        page.set_viewport_size({'width':1280,'height':900})
        page.screenshot(path=str(Path(__file__).resolve().parents[1] / 'target/import-ui.png'), full_page=True)
        page.get_by_label('Installer architecture for Imported App', exact=True).select_option('aarch64')
        page.get_by_label('Software slug', exact=True).fill('console-imported-app')
        page.get_by_label('Software name', exact=True).fill('Imported application')
        page.get_by_label('Version output variable', exact=True).fill('version')
        page.get_by_label('Installer output variable', exact=True).fill('pathname')
        page.get_by_role('button', name='Review sources and import plan', exact=True).click()
        page.get_by_role('heading', name='Catalog plans', exact=True).wait_for()
        expect(page).to_have_url(re.compile(r'/#/catalog$'))
        page.get_by_text('Will be disabled', exact=False).wait_for()
        page.get_by_role('checkbox', name='I have reviewed every change, source pin and target that will be enabled.', exact=True).check()
        page.get_by_role('button', name='Apply reviewed plan', exact=True).click()
        page.get_by_role('heading', name='Catalog applied', exact=True).wait_for()
        page.get_by_role('button', name='Close', exact=True).click()
        page.goto(origin + '/#/targets')
        imported_row = page.get_by_role('row').filter(has_text='console-imported-app')
        imported_row.wait_for()
        assert 'Disabled' in imported_row.inner_text()

        page.goto(origin + '/#/runs/' + published_run)
        expect(page.get_by_role('link', name='Review resulting release', exact=True)).to_be_visible(timeout=20000)
        expect(page.get_by_role('button', name='Logs complete', exact=True)).to_be_visible()
        expect(page.locator('pre.logs')).to_contain_text('Fixture log 239')
        assert page.locator('pre.logs').inner_text().count('Fixture log 000') == 1
        page.get_by_role('link', name='Review resulting release', exact=True).click()
        expect(page.get_by_role('button', name='Promote release', exact=True)).to_be_enabled()
        release_url = page.url
        stale = page.context.new_page()
        stale.goto(release_url)
        stale.get_by_role('button', name='Promote release', exact=True).click()
        stale_dialog = stale.get_by_role('dialog')
        expect(stale_dialog.get_by_label('Variant', exact=True)).to_be_enabled()
        stale_dialog.get_by_role('checkbox').check()
        page.get_by_role('button', name='Promote release', exact=True).click()
        expect(dialog.get_by_label('Variant', exact=True)).to_be_enabled()
        expect(dialog.get_by_text('macOS · Universal', exact=True)).to_be_visible()
        expect(dialog.get_by_text('No upper limit', exact=True)).to_be_visible()
        expect(dialog.locator('.callout')).to_be_in_viewport()
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Promote release', exact=True).click()
        expect(page.get_by_role('heading', name='Delivery and builds', exact=True)).to_be_visible()
        expect(page.locator('main')).to_contain_text('testing:')
        stale_dialog.get_by_role('button', name='Promote release', exact=True).click()
        expect(stale_dialog.get_by_role('alert')).to_contain_text('changed')
        expect(stale_dialog.get_by_label('Release *', exact=True)).not_to_have_value('')
        stale.close()
        page.clock.fast_forward(31 * 60 * 1000)
        expect(page.get_by_role('button', name='Sign in', exact=True)).to_be_visible()
        # Only the browser clock advanced; align it with the real server before signing in again.
        page.clock.set_system_time(datetime.now(timezone.utc))
        login(page, password)
        expect(page.get_by_role('heading', name='Delivery and builds', exact=True)).to_be_visible()
        page.get_by_role('button', name='Sign out', exact=True).click()
        expect(page.get_by_role('button', name='Sign in', exact=True)).to_be_visible()
