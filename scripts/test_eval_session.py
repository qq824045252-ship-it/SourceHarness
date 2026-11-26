import tempfile
import unittest
from pathlib import Path

from eval_session import request_cost
from session_eval_cases import BASE_B, grade, policy_case


class SessionGradingTests(unittest.TestCase):
    def test_wrong_values_in_evidence_fail_even_when_config_is_exact(self):
        case = policy_case(BASE_B)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, contents in case.expected.items():
                (root / name).write_bytes(contents)
            self.assertTrue(grade(root, case)["passed"])
            (root / "evidence.md").write_text("POLICY=POL-072\nTIMEOUT=999\nRETRIES=999\n", encoding="utf-8")
            result = grade(root, case)
            self.assertFalse(result["passed"])
            self.assertEqual(result["mismatched_paths"], [])
            self.assertFalse(result["evidence_exact"])

    def test_workspace_cannot_supply_a_replacement_oracle(self):
        case = policy_case(BASE_B)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, contents in case.expected.items():
                (root / name).write_bytes(contents)
            altered = case.expected["service.cfg"].replace(b"[archive]\ntimeout=30", b"[archive]\ntimeout=999")
            (root / "service.cfg").write_bytes(altered)
            (root / "fixture-original.txt").write_bytes(altered)
            self.assertFalse(grade(root, case)["passed"])
            self.assertIn("service.cfg", grade(root, case)["mismatched_paths"])

    def test_unknown_failed_attempt_includes_known_retry_cost(self):
        events = [
            {"seq": 1, "kind": "model_request", "data": {}},
            {"seq": 2, "kind": "model_error", "data": {"request": 1, "elapsed_ms": 12, "usage": None}},
            {"seq": 3, "kind": "model_request", "data": {}},
            {"seq": 4, "kind": "model_response", "data": {"request": 3, "elapsed_ms": 18, "usage": {"total_tokens": 100}}},
            {"seq": 5, "kind": "model_request", "data": {}},
        ]
        cost = request_cost(events)
        self.assertEqual(cost["known_usage"]["total_tokens"], 100)
        self.assertEqual(cost["attempts"], 3)
        self.assertEqual(cost["unknown_usage_attempts"], 2)
        self.assertEqual(cost["known_model_ms"], 30)
        self.assertEqual(cost["unknown_duration_attempts"], 1)

    def test_64_kib_target_is_in_middle(self):
        config = policy_case(BASE_B, large=True).inputs["service.cfg"]
        self.assertEqual(len(config), 65536)
        self.assertGreater(config.index(b"[primary]"), 32000)
        self.assertLess(config.index(b"[primary]"), 33000)


if __name__ == "__main__":
    unittest.main()
