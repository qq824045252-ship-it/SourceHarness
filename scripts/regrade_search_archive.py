"""Re-score archived four-arm traces without running a new model request."""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re

from session_eval_cases import sha

CARD_STATUS = re.compile(r"card ([\da-f]{8}): REVIEW", re.I)
LINE_RANGE = re.compile(r"(?<!\d)(\d+)\s*[-–—]\s*(\d+)(?!\d)")


def fnv1a(raw):
    value = 0xcbf29ce484222325
    for byte in raw:
        value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    return f"{value:016x}"


def original_bytes(state, data, total_lines):
    if "returned_line_range" in data:
        start, end = data["returned_line_range"]
        content = data["content"].splitlines(keepends=True)
        if len(content) == end - start + 1 and 1 <= start <= end <= total_lines:
            state["lines"].update(zip(range(start, end + 1), content))
        if len(state["lines"]) == total_lines:
            return "".join(state["lines"][line] for line in range(1, total_lines + 1)).encode()
        return None
    if "chunk" in data:
        start, end = data["offset"], data["end_offset"]
        raw = data["chunk"].encode()
        if len(raw) == end - start:
            state["chunks"][start] = raw
            if data["end_of_result"]:
                state["end_byte"] = end
        content = bytearray()
        for offset, chunk in sorted(state["chunks"].items()):
            if offset > len(content):
                break
            content.extend(chunk[len(content) - offset:])
        if state["end_byte"] == len(content):
            return bytes(content)
    return None


def known_matches(row):
    found = {}
    reference = row["first"]["result_ref"]
    for call in row["calls"]:
        if call["args"].get("result_ref") != reference or call["result"]["status"] != "ok":
            continue
        data = call["result"]["data"]
        samples = [(hit["line"], hit["text"]) for hit in data.get("hits") or [] if not hit.get("partial")]
        if "returned_line_range" in data:
            samples.extend(enumerate(data["content"].splitlines(), data["returned_line_range"][0]))
        for line, content in samples:
            if match := CARD_STATUS.fullmatch(content):
                found[line] = match[1]
    return found


def table_claims(answer, row, verified_lines):
    """Check explicit Markdown table counts. Other prose remains unscored."""
    section_bounds = {entry["name"].lower(): entry["lines"] for entry in row["first"]["sections"]}
    result = []
    count_column = None
    for line in answer.splitlines():
        if not line.strip().startswith("|"):
            count_column = None
            continue
        cells = [part.strip().replace("*", "").strip("`") for part in line.strip().strip("|").split("|")]
        header = next((i for i, name in enumerate(cells) if name.lower() in ("review", "review cards", "review count", "# review")), None)
        if header is not None:
            count_column = header
            continue
        if count_column is None or count_column >= len(cells) or not cells[count_column].isdigit():
            continue
        ranges = LINE_RANGE.findall(" ".join(cells[:count_column] + cells[count_column + 1:]))
        if len(ranges) > 1:
            continue
        label = re.split(r"\s*[(\[]", cells[0], maxsplit=1)[0].strip().lower()
        named_bounds = section_bounds.get(label)
        bounds = [int(value) for value in ranges[0]] if ranges else named_bounds
        if bounds is None or not 1 <= bounds[0] <= bounds[1] <= row["first"]["total_lines"]:
            continue
        expected = sum(bounds[0] <= number <= bounds[1] for number in verified_lines)
        actual = int(cells[count_column])
        result.append({
            "label": cells[0], "range": bounds, "claimed": actual, "expected": expected,
            "correct": actual == expected and named_bounds in (None, bounds),
        })
    return result


