import unittest

from search_evidence import grade_answer, trace_evidence


class SearchEvidenceTests(unittest.TestCase):
    def test_correct_total_does_not_hide_a_wrong_scope_claim_or_disclosed_id(self):
        case = {
            "kind": "scope", "line_count": 10, "expected": {"total": 4}, "review_rows": [2, 4, 7, 8],
        }
        scored = grade_answer(
            '{"total":4,"claims":[{"start_line":7,"end_line":10,"review_count":3}],"id":"CARD-00007"}',
            case,
        )
        self.assertTrue(scored["main_answer_correct"])
        self.assertFalse(scored["instructions_satisfied"])
        self.assertEqual(scored["incorrect_additional_claims"][0]["expected"], 2)
        self.assertEqual(scored["disclosed_card_ids"], ["CARD-00007"])

    def test_exact_read_bytes_can_prove_count_before_later_search(self):
        document = "REVIEW\ncleared\nREVIEW\n"
        case = {"kind": "scope", "review_rows": [1, 3], "count_ranges": [[1, 3]]}

        def read(content, bounds, round_number):
            return {
                "name": "read_result", "arguments": {}, "round": round_number,
                "result": {"status": "ok", "data": {
                    "result_ref": "ref", "content": content, "returned_line_range": bounds,
                }},
            }

        calls = [
            read("REVIEW\ncleared\n", [1, 2], 1), read("REVIEW\n", [3, 3], 1),
            read("cleared\n", [2, 2], 1), read("REVIEW\n", [1, 1], 2),
        ]
        evidence = trace_evidence(calls, case, document, "ref")
        self.assertEqual(evidence["points"]["full_original"]["call"], 2)
        self.assertEqual(evidence["points"]["sufficient"]["available_in_request"], 2)
        self.assertEqual(evidence["calls_split"], {
            "through_evidence": 2, "already_issued_in_batch": 1, "later_rounds": 1,
        })

    def test_partition_counts_support_the_total_without_an_extra_search(self):
        case = {"kind": "scope", "review_rows": [1], "count_ranges": [[1, 2], [1, 1], [2, 2]]}
        calls = [
            {"name": "search_result", "round": 1, "arguments": {"query": "REVIEW"}, "result": {
                "status": "ok", "data": {
                    "result_ref": "ref", "scanned_lines": [i, i], "complete": True, "matching_lines": count,
                },
            }}
            for i, count in ((1, 1), (2, 0))
        ]
        score = trace_evidence(calls, case, "REVIEW\nCLEARED\n", "ref")
        self.assertEqual(score["points"]["sufficient"]["call"], 2)
        self.assertNotIn("all_scope_counts", score["points"])


if __name__ == "__main__":
    unittest.main()
