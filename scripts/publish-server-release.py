#!/usr/bin/env python3
"""Publish an immutable server source tag only after all image compatibility checks pass."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib


def validate(evidence, sha):
    if evidence.get('schema_version') != 1 or evidence.get('sources', {}).get('stabbur') != sha:
        raise ValueError('Publication evidence is for another server revision')
    if not re.fullmatch(r'ghcr\.io/terjekv/stabbur-server@sha256:[a-f0-9]{64}', evidence.get('image', '')):
        raise ValueError('Publication evidence has no immutable server image')
    if evidence.get('platform') != 'linux/amd64' or any(
            evidence.get(name + '_compatibility') != 'passed' for name in ('client', 'cli', 'console')):
        raise ValueError('All three public consumers must pass against the published image')
    return evidence['image']


def release_needed(tag):
    result = subprocess.run(['gh', 'api', f'repos/terjekv/stabbur/releases/tags/{tag}'],
                            capture_output=True, text=True, timeout=30)
    if result.returncode:
        if 'HTTP 404' in result.stderr:
            return True
        raise RuntimeError('Could not verify the published release')
    published = json.loads(result.stdout)
    if published.get('tag_name') != tag or published.get('draft') is not False:
        raise ValueError('The existing release is incomplete or identifies another version')
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--evidence', type=Path)
    mode.add_argument('--check-needed', action='store_true')
    args = parser.parse_args()
    if os.environ.get('GITHUB_REPOSITORY') != 'terjekv/stabbur' or os.environ.get('GITHUB_REF') != 'refs/heads/main':
        raise ValueError('Server release publication requires this repository main workflow')
    version = tomllib.loads(Path('Cargo.toml').read_text())['workspace']['package']['version']
    tag = 'v' + version
    if args.check_needed:
        needed = release_needed(tag)
        with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
            output.write('needed=' + str(needed).lower() + '\n')
        print('Version requires publication' if needed else 'Preserving the existing immutable release')
        return
    sha = os.environ['GITHUB_SHA']
    image = validate(json.loads(args.evidence.read_text()), sha)
    existing = subprocess.run(['gh', 'api', f'repos/terjekv/stabbur/git/ref/tags/{tag}'],
                              capture_output=True, text=True, timeout=30)
    if existing.returncode == 0:
        target = json.loads(existing.stdout)['object']
        if target['type'] == 'tag':
            target = json.loads(subprocess.check_output(
                ['gh', 'api', f"repos/terjekv/stabbur/git/tags/{target['sha']}"], timeout=30))['object']
        if target['type'] != 'commit' or target['sha'] != sha:
            raise ValueError('The release tag already identifies different immutable source')
        subprocess.run(['gh', 'release', 'view', tag, '--repo', 'terjekv/stabbur'], check=True, timeout=30)
        print('The immutable server release already exists')
        return
    if 'HTTP 404' not in existing.stderr:
        raise RuntimeError('Could not verify whether the release tag exists')
    with tempfile.TemporaryDirectory(prefix='stabbur-release-') as temporary:
        notes = Path(temporary) / 'notes.md'
        notes.write_text(f'Stabbur server {version}\n\nSource: `{sha}`\n\n'
                         f'Verified Linux amd64 image: `{image}`\n\n'
                         'Client, CLI, and console acceptance passed against this exact image. '
                         'The attached evidence records all four source revisions and CI runs.\n')
        subprocess.run(['gh', 'release', 'create', tag, '--repo', 'terjekv/stabbur',
                        '--target', sha, '--title', f'Stabbur server {version}', '--notes-file', str(notes),
                        str(args.evidence), 'docs/openapi.json', 'COMPATIBILITY.md'], check=True, timeout=120)


if __name__ == '__main__':
    main()
