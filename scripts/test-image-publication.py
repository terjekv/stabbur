#!/usr/bin/env python3
"""Publication evidence must reject stale or incomplete acceptance."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('publication', Path(__file__).with_name('verify-image-publication.py'))
publication = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publication)
SHA = 'a' * 40


class PublicationEvidenceTests(unittest.TestCase):
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


if __name__ == '__main__':
    unittest.main()
