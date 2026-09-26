#!/usr/bin/env python3
"""Publication evidence must reject stale or incomplete acceptance."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('publication', Path(__file__).with_name('verify-image-publication.py'))
publication = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publication)
release_spec = importlib.util.spec_from_file_location('release', Path(__file__).with_name('publish-server-release.py'))
release = importlib.util.module_from_spec(release_spec)
release_spec.loader.exec_module(release)
SHA = 'a' * 40


class PublicationEvidenceTests(unittest.TestCase):
    def test_released_versions_skip_publication_and_lookup_failures_stop(self):
        for response, expected in [
                (subprocess.CompletedProcess([], 1, '', 'HTTP 404'), True),
                (subprocess.CompletedProcess([], 0, json.dumps({'tag_name': 'v0.0.1', 'draft': False}), ''), False)]:
            with patch.object(release.subprocess, 'run', return_value=response):
                self.assertEqual(release.release_needed('v0.0.1'), expected)
        for response, error in [
                (subprocess.CompletedProcess([], 1, '', 'HTTP 403'), RuntimeError),
                (subprocess.CompletedProcess([], 0, json.dumps({'tag_name': 'v0.0.1', 'draft': True}), ''), ValueError),
                (subprocess.CompletedProcess([], 0, json.dumps({'tag_name': 'v0.0.2', 'draft': False}), ''), ValueError)]:
            with patch.object(release.subprocess, 'run', return_value=response), self.assertRaises(error):
                release.release_needed('v0.0.1')

    def report(self):
        return {'schema_version': 1, 'result': 'passed', 'stage': 'complete',
                'installation_requested': True,
                'sources': {name: {'commit': SHA, 'dirty': False} for name in publication.REPOSITORIES}}

    def test_complete_evidence_preserves_exact_revisions(self):
        report = self.report()
        report['sources']['stabbur-client-rust']['commit'] = 'b' * 40
        self.assertEqual(publication.validate_report(report, SHA)['stabbur-client-rust'], 'b' * 40)

    def test_incomplete_or_stale_evidence_is_rejected(self):
        valid = self.report()
        for field, value in [('schema_version', 2), ('result', 'failed'),
                             ('stage', 'installing'), ('installation_requested', False)]:
            report = copy.deepcopy(valid)
            report[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                publication.validate_report(report, SHA)
        for name in publication.REPOSITORIES:
            for field, value in [('commit', None), ('commit', 'main'), ('dirty', True), ('dirty', None)]:
                report = copy.deepcopy(valid)
                report['sources'][name][field] = value
                with self.subTest(name=name, field=field), self.assertRaises(ValueError):
                    publication.validate_report(report, SHA)
        with self.assertRaises(ValueError):
            publication.validate_report(valid, 'b' * 40)

    def test_run_must_match_trusted_main_revision_and_workflow(self):
        run = {'head_sha': SHA, 'head_branch': 'main', 'event': 'push', 'status': 'completed',
               'conclusion': 'success', 'path': '.github/workflows/workspace-integration.yml'}
        publication.validate_run(run, SHA, 'workspace-integration.yml')
        for field, value in [('head_sha', 'b' * 40), ('head_branch', 'topic'),
                             ('event', 'pull_request'), ('status', 'in_progress'),
                             ('conclusion', 'failure'), ('path', '.github/workflows/ci.yml')]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                publication.validate_run({**run, field: value}, SHA, 'workspace-integration.yml')

    def test_pending_ci_waits_but_failed_ci_is_rejected(self):
        completed = {'run_number': 1, 'head_sha': SHA, 'head_branch': 'main', 'event': 'push',
                     'status': 'completed', 'conclusion': 'success', 'path': '.github/workflows/ci.yml'}
        with patch.object(publication, 'api', side_effect=[{'workflow_runs': []}, {'workflow_runs': [completed]}]), \
                patch.object(publication.time, 'sleep'):
            self.assertEqual(publication.completed_ci('stabbur', SHA, 30), completed)
        with patch.object(publication, 'api', return_value={'workflow_runs': [{**completed, 'conclusion': 'failure'}]}), \
                self.assertRaises(ValueError):
            publication.completed_ci('stabbur', SHA, 30)
        with patch.object(publication, 'api', return_value={'workflow_runs': []}), self.assertRaises(ValueError):
            publication.completed_ci('stabbur', SHA, 0)

    def test_versioned_release_requires_all_consumers_and_exact_image_source(self):
        evidence = {'schema_version': 1, 'sources': {'stabbur': SHA}, 'platform': 'linux/amd64',
                    'image': 'ghcr.io/terjekv/stabbur-server@sha256:' + 'b' * 64,
                    **{name + '_compatibility': 'passed' for name in ('client', 'cli', 'console')}}
        self.assertEqual(release.validate(evidence, SHA), evidence['image'])
        for field, value in [('image', 'ghcr.io/terjekv/stabbur-server:latest'),
                             ('sources', {'stabbur': 'c' * 40}), ('platform', 'linux/arm64'),
                             ('client_compatibility', 'failed'), ('cli_compatibility', None),
                             ('console_compatibility', 'not requested')]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                release.validate({**evidence, field: value}, SHA)


if __name__ == '__main__':
    unittest.main()
