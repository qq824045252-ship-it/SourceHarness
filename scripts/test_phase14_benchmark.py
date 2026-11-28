import argparse
import json
import os
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from phase14_benchmark import ScriptedGateway, run_case
from phase14_benchmark_cases import SHAPES, apply, corpus, manifest
from phase14_checker import markers, signature


class CorpusTest(unittest.TestCase):
    def test_unique_content_and_fixed_shape_totals(self):
        plan = manifest(list(corpus()))
        self.assertEqual(plan, manifest(list(corpus())))
        self.assertEqual(len(plan), 100)
        self.assertEqual(len({row["sha256"] for row in plan}), 100)
        counts = {row["category"]: 0 for row in plan}
        for row in plan:
            counts[row["category"]] += 1
        self.assertEqual(counts, {"atomic": 25, "parallel": 25, "branch": 25, "worker": 25})
        self.assertEqual(sum(count for _, _, count in (shape for group in SHAPES.values() for shape in group)), 100)

    def test_every_case_declares_checks_and_actions(self):
        for case in corpus():
            self.assertTrue(case.checks, case.id)
            self.assertTrue(case.actions, case.id)
            kinds = {action["kind"] for action in case.actions}
            self.assertTrue(kinds <= {"agent", "branch", "spawn", "merge"}, case.id)
            self.assertIn("agent", kinds, case.id)
            for action in case.actions:
                if action["kind"] == "agent":
                    self.assertTrue(action["script"], case.id)
                    self.assertTrue({"workspace", "any_of"} & set(action["expect"]), case.id)

    def test_anchor_replacement_is_strict(self):
        self.assertEqual(apply("a b", [("a", "c")]), "c b")
        with self.assertRaises(ValueError):
            apply("a a", [("a", "c")])


class CheckerTest(unittest.TestCase):
    def test_signature_rejects_stale_arity(self):
        files = {"pkg/api.py": b"def compute(x, scale=1):\n    return x\n",
                 "pkg/caller.py": b"from pkg.api import compute\n\n\ndef run(v):\n    return compute(v)\n"}
        mode = {"kind": "signature", "file": "pkg/api.py", "function": "compute", "params": ["x", "scale"],
                "callers": {"pkg/caller.py": 2}}
        self.assertTrue(signature(files, mode))
        fixed = {**files, "pkg/caller.py": files["pkg/caller.py"].replace(b"compute(v)", b"compute(v, scale=1)")}
        self.assertFalse(signature(fixed, mode))

    def test_markers_report_missing_needles(self):
        files = {"a.py": b"if item > 0:\n"}
        self.assertFalse(markers(files, {"files": {"a.py": ["if item > 0"]}}))
        self.assertTrue(markers(files, {"files": {"a.py": ["if item > 1"]}}))
        self.assertTrue(markers(files, {"files": {"b.py": ["x"]}}))


@unittest.skipUnless(os.environ.get("SOURCEHARNESS_QWEN_TEST_CLI"), "compiled CLI optional")
class SmokeTest(unittest.TestCase):
    def run_ids(self, identifiers):
        args = argparse.Namespace(exe=Path(os.environ["SOURCEHARNESS_QWEN_TEST_CLI"]).resolve(),
                                  model="qwen3.8-flash", driver="scripted", action_timeout=120)
        cases = {case.id: case for case in corpus()}
        rows = []
        gateway = ScriptedGateway(args.model)
        with TemporaryDirectory() as directory, gateway.serve() as proxy:
            for identifier in identifiers:
                rows.append(run_case(args, cases[identifier], Path(directory), gateway, proxy))
        self.assertIsNone(gateway.failure)
        for row in rows:
            self.assertTrue(row["passed"], json.dumps(row["actions"], indent=2)[:4000])
        return rows

    def test_atomic_cases(self):
        self.run_ids(["atomic.atomic-rename.00", "atomic.atomic-abort.00", "atomic.atomic-rollback.00"])

    def test_parallel_and_branch_cases(self):
        self.run_ids(["parallel.validate-fanout.00", "parallel.write-barrier.00",
                      "branch.branch-rewind.00", "branch.branch-green.00"])

    def test_worker_cases(self):
        self.run_ids(["worker.worker-disjoint.00", "worker.worker-shared-file.00",
                      "worker.worker-write-skew.00", "worker.worker-conflict.00"])


if __name__ == "__main__":
    unittest.main()
