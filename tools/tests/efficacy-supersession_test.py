#!/usr/bin/env python3
"""Public runner supersession contracts; not measured agent efficacy."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('runner', Path(__file__).resolve().parents[1] / 'lattice-codex-efficacy-runner.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
AUTH = 'repo_' + 'a' * 64
OLD = {'memory_id': 'old', 'repository_id': AUTH, 'content': 'Prior contract.', 'content_sha256': 'unused', 'expectation': 'absent'}
NEW = {'memory_id': 'new', 'repository_id': AUTH, 'content': 'Current contract.', 'content_sha256': runner.hashlib.sha256(b'Current contract.').hexdigest(), 'expectation': 'present'}
RECEIPT={'authority':'repository:'+AUTH,'delivery_id':'delivery-1','payload_hash':'sha256:'+'a'*64,'ack_required':True}

class Rpc:
    def __init__(self, wrong_edge=False, failed_apply=False):
        self.calls=[]; self.wrong_edge=wrong_edge; self.failed_apply=failed_apply
    def tool(self, name, args):
        self.calls.append((name,args))
        if args.get('action') == 'propose':
            return {'structuredContent': {'proposal_id': 'proposal', 'decision': 'pending'}}
        if args.get('action') == 'apply':
            return {'structuredContent': {'proposal_id': 'proposal', 'decision': 'pending' if self.failed_apply else 'applied'}}
        return {'structuredContent': {'conflicts': [{'source': f'memory:{AUTH}/'+('foreign' if self.wrong_edge else 'new'), 'target': f'memory:{AUTH}/old', 'link_type': 'supersedes'}]}}

class Supersession(unittest.TestCase):
    def test_public_mutations_and_scoped_relation_are_required(self):
        rpc=Rpc()
        with tempfile.TemporaryDirectory() as d:
            result=runner.supersede_captured_lesson(rpc, OLD, NEW, Path(d))
            self.assertEqual([x[0] for x in rpc.calls], ['remember','remember','status'])
            self.assertEqual(rpc.calls[0][1]['kind'], 'evolution')
            self.assertEqual(rpc.calls[0][1]['superseded_by_memory_id'], 'new')
            self.assertEqual(set(result), {'proposal','apply','conflicts'})
            self.assertEqual(len(list(Path(d).glob('*.json'))), 3)

    def test_cross_repository_and_self_relation_fail_before_mutation(self):
        for replacement in (dict(NEW, repository_id='repo_foreign'), dict(NEW, memory_id='old')):
            rpc=Rpc()
            with tempfile.TemporaryDirectory() as d, self.assertRaises(ValueError):
                runner.supersede_captured_lesson(rpc, OLD, replacement, Path(d))
            self.assertEqual(rpc.calls, [])

    def test_failed_apply_and_foreign_edge_are_not_accepted(self):
        for rpc in (Rpc(wrong_edge=True), Rpc(failed_apply=True)):
            with tempfile.TemporaryDirectory() as d, self.assertRaises(RuntimeError):
                runner.supersede_captured_lesson(rpc, OLD, NEW, Path(d))

    def test_primary_and_excluded_lesson_bind_same_payload(self):
        data={'memories': [{'memory_id': 'new', 'workspace_id': AUTH, 'content': NEW['content']}], 'memory_deliveries':[RECEIPT]}
        request={'expected_delivery': NEW, 'excluded_deliveries': [OLD]}
        evidence=runner.collect_delivery_evidence('recall', {'structuredContent': data}, request)
        self.assertEqual(len(evidence), 2)
        self.assertIs(evidence[0]['response_payload'], evidence[1]['response_payload'])
        self.assertTrue(evidence[1]['absent'])
        data['memories'].append({'memory_id': 'old', 'workspace_id': AUTH, 'content': OLD['content']})
        evidence=runner.collect_delivery_evidence('recall', {'structuredContent': data}, request)
        self.assertFalse(evidence[1]['absent'])

    def test_absence_checks_text_and_structured_containers_together(self):
        raw={'structuredContent': {'memories': [{'memory_id': 'new', 'workspace_id': AUTH, 'content': NEW['content']}], 'memory_deliveries':[RECEIPT]},
             'content': [{'type': 'text', 'text': json.dumps({'memories': [{'memory_id': 'old', 'workspace_id': AUTH, 'content': OLD['content']}]})}]}
        evidence=runner.collect_delivery_evidence('recall',raw,{'expected_delivery':NEW,'excluded_deliveries':[OLD]})
        self.assertEqual(len(evidence), 2)
        self.assertFalse(evidence[1]['absent'])
        self.assertIs(evidence[0]['raw_response_payload'], raw)
        self.assertIs(evidence[1]['raw_response_payload'], raw)

if __name__ == '__main__': unittest.main()
