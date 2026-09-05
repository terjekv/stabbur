"""Headless CI browser acceptance against only the harness-owned loopback console.

No saved authentication state, HAR, trace, video or credential downloads are collected.
"""
import re
from contextlib import contextmanager
from urllib.parse import urlsplit

from playwright.sync_api import expect, sync_playwright


@contextmanager
def console_page(origin, password):
    assert urlsplit(origin).hostname == '127.0.0.1'
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            context = browser.new_context(viewport={'width': 1280, 'height': 900})
            context.route('**/*', lambda route: route.continue_()
                          if route.request.url.startswith(origin + '/') else route.abort())
            page = context.new_page()
            errors = []
            page.on('pageerror', lambda _error: errors.append('uncaught browser error'))
            page.set_default_timeout(15000)
            page.goto(origin)
            login(page, password)
            try:
                yield page
            except Exception:
                # Only safe UI diagnostics, never input values, response bodies, cookies or traces.
                print('Console notice:', page.locator('#notice').inner_text())
                print('Dialog action labels:', page.get_by_role('dialog').get_by_role('button').all_text_contents())
                raise
            assert not errors, 'browser application raised an uncaught error'
        finally:
            browser.close()


def login(page, password):
    page.get_by_label('Username', exact=True).fill('live-admin')
    page.get_by_label('Password', exact=True).fill(password)
    page.get_by_role('button', name='Sign in', exact=True).click()
    expect(page.get_by_role('navigation', name='Management')).to_be_visible()


def start_build(origin, password, manifest):
    assert urlsplit(origin).hostname == '127.0.0.1'
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            context = browser.new_context(viewport={'width': 1280, 'height': 900})
            # A UI regression cannot send test data to external sites.
            context.route('**/*', lambda route: route.continue_()
                          if route.request.url.startswith(origin + '/') else route.abort())
            page = context.new_page()
            page.clock.install()
            errors = []
            page.on('pageerror', lambda _error: errors.append('uncaught browser error'))
            page.set_default_timeout(15000)
            page.goto(origin)
            login(page, password)
            page.get_by_role('button', name='Catalog plans', exact=True).click()
            page.get_by_label('Catalog file', exact=True).set_input_files(str(manifest))
            page.get_by_role('button', name='Generate plan', exact=True).click()
            apply = page.get_by_role('button', name='Apply reviewed plan', exact=True)
            expect(apply).to_be_enabled()
            page.once('dialog', lambda dialog: dialog.accept())
            apply.click()
            dialog = page.get_by_role('dialog')
            expect(dialog.get_by_role('heading', name='Catalog applied', exact=True)).to_be_visible()
            dialog.get_by_role('button', name='Close', exact=True).click()
            page.get_by_role('button', name='Build targets', exact=True).click()
            page.get_by_role('button', name='delivery-build', exact=True).click()
            dialog.get_by_role('button', name='Trigger Build Target', exact=True).click()
            dialog.get_by_role('checkbox').check()
            dialog.get_by_role('button', name='Apply trigger build target', exact=True).click()
            identity = dialog.locator('dt').filter(has_text=re.compile(r'^Id$')).locator('xpath=following-sibling::dd[1]')
            expect(identity).to_have_text(re.compile(r'^[0-9a-f-]{36}$'))
            run_id = identity.inner_text()
            dialog.get_by_role('button', name='Close', exact=True).click()

            # Re-login proves logout returned the browser to the unauthenticated flow.
            page.get_by_role('button', name='Sign out', exact=True).click()
            expect(page.get_by_role('button', name='Sign in', exact=True)).to_be_visible()
            page.reload()
            login(page, password)
            page.get_by_role('button', name='Runs', exact=True).click()
            expect(page.get_by_role('button', name=run_id, exact=True)).to_be_visible()

            # Browser expiry UI uses its own clock; backend expiry has separate Rust tests.
            page.clock.fast_forward(31 * 60 * 1000)
            expect(page.get_by_role('button', name='Sign in', exact=True)).to_be_visible()
            assert not errors, 'browser application raised an uncaught error'
            return run_id
        finally:
            browser.close()


def promote_release(origin, password, run_id, release_id):
    with console_page(origin, password) as page:
        dialog = page.get_by_role('dialog')
        page.get_by_role('button', name='Runs', exact=True).click()
        page.get_by_role('button', name=run_id, exact=True).click()
        dialog.get_by_role('button', name='List Run Logs', exact=True).click()
        dialog.get_by_role('button', name='Show results', exact=True).click()
        expect(dialog.locator('pre.logs')).not_to_be_empty()
        dialog.get_by_role('button', name='Close', exact=True).click()
        page.get_by_role('button', name='Software', exact=True).click()
        page.get_by_role('button', name='Delivery fixture', exact=True).click()
        dialog.get_by_role('button', name='Promote Channel', exact=True).click()
        dialog.get_by_label('Channel *', exact=True).fill('stable')
        dialog.get_by_label('Current revision *', exact=True).fill('0')
        dialog.get_by_label('Release Id *', exact=True).fill(release_id)
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Apply promote channel', exact=True).click()
        expect(dialog.locator('dl.properties').first).to_be_visible()
        expect(page.locator('#notice')).to_have_text('Action completed.')


def withdraw_release(origin, password, release_id):
    with console_page(origin, password) as page:
        dialog = page.get_by_role('dialog')
        page.get_by_role('button', name='Delivery fixture', exact=True).click()
        dialog.get_by_role('button', name='List Releases', exact=True).click()
        dialog.get_by_role('button', name='Show results', exact=True).click()
        dialog.get_by_role('row').filter(has_text=release_id).get_by_role('button').click()
        dialog.get_by_role('button', name='Withdraw Release', exact=True).click()
        dialog.get_by_label('Reason *', exact=True).fill('CI withdrawal drill')
        dialog.get_by_role('checkbox').check()
        dialog.get_by_role('button', name='Apply withdraw release', exact=True).click()
        expect(dialog.locator('dl.properties').first).to_be_visible()
        expect(page.locator('#notice')).to_have_text('Action completed.')
