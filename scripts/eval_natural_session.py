"""Run consecutive user tasks in one SourceHarness workspace and session."""

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import sys

from check_natural_session import grade_workspace
from eval_repair_loop import call_results, contains_value
from eval_session import Trial, execute, read_events, request_cache, request_cost
from natural_session_cases import CASES, SAMPLE_REPORT, WARMUP, apply_external_changes, protected_paths, seed
from session_eval_cases import encode, sha, workspace_files
from session_wire_audit import request_body, request_context


SCRIPTS = Path(__file__).resolve().parent
CHECKER = SCRIPTS / "check_natural_session.py"


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def checks_for(directory):
    oracle = directory / "current-check.json"
    inputs = [CHECKER, SCRIPTS / "natural_session_cases.py", SCRIPTS / "session_eval_cases.py", oracle]
    checks = directory / "checks.json"
    save(checks, {"acceptance": {
        "program": sys.executable, "args": ["-B", str(CHECKER), str(oracle)],
        "description": "Check the delivered behavior, documentation, and protected files for the current task.",
        "inputs": [str(path) for path in inputs], "timeout_seconds": 60,
    }})
    return checks, oracle, inputs[:-1]


def phase_cost(root, events):
    calls = list(call_results(root, events))
    errors = [
        {"event": event["seq"], "name": event["data"]["name"], "code": result.get("code"),
         "reason": result.get("reason"), "arguments": arguments,
         "later_success_with_same_tool": any(
             later["data"]["name"] == event["data"]["name"] and output["status"] == "ok"
             for later, output, _ in calls[index + 1:]
         )}
        for index, (event, result, arguments) in enumerate(calls) if result["status"] == "error"
    ]
    attempts = {event["seq"] for event in events if event["kind"] == "tool_attempt"}
    return {
        "cost": request_cost(events), "cache": request_cache(root, events),
        "request_context": request_context(root, events),
        "tool_attempts": len(attempts), "tool_results": len(calls),
        "tool_names": dict(Counter(event["data"]["name"] for event, _, _ in calls)),
        "attempts_without_result": sorted(attempts - {event["data"]["attempt"] for event, _, _ in calls}),
        "tool_rejections": errors,
    }


def interrupt_evidence(root, events):
    intent = next((event for event in reversed(events) if event["kind"] == "edit_intent"), None)
    if intent is None:
        return {"triggered": False}
    data = intent["data"]
    path = root / data["path"]
    return {
        "triggered": True, "intent_event": intent["seq"],
        "operation": data["operation"], "path": data["path"],
        "before": data["before"], "after": data["after"],
        "candidate_present_before_resume": path.is_file() and sha(path.read_bytes()) == data["after"],
        "receipt_recorded_before_resume": any(
            event["kind"] == "edit_receipt" and event["data"]["operation"] == data["operation"]
            for event in events),
    }


def reported_validation(final, validated, workspace_revision):
    claim = bool(re.search(
        r"\b(?:acceptance(?:\s+checks?)?|validation|checks?|tests?)"
        r"\s*(?:\*\*)?\s*[:–-]?\s*(?:\*\*)?\s*(?:all\s+)?pass(?:es|ed)?\b"
        r"|(?:测试|验证|检查)(?:全部|均|已)?通过", final, re.I))
    revisions = re.findall(
        r"(?:validated workspace revision|tested workspace revision|受验工作区版本)"
        r"[\s:\x60：*]+([a-f0-9]{8,64})", final, re.I)
    return {"claims_success": claim, "unsupported_success_claim": claim and not validated,
            "claimed_validation_revisions": revisions,
            "mismatched_validation_revisions": [
                value for value in revisions if not validated or not workspace_revision.startswith(value.lower())
            ]}


def old_source_paths(value):
    paths = set()
    sample, _, reading, _ = WARMUP.strip().split(",")
    if contains_value(value, sample) and contains_value(value, reading):
        paths.add("data/day1.csv")
    if contains_value(value, "Valid statuses: OK.") and contains_value(value, "mean to 2 decimal places"):
        paths.add("docs/report_policy.md")
    return paths