def regrade(row, matches):
    total_lines = row["first"]["total_lines"]
    reference = row["first"]["result_ref"]
    gold = int(row["gold"][0])
    points = {}
    state = {"lines": {}, "chunks": {}, "end_byte": None}
    original = None
    for number, call in enumerate(row["calls"], 1):
        if call["args"].get("result_ref") != reference or call["result"]["status"] != "ok":
            continue
        data = call["result"]["data"]
        point = {"call": number, "issued_in_round": call["round"]}
        if call["name"] == "read_result" and original is None:
            raw = original_bytes(state, data, total_lines)
            if raw is not None and fnv1a(raw) == row["snapshot_fnv1a64"]:
                original = raw.decode().splitlines()
                points["full_original"] = point
                review = sum(bool(CARD_STATUS.fullmatch(line)) for line in original)
                if review == gold:
                    points["full_original_supports_gold"] = point
        if (call["name"] == "search_result" and data.get("complete") and data.get("matching_lines") == gold
                and data.get("scanned_lines") == [1, total_lines]):
            points.setdefault("matching_full_range_scalar", point)

    if row["first_enough_call"] is not None:
        points["archive_proof"] = {"call": row["first_enough_call"], "issued_in_round": row["first_enough_round"]}
    candidates = [point for key, point in points.items() if key in ("archive_proof", "full_original_supports_gold")]
    split = None
    if candidates:
        evidence = min(candidates, key=lambda point: point["call"])
        later = row["calls"][evidence["call"]:]
        same_batch = sum(call["round"] == evidence["issued_in_round"] for call in later)
        split = {"through_evidence": evidence["call"], "already_issued_in_batch": same_batch, "later_rounds": len(later) - same_batch}
    answer = row.get("answer") or ""
    explicit = bool(re.search(r"(without listing|do not list|no individual)\s+(?:individual\s+)?IDs", row["prompt"], re.I))
    observed_ids = set(re.findall(r"\b[\da-f]{8}\b", answer, re.I)) if explicit else set()
    ids = set(matches.values())
    if original is not None:
        ids.update(re.findall(r"\b[\da-f]{8}\b", "\n".join(original), re.I))
    confirmed = observed_ids & ids
    complete_locations = len(matches) == gold
    claims = table_claims(answer, row, matches) if complete_locations else []
    return {
        "case_id": row["case_id"], "arm": row["arm"], "calls": len(row["calls"]), "points": points,
        "split": split, "gold": gold, "explicit_no_ids": explicit,
        "confirmed_card_ids_in_answer": sorted(confirmed), "possible_card_ids_in_answer": sorted(observed_ids - confirmed),
        "all_review_locations_independently_matched": complete_locations,
        "table_claims_checked": claims, "other_additional_prose_facts": "unscored",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    raw = args.archive.read_bytes()
    json_blocks = re.findall(r"^```json\s*\n(.*?)\n```", raw.decode("utf-8-sig"), re.M | re.S)
    observations = [json.loads(block) for block in json_blocks]
    observations = [row for row in observations if {"first", "calls", "snapshot_fnv1a64"} <= row.keys()]
    if not observations:
        parser.error("No individual observations with snapshot fingerprints")
    matches = defaultdict(dict)
    for row in observations:
        matches[row["snapshot_fnv1a64"]].update(known_matches(row))
    results = [regrade(row, matches[row["snapshot_fnv1a64"]]) for row in observations]
    summary = {}
    for arm in sorted({row["arm"] for row in results}):
        rows = [row for row in results if row["arm"] == arm]
        summary[arm] = {
            "observations": len(rows),
            "points": dict(Counter(key for row in rows for key in row["points"])),
            "full_read_improves_first_proof": sum(
                row["split"] is not None and row["split"]["through_evidence"]
                < row["points"].get("archive_proof", {}).get("call", float("inf"))
                for row in rows
            ),
            "same_batch_calls_after_evidence": sum(row["split"]["already_issued_in_batch"] for row in rows if row["split"]),
            "new_round_calls_after_evidence": sum(row["split"]["later_rounds"] for row in rows if row["split"]),
            "answers_with_confirmed_forbidden_ids": sum(bool(row["confirmed_card_ids_in_answer"]) for row in rows),
            "answers_with_possible_forbidden_ids": sum(bool(row["possible_card_ids_in_answer"]) for row in rows),
            "checked_table_claims": sum(len(row["table_claims_checked"]) for row in rows),
            "answers_with_incorrect_table_claims": sum(
                any(not claim["correct"] for claim in row["table_claims_checked"]) for row in rows
            ),
        }
    result = {"source_archive_sha256": sha(raw), "archive": args.archive.name, "summary": summary, "observations": results}
    args.out.write_text(json.dumps(result, indent=2) + "\n", encoding="utf8")


if __name__ == "__main__":
    raise SystemExit(main())
