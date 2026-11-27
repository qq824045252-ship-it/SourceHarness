"""Exercise the actual session loop with a rejected submission and archived evidence."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import time
from zipfile import ZipFile

from eval_session import read_events, request_cost
from session_eval_cases import sha
from session_wire_audit import request_body


ARCHIVE_PROMPT = (
    "Read every original line 44 through 179 of report.txt and follow the continuation to the end. "
    "This records the exact source for a later delivery. Leave all files as they are. "
    "After reading, reply with the range covered and do not quote file content."
)
REPAIR_PROMPT = (
    "Complete the archived audit delivery in submission.json. Its requirements are a single JSON object "
    "and the exact texts of lines 44, 117, and 179 from the report you audited in the preceding run. "
    "The live report has since moved to another revision. Keep report.txt unchanged. "
    "Run acceptance on the existing draft before making any correction. Then resolve the findings, "
    "preserve values that already satisfy the audit, and rerun acceptance on the final submission."
)


def parse_object(text):
    def unique_keys(pairs):
        result = dict(pairs)
        if len(result) != len(pairs):
            raise ValueError("duplicate JSON key")
        return result

    try:
        value = json.loads(text, object_pairs_hook=unique_keys)
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


def score_submission(path, expected):
    if not path.is_file():
        return {"issues": [{"code": "MISSING_SUBMISSION"}], "correct_fields": []}
    text = path.read_bytes().decode("utf-8", "replace")
    document = parse_object(text)
    issues = []
    if document is None:
        issues.append({"code": "EXPECTED_SINGLE_JSON_OBJECT"})
        fenced = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", text, flags=re.S | re.I)
        document = parse_object(fenced[1]) if fenced else None
    if document is None:
        return {"issues": issues, "correct_fields": []}
    correct = []
    for name, value in expected.items():
        if name not in document:
            issues.append({"code": "MISSING_FIELD", "field": name})
        elif document[name] != value:
            issues.append({"code": "ARCHIVED_VALUE_MISMATCH", "field": name})
        else:
            correct.append(name)
    if extra := sorted(document.keys() - expected.keys()):
        issues.append({"code": "UNEXPECTED_FIELDS", "fields": extra})
    return {"issues": issues, "correct_fields": correct}


def check(oracle):
    answer = score_submission(Path("submission.json"), json.loads(oracle.read_bytes()))
    print(json.dumps(answer, separators=(",", ":")))
    return int(bool(answer["issues"]))


def run_cli(args, root, config, prompt, label):
    command = [
        str(args.exe), "agent", "--root", str(root), "--session", "work", "--checks", str(config),
        "--model", args.model, "--effort", "low", "--history-mode", "facts",
        "--freshness-instructions", "off", "--stale-projection", "on", "--workspace-change-notices", "on",
        "--prompt", prompt,
    ]
    start = time.monotonic()
    run = subprocess.run(command, capture_output=True, encoding="utf-8", timeout=240)
    record = {"exit": run.returncode, "seconds": round(time.monotonic() - start, 3),
              "stdout": run.stdout, "stderr": run.stderr}
    (root.parent / f"{label}.json").write_text(json.dumps(record, indent=2), encoding="utf-8")
    print(label, run.returncode, run.stdout.strip(), flush=True)
    return record


def events_with_payload(root, events, kind, field):
    blobs = root / ".sourceharness/sessions/work/blobs"
    for event in events:
        if event["kind"] == kind:
            yield event, json.loads((blobs / event["data"][field]).read_bytes())


def call_results(root, events):
    blobs = root / ".sourceharness/sessions/work/blobs"
    by_seq = {event["seq"]: event for event in events}
    for event, payload in events_with_payload(root, events, "tool_result", "output"):
        attempt = by_seq.get(event["data"].get("attempt"))
        arguments = json.loads((blobs / attempt["data"]["arguments"]).read_bytes()) if attempt else {}
        if not isinstance(arguments, dict):
            arguments = {}
        yield event, payload, arguments


def verify_baseline(reads, archived):
    pieces = archived.splitlines(keepends=True)
    start, end = len(b"".join(pieces[:43])), len(b"".join(pieces[:179]))
    bytes_seen = bytearray(end - start)
    references = set()
    for event, payload, _ in reads:
        if event["data"]["name"] != "read" or payload.get("status") != "ok":
            continue
        if payload.get("source", {}).get("path") != "report.txt" or payload["revision"] != sha(archived):
            continue
        low, high = payload["range"]["byte_start"], payload["range"]["byte_end"]
        if payload["content"].encode() != archived[low:high]:
            raise ValueError("Returned bytes disagree with the archived report")
        if max(low, start) < min(high, end):
            bytes_seen[max(low, start) - start:min(high, end) - start] = b"\1" * (min(high, end) - max(low, start))
        references.add(payload["result_ref"])
    return bool(all(bytes_seen)), references


def request_shows_output(body, call_id, ref, required_value=None):
    for item in body["input"]:
        if item.get("type") != "function_call_output" or item.get("call_id") != call_id:
            continue
        payload = json.loads(item["output"])
        if payload.get("result_ref") == ref and (required_value is None or required_value in payload.get("content", "")):
            return True
    return False


def contains_value(value, target):
    if isinstance(value, dict):
        return any(contains_value(item, target) for item in value.values())
    if isinstance(value, list):
        return any(contains_value(item, target) for item in value)
    if isinstance(value, str):
        if target in value:
            return True
        if value.lstrip().startswith(("{", "[")):
            try:
                return contains_value(json.loads(value), target)
            except ValueError:
                return False
    return False


def evidence_for_repair(root, events, baseline_refs, archived_line):
    calls = list(call_results(root, events))
    requests = [(event, request_body(root, event)[0]) for event in events if event["kind"] == "model_request"]
    receipts = [event for event in events if event["kind"] == "validation_receipt"]
    corrections = [
        event for event in events
        if event["kind"] == "edit_receipt" and event["data"]["source"].get("path") == "submission.json"
    ]
    failed = next(((event, payload) for event, payload, _ in calls
                   if event["data"]["name"] == "validate" and payload.get("passed") is False), None)
    recovered = [
        (event, payload) for event, payload, arguments in calls
        if event["data"]["name"] == "read" and arguments.get("result_ref") in baseline_refs
        and archived_line in payload.get("content", "") and payload.get("status") == "ok"
    ]
    first_mutation = corrections[0]["seq"] if corrections else None
    failure_seen = bool(failed and first_mutation and any(
        failed[0]["seq"] < request["seq"] < first_mutation
        and request_shows_output(body, failed[0]["data"]["call_id"], failed[1]["result_ref"])
        for request, body in requests
    ))
    archived_value_seen_after_failure = bool(failed and first_mutation and any(
        failed[0]["seq"] < event["seq"] < request["seq"] < first_mutation
        and request_shows_output(body, event["data"]["call_id"], payload["result_ref"], archived_line)
        for event, payload in recovered for request, body in requests
    ))
    return {
        "initial_request_contains_archived_value": contains_value(requests[0][1], archived_line) if requests else None,
        "validation": [event["data"] | {"event": event["seq"]} for event in receipts],
        "initial_failure_preview": failed[1]["preview"] if failed else None,
        "failed_check_visible_before_edit": failure_seen,
        "archived_value_delivered_after_failure_before_edit": archived_value_seen_after_failure,
        "archived_reads": [
            {"event": event["seq"], "ref": payload["result_ref"], "revision": payload["revision"], "range": payload["range"]}
            for event, payload in recovered
        ],
        "live_report_reads": sum(event["data"]["name"] == "read" and args.get("path") == "report.txt"
                                 and not args.get("result_ref") for event, _, args in calls),
        "corrections": [event["seq"] for event in corrections],
        "tools": [{"event": event["seq"], "name": event["data"]["name"], "status": payload["status"],
                   "code": payload.get("code")} for event, payload, _ in calls],
        "model_cost": request_cost(events),
    }


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--check":
        return check(Path(sys.argv[2]))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", required=True, type=Path)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--model", default="auto/deepseek-v4.1-flash-1017")
    args = parser.parse_args()
    args.exe, args.out = args.exe.resolve(), args.out.resolve()
    root = args.out / "workspace"
    args.out.mkdir(parents=True)
    root.mkdir()

    with ZipFile(args.archive) as evidence:
        source = evidence.read("same-schema/fixtures/pages-6.txt")
        results = json.loads(evidence.read("same-schema/summary-verified.json"))
    draft = next(row["arms"]["candidate"]["answer"] for row in results["pairs"] if row["case"] == "pages-6")
    first_lines = source.splitlines(keepends=True)
    archived_token = f' original-audit={sha(source)[0:16]}'.encode()
    first_lines[116] = first_lines[116].rstrip(b"\r\n") + archived_token + b"\n"
    archived = b"".join(first_lines)
    expected = {f"line_{number}": first_lines[number - 1].decode().rstrip("\r\n") for number in (44, 117, 179)}
    (args.out / "archived-report.txt").write_bytes(archived)
    report = root / "report.txt"
    report.write_bytes(archived)
    oracle = args.out / "oracle.json"
    oracle.write_text(json.dumps(expected, indent=2), encoding="utf-8")
    config = args.out / "checks.json"
    worker = Path(__file__).resolve()
    config.write_text(json.dumps({"acceptance": {
        "description": "Check the draft format and compare the three fields with the archived audit.",
        "program": sys.executable, "args": ["-B", str(worker), "--check", str(oracle)],
        "inputs": [str(worker), str(oracle)], "timeout_seconds": 20,
    }}, indent=2), encoding="utf-8")

    earlier = run_cli(args, root, config, ARCHIVE_PROMPT, "archive")
    original_events = read_events(root)
    complete, references = verify_baseline(call_results(root, original_events), archived)
    if earlier["exit"] != 0 or not complete:
        raise RuntimeError("The earlier live run did not establish full source coverage")
    (args.out / "initial-submission.txt").write_bytes(draft.encode())
    submission = root / "submission.json"
    submission.write_bytes(draft.encode())
    before = score_submission(submission, expected)
    assert before["issues"] == [
        {"code": "EXPECTED_SINGLE_JSON_OBJECT"},
        {"code": "ARCHIVED_VALUE_MISMATCH", "field": "line_117"},
    ]
    first_lines[116] = b"Row 117 belongs to the live revision of this report.\n"
    live = b"".join(first_lines)
    report.write_bytes(live)
    repaired = run_cli(args, root, config, REPAIR_PROMPT, "repair")
    active_events = read_events(root)[len(original_events):]
    history = evidence_for_repair(root, active_events, references, expected["line_117"])
    final = score_submission(submission, expected)
    validations = history["validation"]
    original_result = json.loads(history["initial_failure_preview"]) if history["initial_failure_preview"] else None
    final_revision = sha(json.dumps(
        {"report.txt": sha(live), "submission.json": sha(submission.read_bytes())},
        separators=(",", ":"), sort_keys=True,
    ).encode())
    result = {
        "model": args.model, "effort": "low", "exe_sha256": sha(args.exe.read_bytes()),
        "archive_sha256": sha(args.archive.read_bytes()), "old_report_sha256": sha(archived),
        "live_report_sha256": sha(live), "baseline_verified_full_interval": complete,
        "historical_snapshot_refs": sorted(references), "initial": before, "final": final,
        "report_preserved": report.read_bytes() == live, "repair_process_exit": repaired["exit"],
        "final_validation_matches_delivered_revision": bool(
            validations and validations[-1]["passed"] and validations[-1]["workspace_revision"] == final_revision
        ),
        **history,
    }
    result["passed"] = (
        original_result == before and repaired["exit"] == 0 and not final["issues"]
        and not history["initial_request_contains_archived_value"]
        and history["failed_check_visible_before_edit"] and history["archived_value_delivered_after_failure_before_edit"]
        and result["report_preserved"] and result["final_validation_matches_delivered_revision"]
    )
    (args.out / "summary.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    return int(not result["passed"])


if __name__ == "__main__":
    raise SystemExit(main())
