"""Independent checks for search answers and evidence in actual model requests."""

import json
import re
from collections import Counter, defaultdict


def same_json(actual, expected):
    return json.dumps(actual, sort_keys=True) == json.dumps(expected, sort_keys=True)


def grade_answer(answer, case):
    format_ok = True
    raw_json = False
    try:
        parsed = json.loads(answer)
        raw_json = True
    except ValueError:
        fenced = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", answer, re.S | re.I)
        format_ok = bool(fenced and not (answer[:fenced.start()] + answer[fenced.end():]).strip())
        try:
            parsed = json.loads(fenced[1]) if fenced else {}
        except ValueError:
            parsed = {}
    if not isinstance(parsed, dict):
        parsed, format_ok = {}, False
    checks = {
        key: key in parsed and same_json(parsed[key], value) for key, value in case["expected"].items()
    }
    extras = parsed.keys() - checks.keys()
    extra_wrong, extra_unscored = [], []
    review_rows = set(case.get("review_rows", []))
    for key in sorted(extras):
        value = parsed[key]
        if key != "claims" or not isinstance(value, list) or not review_rows:
            extra_unscored.append(key)
            continue
        for claim in value:
            if (
                isinstance(claim, dict) and set(claim) == {"start_line", "end_line", "review_count"}
                and all(type(n) is int for n in claim.values())
                and 1 <= claim["start_line"] <= claim["end_line"] <= case["line_count"]
            ):
                count = sum(claim["start_line"] <= line <= claim["end_line"] for line in review_rows)
                if claim["review_count"] != count:
                    extra_wrong.append({"claim": claim, "expected": count})
            else:
                extra_unscored.append(claim)
    ids_disclosed = sorted(set(re.findall(r"\bCARD-\d{5}\b", answer))) if case["kind"] == "scope" else []
    instruction_ok = format_ok and not extras and not ids_disclosed
    return {
        "required_fields": checks,
        "main_answer_correct": all(checks.values()),
        "instructions_satisfied": instruction_ok,
        "format_ok": format_ok,
        "raw_json": raw_json,
        "disclosed_card_ids": ids_disclosed,
        "incorrect_additional_claims": extra_wrong,
        "unscored_additional_content": extra_unscored + (["outside JSON"] if not format_ok and answer else []),
        "passed": all(checks.values()) and instruction_ok and not extra_wrong,
    }


def new_calls(previous_input, current_input, round_number):
    if current_input[:len(previous_input)] != previous_input:
        raise ValueError("Expected exact append-only tool replay")
    added = current_input[len(previous_input):]
    calls = {item["call_id"]: item for item in added if item.get("type") == "function_call"}
    for item in added:
        if item.get("type") != "function_call_output":
            continue
        call = calls[item["call_id"]]
        yield {
            "round": round_number, "call_id": item["call_id"], "name": call["name"],
            "arguments": parse_arguments(call["arguments"]), "result": json.loads(item["output"]),
        }


def parse_arguments(raw):
    try:
        value = json.loads(raw)
        return value if isinstance(value, dict) else {}
    except ValueError:
        return {}


def exact_lines(data, lines):
    start, end = data["returned_line_range"]
    if 1 <= start <= end <= len(lines) and data["content"] == "".join(lines[start - 1:end]):
        return set(range(start, end + 1))
    return set()


def scoped_count_proven(call, data, lines, review_rows):
    args = call["arguments"]
    query = args.get("query")
    start, end = data["scanned_lines"]
    flags = re.I if not args.get("case_sensitive", True) else 0
    try:
        pattern = re.compile(query if args.get("match_mode") == "regex" else re.escape(query), flags)
    except re.error:
        return False
    matched = {
        index for index in range(start, end + 1)
        if pattern.search(lines[index - 1].rstrip("\r\n"))
    }
    return matched == review_rows.intersection(range(start, end + 1)) and len(matched) == data["matching_lines"]


def aggregation_proven(data, expected_frequencies):
    groups = data.get("aggregation")
    if not groups or groups["value_counts_truncated"]:
        return False
    frequencies = {group["value"]: group["count"] for group in groups["value_counts"]}
    return (
        frequencies == expected_frequencies
        and groups["distinct_values"] == len(expected_frequencies)
        and groups["total_captured"] == sum(expected_frequencies.values())
    )


