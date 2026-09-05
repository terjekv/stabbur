#!/usr/bin/env python3
"""Require green main CI and complete macOS acceptance before publishing an image."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess

REPOSITORIES = ('stabbur', 'stabbur-client-rust', 'stabbur-cli', 'stabbur-frontend')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def api(path):
    return json.loads(subprocess.check_output(['gh', 'api', path], timeout=60))


def validate_report(report, expected_sha):
    require(report.get('schema_version') == 1, 'Unknown acceptance schema')
    require(report.get('result') == 'passed' and report.get('stage') == 'complete',
            'Acceptance did not complete successfully')
    require(report.get('installation_requested') is True, 'Installation evidence is required')
    sources = report.get('sources', {})
    for name in REPOSITORIES:
        source = sources.get(name, {})
        require(re.fullmatch(r'[0-9a-f]{40}', str(source.get('commit'))) is not None,
                f'Missing exact revision for {name}')
        require(source.get('dirty') is False, f'Dirty source tree: {name}')
    require(sources['stabbur']['commit'] == expected_sha, 'Acceptance is for another server revision')
    return {name: sources[name]['commit'] for name in REPOSITORIES}


def validate_run(run, sha, workflow):
    require(run.get('head_sha') == sha and run.get('head_branch') == 'main',
            'Run is for another revision or branch')
    require(run.get('event') in ('push', 'workflow_dispatch'), 'Run must use trusted main source')
    require(run.get('path') == f'.github/workflows/{workflow}', 'Unexpected workflow')
    require(run.get('status') == 'completed' and run.get('conclusion') == 'success',
            'Workflow has not passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--acceptance-run-id', required=True)
    parser.add_argument('--evidence-dir', type=Path, required=True)
    args = parser.parse_args()
    require(re.fullmatch(r'[1-9][0-9]{0,19}', args.acceptance_run_id) is not None, 'Invalid run ID')
    require(os.environ['GITHUB_REPOSITORY'] == 'terjekv/stabbur', 'Unexpected repository')
    require(os.environ['GITHUB_REF'] == 'refs/heads/main', 'Publication requires main')
    sha = os.environ['GITHUB_SHA']
    require(re.fullmatch(r'[0-9a-f]{40}', sha) is not None, 'Invalid server revision')
    run = api(f'repos/terjekv/stabbur/actions/runs/{args.acceptance_run_id}')
    validate_run(run, sha, 'workspace-integration.yml')
    args.evidence_dir.mkdir(parents=True, exist_ok=False)
    subprocess.run(['gh', 'run', 'download', args.acceptance_run_id, '--repo', 'terjekv/stabbur',
                    '--name', 'macos-single-host-evidence', '--dir', str(args.evidence_dir)],
                   check=True, timeout=60)
    report_file = args.evidence_dir / 'single-host-evidence.json'
    require(report_file.stat().st_size < 65536, 'Acceptance report is oversized')
    sources = validate_report(json.loads(report_file.read_text()), sha)
    ci_runs = {}
    for name, revision in sources.items():
        runs = api(f'repos/terjekv/{name}/actions/workflows/ci.yml/runs?'
                   f'head_sha={revision}&branch=main&event=push&per_page=100')['workflow_runs']
        require(bool(runs), f'No main CI evidence for {name}')
        latest = max(runs, key=lambda item: item['run_number'])
        validate_run(latest, revision, 'ci.yml')
        ci_runs[name] = latest['html_url']
    evidence = {'schema_version': 1, 'sources': sources, 'ci_runs': ci_runs,
                'acceptance_run': run['html_url']}
    (args.evidence_dir / 'publication-inputs.json').write_text(json.dumps(evidence, indent=2) + '\n')
    with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
        output.write(f"client_sha={sources['stabbur-client-rust']}\n")
    print('All four source revisions passed CI; exact server revision passed macOS installation acceptance.')


if __name__ == '__main__':
    main()