def history_audit(root, case, all_events, phase_events):
    first_run = next(event["run"] for event in all_events if event["kind"] == "user")
    paths = ("data/day1.csv", "docs/report_policy.md")
    revisions = {name: sha(case.initial[name].encode()) for name in paths}
    refs = {
        event["data"]["result_ref"]: event["data"]["source"]["path"]
        for event in all_events
        if event["run"] == first_run and event["kind"] == "artifact"
        and event["data"]["source"].get("path") in revisions
        and event["data"]["revision"] == revisions[event["data"]["source"]["path"]]
    }
    blobs = root / ".sourceharness/sessions/work/blobs"
    events_by_seq = {event["seq"]: event for event in all_events}
    event_sources = {}
    aliases = dict(refs)
    for event in all_events:
        data = event["data"]
        if event["kind"] == "artifact" and data.get("result_ref") in refs:
            event_sources[event["seq"]] = {refs[data["result_ref"]]}
        if event["kind"] == "tool_result":
            payload = json.loads((blobs / data["output"]).read_bytes())
            if payload.get("result_ref") in refs:
                source = refs[payload["result_ref"]]
                if source in old_source_paths(payload):
                    event_sources[event["seq"]] = {source}
        if event["kind"] == "artifact" and data["source"].get("kind") in ("history", "history_result"):
            origin = event_sources.get(data["source"].get("event"))
            if origin and len(origin) == 1:
                aliases[data["result_ref"]] = next(iter(origin))

    requests = [(event, request_body(root, event)[0])
                for event in phase_events if event["kind"] == "model_request"]
    first_visible = old_source_paths(requests[0][1]) if requests else set()
    initial_sources = []
    if requests:
        for index, item in enumerate(requests[0][1]["input"]):
            available = old_source_paths(item)
            if available:
                initial_sources.append({"input_index": index, "type": item.get("type", "message"),
                                        "role": item.get("role"), "tool": item.get("name"),
                                        "paths": sorted(available)})
        available = old_source_paths(requests[0][1].get("instructions"))
        if available:
            initial_sources.append({"type": "instructions", "paths": sorted(available)})
    mutations = [event["seq"] for event in phase_events if event["kind"] == "edit_receipt"
                 and event["data"]["source"].get("path") == "docs/change-note.md"]
    last_relevant = max(mutations, default=0)
    retrievals = []
    for event, result, arguments in call_results(root, phase_events):
        if event["data"]["name"] not in ("read", "research", "search_result") or result["status"] != "ok":
            continue
        sources = old_source_paths(result)
        if not sources:
            continue
        old_ref = arguments.get("result_ref")
        identity = aliases.get(old_ref)
        if identity not in sources:
            continue
        visible = any(event["seq"] < request["seq"] < last_relevant and any(
            item.get("type") == "function_call_output" and item.get("call_id") == event["data"]["call_id"]
            and identity in old_source_paths(item.get("output", ""))
            for item in body["input"]) for request, body in requests)
        retrievals.append({"event": event["seq"], "tool": event["data"]["name"],
                           "path": identity, "reference": old_ref, "visible_before_note": visible})
        if visible:
            aliases[result["result_ref"]] = identity
            event_sources[event["seq"]] = {identity}
    note = root / "docs/change-note.md"
    text = note.read_text(encoding="utf-8") if note.is_file() else ""
    cited_refs = {reference.lower() for reference in re.findall(r"\br\d+\b", text, re.I)}
    cited_events = {int(number) for number in re.findall(r"(?:event|seq|事件)[\s_#\x60:：-]*(\d+)", text, re.I)}
    verified = {aliases[ref] for ref in cited_refs & aliases.keys()}
    for event in cited_events & events_by_seq.keys():
        verified.update(event_sources.get(event, set()))
    new_retrieval = {row["path"] for row in retrievals if row["visible_before_note"]}
    return {
        "first_release_snapshots": refs, "missing_first_release_snapshots": sorted(set(paths) - set(refs.values())),
        "visible_at_phase_start": sorted(first_visible), "phase_start_sources": initial_sources,
        "retrievals": retrievals,
        "historical_retrieval_before_deliverable": sorted(new_retrieval),
        "cited_refs": sorted(cited_refs), "cited_events": sorted(cited_events),
        "verified_citation_paths": sorted(verified), "citations_passed": verified == set(paths),
    }


