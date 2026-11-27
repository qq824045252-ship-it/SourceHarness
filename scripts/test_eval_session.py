import tempfile
import unittest
import json
from pathlib import Path

from eval_natural_session import resumable_transport_failure
from eval_session import feedback_audit, request_cache, request_cost
from session_eval_cases import BASE_B, build_case, grade, policy_case


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

    def test_natural_session_retries_transport_and_preserves_guard_stops(self):
        self.assertTrue(resumable_transport_failure({"exit": None, "error": "Evaluation process exceeded 240 seconds"}))
        self.assertTrue(resumable_transport_failure({"exit": 1, "error": "HTTP 502: Bad Gateway. The run can be resumed."}))
        self.assertFalse(resumable_transport_failure({"exit": -9, "error": ""}))
        self.assertFalse(resumable_transport_failure({"exit": 1, "error": "Loop guard paused the run."}))
        self.assertFalse(resumable_transport_failure({"exit": 1, "error": "Turn limit. The run can be resumed."}))

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

    def test_nested_deliverable_is_graded_at_the_requested_path(self):
        case = policy_case(BASE_B, report_path="deliverables/release/evidence.md")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, contents in case.expected.items():
                if name != case.report_path:
                    (root / name).write_bytes(contents)
            self.assertFalse(grade(root, case)["passed"])
            output = root / case.report_path
            output.parent.mkdir(parents=True)
            output.write_bytes(case.expected[case.report_path])
            self.assertTrue(grade(root, case)["passed"])

    def test_request_cache_separates_missing_telemetry_from_zero_hits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            blobs = root / ".sourceharness/sessions/work/blobs"
            blobs.mkdir(parents=True)
            (blobs / "hit").write_text('{"usage":{"input_tokens_details":{"cached_tokens":64}}}', encoding="utf8")
            (blobs / "miss").write_text('{"usage":{"input_tokens_details":{"cached_tokens":0}}}', encoding="utf8")
            events = [
                {"kind": "model_request", "seq": i, "data": {}} for i in range(1, 4)
            ] + [
                {"kind": "model_response", "data": {"request": 1, "raw_response": "hit"}},
                {"kind": "model_rejected", "data": {"request": 2, "raw_response": "miss"}},
                {"kind": "model_error", "data": {"request": 3}},
            ]
            self.assertEqual(request_cache(root, events), {
                "known_cached_input_tokens": 64, "requests_with_cache_usage": 2, "requests_without_cache_usage": 1,
            })

    def test_equivalent_cli_implementation_passes_and_wrong_behavior_fails(self):
        case = build_case()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, raw in case.expected.items():
                (root / name).write_bytes(raw)
            (root / "evidence.md").write_bytes(b"SYMBOL=render_count\nOUTPUT=count=3\n")
            implementation = root / "formatter.py"
            implementation.write_bytes(b'def render_count(total):\n    return "count=" + str(total)\n')
            self.assertTrue(grade(root, case)["passed"])
            implementation.write_bytes(b'def render_count(total):\n    return "count=3"\n')
            result = grade(root, case)
            self.assertFalse(result["passed"])
            self.assertEqual(result["mismatched_paths"], [])
            self.assertIn("render_count(0)", result["behavior_errors"][0])

    def test_feedback_checks_actual_wire_receipt_and_reread(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            blobs = root / ".sourceharness/sessions/work/blobs"
            blobs.mkdir(parents=True)
            source = {"kind": "workspace", "path": "config.txt"}
            raw = {"status": "ok", "source": source, "outcome": "committed", "diff": "-old\n+new", "revision": "r2"}
            delivered = {key: raw[key] for key in ("status", "source", "outcome", "diff")}
            (blobs / "raw").write_text(json.dumps(raw), encoding="utf8")
            (blobs / "request").write_text(json.dumps({
                "tools": [], "input": [{"role": "user", "content": "Do the task"},
                    {"type": "function_call_output", "call_id": "call", "output": json.dumps(delivered)}],
            }), encoding="utf8")
            events = [
                {"kind": "tool_result", "data": {"status": "ok", "name": "edit", "operation": "op",
                    "call_id": "call", "output": "raw", "source": source}},
                {"kind": "model_request", "data": {"input": "request"}},
                {"kind": "tool_result", "data": {"status": "ok", "name": "read", "source": source}},
            ]
            result = feedback_audit(root, events, "diff")
            self.assertTrue(result["verified_delivery"])
            self.assertTrue(result["isolated_receipt_exposure"])
            self.assertEqual(result["post_mutation_reads"], 1)
            self.assertFalse(feedback_audit(root, events, "versioned")["verified_delivery"])
            converted = json.loads((blobs / "request").read_bytes())
            converted["instructions"] = "Related facts\n" + json.dumps(
                {"edit_receipt": [{"data": {"operation": "op", **raw}}]}
            )
            converted["input"].append({
                "type": "function_call_output", "call_id": "history",
                "output": json.dumps({"content": json.dumps({"operation": "op", **raw})}),
            })
            converted["input"].append({
                "type": "function_call_output", "call_id": "reread",
                "output": json.dumps({"source": source, "revision": "r2", "content": "new"}),
            })
            (blobs / "request").write_text(json.dumps(converted), encoding="utf8")
            contaminated = feedback_audit(root, events, "diff")
            self.assertTrue(contaminated["verified_delivery"])
            self.assertFalse(contaminated["isolated_receipt_exposure"])
            self.assertEqual(contaminated["side_channels"], [
                {"request": 2, "channel": channel, "operation": "op", "fields": ["diff", "revision"],
                 "outside_direct_mode": ["revision"]}
                for channel in ("instructions", "other_tool_output")
            ])


if __name__ == "__main__":
    unittest.main()
