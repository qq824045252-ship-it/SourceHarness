"""Live cross-tool acceptance for decoded history, source attribution, and writes."""

import argparse
import json
from pathlib import Path

from eval_repair_loop import call_results, run_cli
from eval_session import read_events, write_checks
from session_eval_cases import Case, grade, sha, workspace_files


def fixture():
    exact = 'mode="stable"\r\nroute=team\\east'
    # Source CRLF and backslashes are significant. The later live file uses another code.
    archived = f"release audit\r\n{exact}\r\nAUDIT_CODE=ARCH-75F4\r\n".encode()
    live = b"release audit\r\nmode=preview\r\nroute=team\\west\r\nAUDIT_CODE=LIVE-E291\r\n"
    before = b"# deployment\r\nAUDIT_CODE=unset\r\nENV=production\r\n"
    desired = before.replace(b"unset", b"ARCH-75F4", 1)
    record_path = "deliverables/release/old-code.txt"
    record = b"AUDIT_CODE=ARCH-75F4\n"
    return exact, Case({"settings.txt": archived, "deployment.cfg": before},
                       {"settings.txt": live, "deployment.cfg": desired, record_path: record},
                       "", {"AUDIT_CODE": "ARCH-75F4"}, report_path=record_path)


def source_reads(root, events, archived):
    return {
        event["seq"]: payload["result_ref"] for event, payload, call in call_results(root, events)
        if event["data"]["name"] == "read" and call.get("path") == "settings.txt"
        and payload.get("status") == "ok" and payload.get("content", "").encode() == archived
    }


def archived_routes(calls, snapshots, archived):
    routes = []
    for event, payload, call in calls:
        if event["data"]["name"] != "read" or payload["status"] != "ok":
            continue
        source = payload.get("source", {})
        if source.get("kind") == "history_result" and source.get("event") in snapshots:
            try:
                envelope = json.loads(payload["content"])
            except (KeyError, json.JSONDecodeError):
                continue
            if (isinstance(envelope, dict) and envelope.get("result_ref") == snapshots[source["event"]]
                    and envelope.get("revision") == sha(archived)
                    and envelope.get("content") == archived.decode()):
                routes.append({"seq": event["seq"], "kind": "history_result", "source_event": source["event"]})
        elif (call.get("result_ref") in snapshots.values()
                and payload.get("revision") == sha(archived)
                and payload.get("content", "").encode() == archived):
            routes.append({"seq": event["seq"], "kind": "snapshot", "result_ref": call["result_ref"]})
    return routes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--model", default="auto/deepseek-v4.1-flash-1017")
    args = parser.parse_args()
    args.exe, args.out = args.exe.resolve(), args.out.resolve()
    root = args.out / "workspace"
    root.mkdir(parents=True)
    exact, case = fixture()
    case.seed(root)
    checks = write_checks(args.out, [case])
    oracle = args.out / "oracle.json"
    rules = json.loads(oracle.read_bytes())
    rules["no-policy"]["expected_sha256"][case.report_path] = sha(case.expected[case.report_path])
    oracle.write_text(json.dumps(rules, indent=2), encoding="utf-8")
    initial = run_cli(
        args, root, checks,
        "Read the exact contents of settings.txt into the session for a later audit. "
        "Keep the files as they are. Reply with the observed revision, without quoting file contents.",
        "archive",
    )
    earlier = read_events(root)
    if initial["exit"] or not source_reads(root, earlier, case.inputs["settings.txt"]):
        raise RuntimeError("The archive run did not return exact source bytes")
    (root / "settings.txt").write_bytes(case.expected["settings.txt"])
    query_args = json.dumps({"scope": "history", "query": exact}, separators=(",", ":"))
    result = run_cli(
        args, root, checks,
        "Find the previous audit record containing the exact two-line block. To test lookup through "
        "the CRLF separator, quote and backslash, first use research with these exact JSON arguments "
        f"(the JSON escapes represent source characters): {query_args}\n"
        "If research suggests retry, call research with that retry object unchanged.\n\n"
        "Use the original settings.txt snapshot from that audit to obtain the AUDIT_CODE on the line "
        "after the block. Edit only the code value in deployment.cfg and preserve all other bytes. "
        f"Write a new {case.report_path} with the audit code in the form AUDIT_CODE=value followed by one LF. "
        "Leave settings.txt at the current revision. Run acceptance and report its outcome.",
        "delivery",
    )
    report = assess(args, root, earlier, exact, case, result)
    (args.out / "summary.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print("Acceptance:", report["passed"], flush=True)
    return int(not report["passed"])


def assess(args, root, earlier, exact, case, result):
    archived = case.inputs["settings.txt"]
    snapshots = source_reads(root, earlier, archived)
    events = read_events(root)[len(earlier):]
    calls = list(call_results(root, events))
    journal_blobs = root / ".sourceharness/sessions/work/blobs"
    exact_retries = {
        event["data"]["result_ref"] for event in events
        if event["kind"] == "artifact" and event["data"]["source"].get("kind") == "research_retry"
        and json.loads((journal_blobs / event["data"]["revision"]).read_bytes())["query"] == exact
    }
    full_history = [
        {"result_seq": event["seq"], "attempt_seq": event["data"]["attempt"],
         "hits": [hit["seq"] for hit in payload.get("hits", [])],
         "query_is_exact": call.get("query") == exact or (
             call.get("scope") == "retry" and call.get("result_ref") in exact_retries
         ),
         "suggested_exact_retry": payload.get("retry", {}).get("result_ref") in exact_retries,
         "matched": payload.get("presentation", {}).get("matching_items")}
        for event, payload, call in calls
        if event["data"]["name"] == "research" and call.get("scope") in ("history", "retry")
        and payload["status"] == "ok"
    ]
    observed_routes = archived_routes(calls, snapshots, archived)
    errors = [
        {"seq": event["seq"], "tool": event["data"]["name"], "code": payload.get("code")}
        for event, payload, _ in calls if payload["status"] == "error"
    ]
    tool_names = [event["data"]["name"] for event, _, _ in calls]
    receipts = [event["data"] for event in events if event["kind"] == "validation_receipt"]
    final_revision = case.workspace_revision(root)
    checked_final = bool(receipts and receipts[-1]["passed"] and receipts[-1]["workspace_revision"] == final_revision)
    expected_files = workspace_files(root) == case.expected
    result = {
        "model": args.model, "effort": "low", "build_sha256": sha(args.exe.read_bytes()),
        "first_known_snapshot_refs": sorted(set(snapshots.values())), "history": full_history,
        "archived_read": bool(observed_routes), "archived_routes": observed_routes,
        "tool_sequence": tool_names, "errors": errors,
        "files_match_exactly": expected_files, "final_revision_validated": checked_final,
        "independent": grade(root, case), "exit": result["exit"], "final": result["stdout"].strip(),
    }
    result["passed"] = (
        result["exit"] == 0 and not errors and expected_files and result["independent"]["passed"] and checked_final
        and observed_routes and {"read", "research", "edit", "write", "validate"} <= set(tool_names)
        and any(row["query_is_exact"] and any(seq in snapshots for seq in row["hits"]) for row in full_history)
        and all(row["attempt_seq"] not in row["hits"] for row in full_history)
    )
    return result


if __name__ == "__main__":
    raise SystemExit(main())