def count_ranges_proven(ranges, searched, read_lines):
    # Each interval fixes the difference between two prefix counts. Connected
    # boundaries prove a count by summing or subtracting verified intervals.
    adjacent = defaultdict(set)
    for start, end in searched | {(line, line) for line in read_lines}:
        adjacent[start - 1].add(end)
        adjacent[end].add(start - 1)
    for start, end in ranges:
        pending = [start - 1]
        seen = {start - 1}
        while pending and end not in seen:
            neighbors = adjacent[pending.pop()] - seen
            seen.update(neighbors)
            pending.extend(neighbors)
        if end not in seen:
            return False
    return True


def trace_evidence(calls, case, document, reference):
    lines = document.splitlines(keepends=True)
    all_lines = set(range(1, len(lines) + 1))
    ranges = {tuple(bounds) for bounds in case.get("count_ranges", [])}
    read_range = case.get("read_range")
    wanted_lines = set(range(read_range[0], read_range[1] + 1)) if read_range else all_lines
    review_rows = set(case.get("review_rows", []))
    needed_samples = set(case.get("sample_rows", []))
    sample_lines = set()
    covered, counts, expected_cursors = set(), set(), {}
    evidence = {}
    cursor_followed = 0
    sample_increases = 0
    searches = {}
    tool_errors = Counter()
    for number, call in enumerate(calls, 1):
        output = call["result"]
        if call["name"] == "search_result":
            args = call["arguments"]
            key = (
                args.get("result_ref"), args.get("query"), args.get("match_mode", "literal"),
                args.get("case_sensitive", True), args.get("start_line", 1), args.get("end_line", len(lines)),
            )
            samples = args.get("max_hits", 0)
            if type(samples) is int and all(isinstance(value, (str, int, type(None))) for value in key):
                if key in searches and call["round"] > searches[key][0] and samples > searches[key][1]:
                    sample_increases += 1
                if output["status"] == "ok":
                    searches[key] = call["round"], samples
        if output["status"] == "error":
            tool_errors[f'{output["code"]}/{output.get("reason")}'] += 1
            continue
        data = output["data"]
        if data["result_ref"] != reference:
            continue
        point = {"call": number, "issued_in_round": call["round"], "available_in_request": call["round"] + 1}
        if call["name"] == "read_result":
            returned = exact_lines(data, lines)
            if not returned:
                continue
            start = data["returned_line_range"][0]
            if expected_cursors.get(start, call["round"]) < call["round"]:
                cursor_followed += 1
            if next_start := data.get("next_start_line"):
                expected_cursors[next_start] = call["round"]
                evidence.setdefault("budget_pagination", point)
            covered.update(returned)
            if all_lines <= covered:
                evidence.setdefault("full_original", point)
            if read_range and wanted_lines <= covered:
                evidence.setdefault("requested_read_coverage", point)
        elif call["name"] == "search_result" and data["complete"]:
            limits = tuple(data["scanned_lines"])
            if case["kind"] == "scope" and scoped_count_proven(call, data, lines, review_rows):
                counts.add(limits)
                sample_lines.update(hit["line"] for hit in data.get("hits") or [] if hit["line"] in review_rows)
                evidence.setdefault("first_scope_count", point)
                if ranges <= counts:
                    evidence.setdefault("all_scope_counts", point)
            if case["kind"] == "group" and aggregation_proven(data, case["frequencies"]):
                evidence.setdefault("all_aggregation_values", point)
        if case["kind"] == "scope":
            if needed_samples <= sample_lines | covered and count_ranges_proven(ranges, counts, covered):
                evidence.setdefault("sufficient", point)
        elif case["kind"] == "pages" and "requested_read_coverage" in evidence:
            evidence.setdefault("sufficient", point)
        elif case["kind"] == "group" and ("full_original" in evidence or "all_aggregation_values" in evidence):
            evidence.setdefault("sufficient", point)
    split = None
    if sufficient := evidence.get("sufficient"):
        tail = calls[sufficient["call"]:]
        same_batch = sum(call["round"] == sufficient["issued_in_round"] for call in tail)
        split = {"through_evidence": sufficient["call"], "already_issued_in_batch": same_batch, "later_rounds": len(tail) - same_batch}
    return {
        "points": evidence, "calls_split": split, "errors": dict(tool_errors),
        "delivered_calls": len(calls), "lines_verified": len(covered),
        "read_target_lines_verified": len(wanted_lines & covered), "cursor_followed": cursor_followed,
        "sample_limit_increases_on_same_search": sample_increases,
    }
