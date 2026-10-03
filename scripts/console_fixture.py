"""Publish synthetic test bytes through the normal leased worker protocol.

This is browser contract evidence, not a real AutoPkg build or installable package.
All requests go to the harness-owned loopback server; credentials never leave memory.
"""
import base64
from datetime import datetime, timezone
import hashlib
import urllib.request
from urllib.parse import urlsplit


def publish_fixture(origin, authorization, http):
    assert urlsplit(origin).hostname == '127.0.0.1'

    def call(path, body=None, headers=None, expected=200):
        status, _, result = http(origin, path, body, authorization if headers is None else headers)
        if status != expected:
            raise AssertionError(f'console publication fixture: expected {expected}, received {status}')
        return result

    call('/api/v1/software', {'slug':'console-delivery','name':'Console delivery fixture'}, expected=201)
    recipe = call('/api/v1/recipes', {'name':'console-delivery'}, expected=201)
    pin = {'url':'https://example.test/recipes.git','commit':'a' * 40}
    revision = call('/api/v1/recipes/' + recipe['id'] + '/revisions', {
        'builder':'autopkg','required_capabilities':[],
        'definition':{'sources':[pin],'entrypoint':'Fixture.pkg.recipe','output':{
            'version_pointer':'/version','recipe_trust_pointer':'/stabbur/recipe_trust_succeeded',
            'variants':[{'platform':'mac_os','architecture':'universal','minimum_macos':'13.0',
                         'maximum_macos':None,'resolution_priority':0,'artifacts':[{
                             'path_pointer':'/artifact_path','media_type':'application/octet-stream','role':'primary_installer'}]}],
            'verification':[{'name':'fixture','pointer':'/fixture_valid','required':True}]}}}, expected=201)
    credential = call('/api/v1/workers', {'name':'console-fixture-worker','allowed_capabilities':['os.macos','builder.autopkg']}, expected=201)
    worker_auth = {'authorization':'Bearer ' + credential['token']}
    prefix = '/api/v1/internal/workers/' + credential['worker_id']
    call('/api/v1/internal/workers/register', {'worker_id':credential['worker_id'],'capabilities':['os.macos','builder.autopkg']}, worker_auth,204)
    catalog = {'schema_version':1, 'producer':'autopkg',
        'source':{'locator':'stabbur-worker:' + credential['worker_id'] + ':autopkg', 'revision':'browser-fixture'},
        'recipes':[
            {'identifier':'example.download.ImportedApp','guidance':{'name':'ImportedApp','purpose':'fetch_artifact'},'builder':'autopkg','parents':[],
             'required_capabilities':['builder.autopkg','os.macos'],
             'import_sources':[{'locator':pin['url'],'revision':pin['commit']}]},
            {'identifier':'example.override.Uncommitted','guidance':{'name':'Uncommitted','purpose':'fetch_artifact'},'builder':'autopkg','parents':['example.download.ImportedApp'],
             'required_capabilities':['builder.autopkg','os.macos']}],
        'diagnostics':[{'identifier':'example.override.Uncommitted','code':'unpinned_source','severity':'error',
                        'detail':'Commit and publish this override before importing.'}]}
    call(prefix + '/recipe-catalogs', catalog, worker_auth, 201)
    run = call('/api/v1/runs', {'software':'console-delivery','recipe_revision':revision['id'],'parameters':{}},
               {**authorization,'idempotency-key':'console-published-run'},201)
    claimed = call(prefix + '/claim', {'lease_seconds':60},worker_auth)
    assert claimed['job']['payload']['request']['run_id'] == run['id']
    for batch in range(3):
        entries = [{'stream':'stdout','message_base64':base64.b64encode(f'Fixture log {i:03d}\n'.encode()).decode()}
                   for i in range(batch * 80,(batch + 1) * 80)]
        call(prefix + '/logs', {'lease':claimed['lease'],'idempotency_key':f'fixture-logs-{batch}','entries':entries},worker_auth,201)
    content = b'xar!Stabbur browser fixture bytes; not an installable package.'
    digest = hashlib.sha256(content).hexdigest()
    upload = urllib.request.Request(origin + prefix + '/attempts/' + claimed['lease']['attempt_id'] + '/artifacts/' + digest,
        method='PUT',data=content,headers={**worker_auth,'content-type':'application/octet-stream','x-stabbur-artifact-role':'primary_installer'})
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(upload,timeout=15) as response:
        assert response.status == 201
    now = datetime.now(timezone.utc).isoformat()
    completion = call(prefix + '/complete', {'lease':claimed['lease'],'idempotency_key':'fixture-complete','result':{
        'schema_version':1,'run_id':run['id'],'adapter':'autopkg','sources':[],'tools':{},
        'raw_report':{'version':'fixture-01','stabbur':{'recipe_trust_succeeded':True},'fixture_valid':True,'artifact_path':'fixture.pkg'},
        'build_result':{'discovered_version':'fixture-01','variants':[{
            'variant_id':None,'platform':'mac_os','architecture':'universal','minimum_macos':'13',
            'maximum_macos':None,'resolution_priority':0,'artifacts':[{'digest':digest,'size':len(content),'role':'primary_installer'}]}],
            'uploaded_artifacts':[digest],'provenance':{'builder':'console-fixture','worker_version':'0.0.1',
                'operating_system':'fixture','tools':{},'sources':[],'recipe_trust_succeeded':True,
                'raw_report':{'summary':'synthetic browser fixture'},'captured_at':now},
            'verification_results':[{'check':'fixture','required':True,'succeeded':True,'detail':None}]},'completed_at':now}},worker_auth,201)
    assert completion['disposition'] == 'release_created'
    return run['id']