def run_scenario(args, case, index):
    directory = args.out / f"session-{index:02d}"
    directory.mkdir()
    root = directory / "workspace"
    seed(case, root)
    checks, oracle, sources = checks_for(directory)
    trial = Trial(case.name, None, edit="text", history="events", freshness="on",
                  projection="on", notices="on", receipt="versioned")
    phases = []
    for phase, prompt in enumerate(case.prompts, 1):
        checkpoint = len(read_events(root))
        before = workspace_files(root)
        external = apply_external_changes(case, root, phase)
        start = workspace_files(root)
        config = {"case": case.name, "phase": phase, "protected": {
            name: sha(start[name]) for name in protected_paths(case, phase, start)}}
        label = f"phase-{phase:02d}"
        save(directory / f"{label}-check.json", config)
        save(oracle, config)
        oracle_revision = sha(oracle.read_bytes())
        inject = args.interrupt_phase == phase
        runs = [execute(args, checks, root, trial, prompt, interrupt=inject, label=label)]
        interrupted = {"requested": inject, "triggered": False}
        if inject and runs[-1]["exit"] == 86:
            interrupted.update(interrupt_evidence(root, read_events(root)[checkpoint:]))
            runs.append(execute(args, checks, root, trial, None, label=f"{label}-resume-01"))
        if (runs[-1]["exit"] != 0
                and "The run can be resumed" in runs[-1]["error"]):
            runs.append(execute(args, checks, root, trial, None, label=f"{label}-resume-limit"))
        all_events = read_events(root)
        events = all_events[checkpoint:]
        if interrupted["triggered"]:
            receipts = [event for event in events if event["kind"] == "edit_receipt"
                        and event["data"]["operation"] == interrupted["operation"]]
            interrupted["receipt_events"] = [event["seq"] for event in receipts]
            interrupted["passed"] = (
                interrupted["candidate_present_before_resume"] and not interrupted["receipt_recorded_before_resume"]
                and len(receipts) == 1 and receipts[0]["data"]["outcome"] == "observed_target_on_recovery"
                and runs[-1]["exit"] == 0)
        independent = grade_workspace(root, config)
        current = workspace_files(root)
        workspace_revision = sha(encode({name: sha(raw) for name, raw in current.items()}))
        validation = [{"event": event["seq"], **event["data"]}
                      for event in events if event["kind"] == "validation_receipt"]
        validated = any(
            row["passed"] and row["workspace_revision"] == workspace_revision
            and row.get("input_revisions_stable")
            and row.get("input_revisions", {}).get(str(oracle)) == oracle_revision
            for row in validation)
        claims = reported_validation(runs[-1]["final"], validated, workspace_revision)
        history = history_audit(root, case, all_events, events) if case.name == SAMPLE_REPORT.name and phase == 4 else None
        row = {
            "phase": phase, "run_ids": sorted({event["run"] for event in events if event["kind"] == "user"}),
            "prompt_sha256": sha(prompt.encode()), "events": [checkpoint + 1, len(all_events)],
            "external_changes": [{"path": name, "before": sha(before[name]) if name in before else None,
                                  "after": sha(start[name]) if name in start else None} for name in external],
            "final_workspace_revision": workspace_revision, "independent": independent,
            "final_revision_validated": validated, "validation": validation, "checker_fixture_revision": oracle_revision,
            "claims": claims,
            "history": history, "interruption": interrupted, "executions": runs,
            **phase_cost(root, events),
        }
        row["passed"] = (
            runs[-1]["exit"] == 0 and independent["passed"] and validated and not claims["unsupported_success_claim"]
            and not claims["mismatched_validation_revisions"]
            and (history is None or history["citations_passed"])
            and (not inject or interrupted.get("passed", False)))
        phases.append(row)
        save(directory / f"{label}.json", row)
        print(f"session {index} phase {phase}: passed={row['passed']} exit={runs[-1]['exit']} "
              f"checks={independent['failures']} validated={validated}", flush=True)
        if runs[-1]["exit"] != 0:
            break

    all_events = read_events(root)
    profiles = [event["data"]["profile"] for event in all_events if event["kind"] == "user"]
    stable_keys = ("gateway", "model", "effort", "tools_revision", "binary_revision",
                   "history_mode", "freshness_instructions", "stale_projection", "history_budget_bytes")
    summary = {
        "session": "work", "case": case.name, "model": args.model, "effort": args.effort,
        "exe_sha256": sha(args.exe.read_bytes()),
        "settings": {"edit": trial.edit, "history": trial.history, "receipt": trial.receipt,
                     "freshness": trial.freshness, "projection": trial.projection, "notices": trial.notices,
                     "history_budget_bytes": 8192},
        "checker_inputs": {path.name: sha(path.read_bytes()) for path in sources},
        "profile": profiles[0] if profiles else None,
        "execution_conditions_unchanged": bool(profiles) and all(
            all(profile.get(key) == profiles[0].get(key) for key in stable_keys) for profile in profiles),
        "validation_inputs_by_phase": [profile.get("validation_inputs") for profile in profiles],
        "phases": phases, "cumulative": phase_cost(root, all_events),
    }
    summary["passed"] = (len(phases) == len(case.prompts) and all(row["passed"] for row in phases)
                         and summary["execution_conditions_unchanged"])
    save(directory / "summary.json", summary)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--case", choices=CASES, default=SAMPLE_REPORT.name)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--model", default="auto/deepseek-v4.1-flash-1017")
    parser.add_argument("--effort", default="low")
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--interrupt-phase", type=int)
    args = parser.parse_args()
    case = CASES[args.case]
    if args.repeats < 1 or (args.interrupt_phase is not None and not 1 <= args.interrupt_phase <= len(case.prompts)):
        parser.error("repeats must be positive and the interrupt phase must exist in the selected case")
    args.exe, args.out = args.exe.resolve(), args.out.resolve()
    args.out.mkdir(parents=True)
    runs = []
    for index in range(1, args.repeats + 1):
        runs.append(run_scenario(args, case, index))
        save(args.out / "summary.json", {"case": case.name, "runs": runs, "passed": all(run["passed"] for run in runs)})
    return int(not all(run["passed"] for run in runs))


if __name__ == "__main__":
    raise SystemExit(main())
