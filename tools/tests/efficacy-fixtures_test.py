"""Contract tests for the versioned efficacy fixture manifest."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    "fixtures", Path(__file__).resolve().parents[1] / "lattice_efficacy_fixtures.py"
)
fixtures = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixtures)


def grade(task, content):
    with tempfile.TemporaryDirectory() as directory:
        submission = Path(directory) / task["allowed_files"][0]
        submission.write_text(content)
        result = subprocess.run(
            [sys.executable, "-I", "-c", task["grader"], str(submission)],
            check=True,
            capture_output=True,
            text=True,
        )
    return json.loads(result.stdout)


def load_submission(task):
    directory = tempfile.TemporaryDirectory()
    path = Path(directory.name) / task["allowed_files"][0]
    path.write_text(task["reference_files"][path.name])
    spec = importlib.util.spec_from_file_location(task["id"], path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return directory, module


class EfficacyFixtureTests(unittest.TestCase):
    def test_manifest_is_versioned_and_has_complete_dict_records(self):
        self.assertEqual(fixtures.FIXTURE_VERSION, "v2")
        self.assertEqual(len(fixtures.TASKS), 7)
        required = {
            "id", "problem", "source", "allowed_files", "seed",
            "target_decision_id", "base_files", "reference_files", "grader",
        }
        for task in fixtures.TASKS:
            self.assertTrue(required.issubset(task))
            self.assertEqual(set(task) - required, {"predecessor"} if task["id"] == "branch-policy-supersession" else set())
            self.assertIsInstance(task["target_decision_id"], str)
            self.assertTrue(task["target_decision_id"])
            self.assertIsInstance(task["allowed_files"], list)
            self.assertEqual(len(task["allowed_files"]), 1)
        successor = next(task for task in fixtures.TASKS if task["id"] == "branch-policy-supersession")
        predecessor = successor["predecessor"]
        self.assertEqual(set(predecessor), required)
        self.assertEqual(predecessor["id"], "branch-policy-supersession-v1")
        self.assertLessEqual(len(successor["seed"].encode()), 512)

    def test_public_contracts_state_input_shapes_and_control_lacks_seed(self):
        for identifier, contract in fixtures.CONTRACTS.items():
            self.assertIn("Implement", contract, identifier)
            self.assertTrue("reject" in contract.lower() or "raise" in contract.lower(), identifier)
        self.assertIn("list of dict records", fixtures.CONTRACTS["scope-before-candidate-budget"])
        self.assertIn("`state` is a dict", fixtures.CONTRACTS["retry-durable-idempotency"])
        self.assertIn("path-like existing directory", fixtures.CONTRACTS["path-boundary"])
        self.assertIn("list of dict records", fixtures.CONTRACTS["stale-contradiction-trust"])
        self.assertIn("list of two-item", fixtures.CONTRACTS["transactional-changed-state"])
        self.assertIn("including nonstrings", fixtures.CONTRACTS["unfamiliar-control"])
        self.assertIn("no seeded analogue", fixtures.CONTRACTS["unfamiliar-control"])

    def test_reference_passes_and_bases_observe_named_seeded_mistake(self):
        for task in fixtures.TASKS:
            filename = task["allowed_files"][0]
            base = grade(task, task["base_files"][filename])
            reference = grade(task, task["reference_files"][filename])
            for result in (base, reference):
                self.assertEqual(result["assessment"], "assessed", task["id"])
                self.assertEqual(result["target_decision_id"], task["target_decision_id"])
                self.assertIsInstance(result["checks"], dict)
                self.assertTrue(all(isinstance(value, bool) for value in result["checks"].values()))
            self.assertTrue(reference["correct"], task["id"])
            self.assertFalse(reference["mistake_recurrence"], task["id"])
            if task["id"] == "unfamiliar-control":
                self.assertFalse(base["mistake_recurrence"])
            else:
                self.assertTrue(base["mistake_recurrence"], task["id"])

    def test_fixed_target_with_unrelated_regression_is_incorrect_without_recurrence(self):
        task = next(task for task in fixtures.TASKS if task["id"] == "scope-before-candidate-budget")
        filename = task["allowed_files"][0]
        mutant = task["reference_files"][filename].replace(
            "if budget<=0: return []", "if budget<=0: return records[:]"
        )
        result = grade(task, mutant)
        self.assertFalse(result["correct"])
        self.assertFalse(result["mistake_recurrence"])
        self.assertTrue(result["checks"][task["target_decision_id"]])
        self.assertFalse(result["checks"]["nonpositive_budget"])

    def test_policy_predecessor_and_successor_references_and_mutant_are_honest(self):
        successor = next(task for task in fixtures.TASKS if task["id"] == "branch-policy-supersession")
        predecessor = successor["predecessor"]
        for task in (predecessor, successor):
            filename = task["allowed_files"][0]
            base = grade(task, task["base_files"][filename])
            reference = grade(task, task["reference_files"][filename])
            self.assertFalse(base["correct"], task["id"])
            self.assertTrue(base["mistake_recurrence"], task["id"])
            self.assertEqual(base["target_decision_id"], task["target_decision_id"])
            self.assertTrue(reference["correct"], task["id"])
            self.assertFalse(reference["mistake_recurrence"], task["id"])
        filename = successor["allowed_files"][0]
        mutant = successor["reference_files"][filename].replace(
            " and row['exportable'] and not row['archived']", " and not row['archived']"
        )
        result = grade(successor, mutant)
        self.assertFalse(result["correct"])
        self.assertFalse(result["mistake_recurrence"])
        self.assertTrue(result["checks"][successor["target_decision_id"]])
        self.assertFalse(result["checks"]["other_filter"])

    def test_policy_grader_detects_input_row_mutation(self):
        task = next(task for task in fixtures.TASKS if task["id"] == "branch-policy-supersession")
        filename = task["allowed_files"][0]
        mutant = task["reference_files"][filename].replace(
            " return [row for row in records", " records[0]['id']='mutated'; return [row for row in records"
        )
        result = grade(task, mutant)
        self.assertFalse(result["correct"])
        self.assertTrue(result["checks"][task["target_decision_id"]])
        self.assertFalse(result["checks"]["input_unchanged"])

    def test_unloadable_submission_is_unassessable_not_observed_recurrence(self):
        task = fixtures.TASKS[0]
        result = grade(task, "def select(:\n")
        self.assertEqual(result, {
            "assessment": "unassessable",
            "correct": False,
            "mistake_recurrence": False,
            "target_decision_id": task["target_decision_id"],
            "checks": {"submission_loadable": False},
        })

    def test_references_reject_their_documented_invalid_input_shapes(self):
        tasks = {task["id"]: task for task in fixtures.TASKS}
        directory, module = load_submission(tasks["scope-before-candidate-budget"])
        with directory, self.assertRaises(ValueError):
            module.select([], "repo", False)
        directory, module = load_submission(tasks["retry-durable-idempotency"])
        with directory, self.assertRaises(ValueError):
            module.capture({}, "", "payload")
        directory, module = load_submission(tasks["path-boundary"])
        with directory, self.assertRaises(ValueError):
            module.resolve(None, "file")
        directory, module = load_submission(tasks["stale-contradiction-trust"])
        with directory, self.assertRaises(ValueError):
            module.choose([{"score": float("nan"), "status": "verified"}])
        directory, module = load_submission(tasks["transactional-changed-state"])
        with directory, self.assertRaises(ValueError):
            module.apply({}, [("value", True)])
        directory, module = load_submission(tasks["unfamiliar-control"])
        with directory, self.assertRaises(ValueError):
            module.parse(None)
        directory, module = load_submission(tasks["branch-policy-supersession"])
        with directory, self.assertRaises(ValueError):
            module.export_rows([
                {"id": "row", "tenant": "acme", "archived": False,
                 "exportable": True, "unexpected": True}
            ], "acme", 1)


if __name__ == "__main__":
    unittest.main()
