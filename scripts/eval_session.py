"""Run live SourceHarness sessions against runner-owned fixtures."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, replace
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

from session_eval_cases import (
    BASE_A, BASE_B, BASELINE_PROMPT, HOLDOUT, Case, build_case, grade, policy_case, sha,
)

MAINTAINER_NOTE = b"\n# Maintainer note, preserve it.\n"
PROVENANCE_PROMPT = (
    "Keep all workspace files as they are. Determine whether the tool definitions, validation configuration "
    "and workspace changed since the previous successful verification. Run the newly configured acceptance "
    "check against the current workspace. Report the check name and put TOOLS=changed or TOOLS=same, "
    "VALIDATION=changed or VALIDATION=same, and WORKSPACE=changed or WORKSPACE=same on separate lines."
)


@dataclass(frozen=True)
class Trial:
    name: str
    case: Case
    group: str = "policy"
    edit: str = "text"
    history: str = "events"
    freshness: str = "on"
    projection: str = "on"
    recovery: str = ""
    provenance: bool = False
    repeat: int = 1

    def flags(self):
        return [
            "--edit-mode", self.edit, "--history-mode", self.history,
            "--freshness-instructions", self.freshness, "--stale-projection", self.projection,
            "--history-budget", "8192",
        ]


def read_events(root):
    journal = root / ".sourceharness/sessions/work/events.jsonl"
    if not journal.exists():
        return []
    return [json.loads(line) for line in journal.read_text(encoding="utf-8").splitlines()]


def visible_facts(root, request):
    blob = root / ".sourceharness/sessions/work/blobs" / request["input"]
    wire = json.loads(blob.read_bytes())
    _, marker, tail = wire.get("instructions", "").partition(
        "A validation receipt establishes the workspace and conditions it actually tested.\n"
    )
    if not marker:
        return []
    facts, _ = json.JSONDecoder().raw_decode(tail)
    return facts if isinstance(facts, list) else [row for bucket in facts.values() for row in bucket]


def request_cost(events):
    requests = {event["seq"] for event in events if event["kind"] == "model_request"}
    terminals = {
        event["data"]["request"]: event["data"]
        for event in events
        if event["kind"] in ("model_error", "model_response", "model_rejected")
    }
    usages = [terminals.get(seq, {}).get("usage") for seq in requests]
    known = [usage for usage in usages if usage is not None and usage.get("total_tokens") is not None]
    latencies = [terminals.get(seq, {}).get("elapsed_ms") for seq in requests]
    return {
        "attempts": len(requests),
        "known_usage": {
            name: sum(usage.get(name, 0) for usage in known)
            for name in ("input_tokens", "output_tokens", "reasoning_tokens", "total_tokens")
        },
        "unknown_usage_attempts": len(requests) - len(known),
        "known_model_ms": sum(value for value in latencies if value is not None),
        "unknown_duration_attempts": latencies.count(None),
    }


def execute(args, checks, root, trial, prompt, interrupt=False, continuation=False, executable=None):
    env = os.environ.copy()
    env.pop("SOURCE_HARNESS_TEST_INTERRUPT", None)
    if interrupt:
        env["SOURCE_HARNESS_TEST_INTERRUPT"] = "after_file_replace"
    command = [
        str(executable or args.exe), "agent", "--root", str(root), "--session", "work", "--checks", str(checks),
        "--model", args.model, "--effort", args.effort, *trial.flags(),
    ]
    if continuation:
        command.append("--continue")
    if prompt is not None:
        command.extend(["--prompt", prompt])
    elif not continuation:
        command.append("--resume")
    start = time.monotonic()
    try:
        result = subprocess.run(command, capture_output=True, text=True, encoding="utf-8", env=env, timeout=240)
        exit_code, stdout, stderr = result.returncode, result.stdout, result.stderr
    except subprocess.TimeoutExpired as error:
        exit_code = None
        stdout = (error.stdout or b"").decode("utf-8", "replace")
        stderr = "Evaluation process exceeded 240 seconds"
    suffix = "continue" if continuation else "resume" if prompt is None else "start"
    (root.parent / f"{root.name}-{suffix}.stdout.txt").write_text(stdout, encoding="utf-8")
    (root.parent / f"{root.name}-{suffix}.stderr.txt").write_text(stderr, encoding="utf-8")
    return {"exit": exit_code, "seconds": round(time.monotonic() - start, 2), "final": stdout.strip(), "error": stderr.strip()}


def summarize(root, case, events, executions):
    independent = grade(root, case)
    revision = case.workspace_revision(root)
    receipts = [e["data"] for e in events if e["kind"] == "validation_receipt"]
    validated = any(r["passed"] and r["workspace_revision"] == revision for r in receipts)
    calls = [e["data"] for e in events if e["kind"] == "tool_result"]
    cost = request_cost(events)
    return {
        "passed": bool(executions[-1]["exit"] == 0 and independent["passed"] and validated),
        "independent": independent,
        "active_revision": revision,
        "active_revision_validated": validated,
        "validation_receipts": receipts,
        "tool_calls": dict(Counter(call["name"] for call in calls)),
        "errors": [call["code"] for call in calls if call["status"] == "error"],
        "model_rounds": sum(e["kind"] == "model_response" for e in events),
        "cost": cost,
        "tokens": None if cost["unknown_usage_attempts"] else cost["known_usage"]["total_tokens"],
        "history_auxiliary": [
            e["data"]["history_auxiliary"]
            for e in events if e["kind"] == "model_request" and "history_auxiliary" in e["data"]
        ],
        "executions": executions,
    }


def write_checks(directory, group, cases):
    variants = {}
    for case in cases:
        config = case.grader_config()
        if group == "build":
            config["app_output"] = case.report_fields["OUTPUT"]
        if "service.cfg" in case.expected:
            config["acceptable_maintainer_change"] = {"service.cfg": sha(case.expected["service.cfg"] + MAINTAINER_NOTE)}
        identity = sha(case.inputs["policy.md"]) if "policy.md" in case.inputs else "no-policy"
        if identity in variants and variants[identity] != config:
            raise ValueError("Different fixture variants need different input identities")
        variants[identity] = config
    oracle = directory / "oracle.json"
    oracle.write_text(json.dumps(variants, indent=2), encoding="utf-8")
    checker = Path(__file__).with_name("check_session_fixture.py").resolve()
    definition = {
        "program": sys.executable,
        "args": ["-B", str(checker), str(oracle)],
        "description": "Check the delivered files and evidence against the current fixture.",
        "timeout_seconds": 20,
    }
    for label in ("acceptance", "acceptance_b"):
        config = definition if label == "acceptance" else definition | {
            "description": "Recheck the delivered files and evidence with acceptance configuration B.",
        }
        (directory / f"{label}.json").write_text(json.dumps({"acceptance": config}, indent=2), encoding="utf-8")
    return directory / "acceptance.json"


def prepare_group(output, group, trials, args):
    directory = output / group
    directory.mkdir()
    base = None if group == "build" else policy_case(BASE_A)
    cases = [trial.case for trial in trials]
    if group == "continuation":
        cases.append(policy_case(HOLDOUT))
    checks = write_checks(directory, group, cases + ([base] if base else []))
    if not base:
        return directory, checks, None, None
    seed = directory / "baseline"
    base.seed(seed)
    execution = execute(args, checks, seed, Trial("baseline", base), BASELINE_PROMPT)
    baseline = summarize(seed, base, read_events(seed), [execution])
    if not baseline["passed"]:
        (directory / "baseline-failed.json").write_text(json.dumps(baseline, indent=2), encoding="utf-8")
        raise RuntimeError(f"Baseline did not pass for {group}; see {directory}")
    return directory, checks, seed, baseline



def run_trial(args, group, trial):
    directory, checks, base, _ = group
    root = directory / trial.name
    case = trial.case
    if base is not None:
        shutil.copytree(base, root, ignore=shutil.ignore_patterns("workspace.lock"))
        if "evidence.md" not in case.inputs:
            (root / "evidence.md").unlink()
    case.seed(root)
    checkpoint = len(read_events(root))
    if trial.provenance:
        checks = directory / "acceptance_b.json"
    prompt = PROVENANCE_PROMPT if trial.provenance else case.prompt
    if trial.recovery:
        prompt = "Update service.cfg before writing the evidence note. " + prompt
    first_exe = args.initial_exe if trial.recovery == "upgrade" else None
    executions = [execute(args, checks, root, trial, prompt, bool(trial.recovery), executable=first_exe)]
    recovery = {}
    if trial.recovery and executions[0]["exit"] == 86:
        initial_events = read_events(root)[checkpoint:]
        intent = next(e["data"] for e in reversed(initial_events) if e["kind"] == "edit_intent")
        target = root / intent["path"]
        recovery["candidate_present_before_resume"] = sha(target.read_bytes()) == intent["after"]
        resume_conflict = trial.recovery == "conflict" and intent["path"] == "service.cfg"
        if resume_conflict:
            target.write_bytes(target.read_bytes() + MAINTAINER_NOTE)
            case = case.with_maintainer_note("service.cfg", MAINTAINER_NOTE)
        if trial.recovery == "upgrade":
            case = policy_case(HOLDOUT)
            (root / "policy.md").write_bytes(case.inputs["policy.md"])
            correction = "The policy changed during the interruption. Use the current policy file for this delivery."
            executions.append(execute(args, checks, root, replace(trial, edit="range"), correction, continuation=True))
        else:
            executions.append(execute(args, checks, root, trial, None))
        rows = [e for e in read_events(root)[checkpoint:] if e["data"].get("operation") == intent["operation"]]
        receipts = [e["data"] for e in rows if e["kind"] == "edit_receipt"]
        recovery["interrupted_path"] = intent["path"]
        recovery["receipts"] = receipts
        recovery["conflict_detected"] = any(
            e["kind"] == "tool_result" and e["data"]["code"] == "REVISION_CONFLICT" for e in rows
        )
        recovery["maintainer_note_preserved"] = MAINTAINER_NOTE in (root / "service.cfg").read_bytes()
        if trial.recovery == "conflict":
            recovery["passed"] = recovery["conflict_detected"] and recovery["maintainer_note_preserved"]
        elif trial.recovery == "upgrade":
            linked = next((
                e["data"] for e in read_events(root)[checkpoint:]
                if e["kind"] == "user" and e["data"].get("continuation", {}).get("correction")
            ), {})
            recovery["linked_continuation"] = linked.get("continuation")
            recovery["passed"] = bool(recovery["linked_continuation"]) and len(receipts) == 0
        else:
            recovery["passed"] = len(receipts) == 1 and receipts[0]["outcome"] == "observed_target_on_recovery"
        recovery["passed"] &= recovery["candidate_present_before_resume"]
    events = read_events(root)[checkpoint:]
    report = summarize(root, case, events, executions)
    first = next((e["data"] for e in events if e["kind"] == "model_request"), None)
    report["first_visible_facts"] = visible_facts(root, first) if first else []
    if trial.recovery:
        report["passed"] &= bool(recovery.get("passed"))
    if trial.provenance:
        answers = dict(re.findall(r"\b(TOOLS|VALIDATION|WORKSPACE)=(changed|same)\b", executions[-1]["final"]))
        report["provenance_answer"] = answers
        requests = [e["data"] for e in events if e["kind"] == "model_request"]
        report["provenance_by_request"] = [
            any(
                item["workspace_revision_comparison"]["status"] == "same"
                and item["comparisons"]["tools_revision"]["status"] == "changed"
                and item["comparisons"]["validation_config_revision"]["status"] == "changed"
                and bool(item["result_ref"])
                for item in visible_facts(root, request) if item.get("kind") == "validation_provenance"
            )
            for request in requests
        ]
        report["provenance_observed"] = bool(requests) and all(report["provenance_by_request"])
        report["passed"] &= answers == {"TOOLS": "changed", "VALIDATION": "changed", "WORKSPACE": "same"}
        report["passed"] &= any(r.get("check") == "acceptance" for r in report["validation_receipts"])
        report["passed"] &= report["provenance_observed"]
    row = {
        "name": trial.name, "group": trial.group, "repeat": trial.repeat,
        "edit": trial.edit, "history": trial.history, "freshness": trial.freshness, "projection": trial.projection,
        "expected": case.grader_config(), "recovery": recovery, **report,
    }
    print(row["name"], row["passed"], row["cost"], row["errors"], flush=True)
    return row


def trials_for(profile, repeats):
    standard = policy_case(BASE_B)
    holdout = policy_case(HOLDOUT, middle=True, create=True)
    large = policy_case(HOLDOUT, large=True, create=True)
    selected = [
        Trial("default-current-version", standard),
        Trial("holdout-default-create", holdout, "holdout"),
        Trial("resume-after-replace", standard, recovery="clean"),
        Trial("resume-with-external-change", standard, recovery="conflict"),
        Trial("64k-middle-edit", large, "large"),
        Trial("64k-resume-after-replace", large, "large", recovery="clean"),
        Trial("repair-cli-and-docs", build_case(), "build"),
        Trial("changed-tools-same-workspace", policy_case(BASE_A), "provenance", edit="range", provenance=True),
    ]
    history = [
        Trial("history-conversation", standard, "history", history="conversation", freshness="off", projection="off"),
        Trial("history-events", standard, "history", freshness="off", projection="off"),
        Trial("history-facts", standard, "history", history="facts", freshness="off", projection="off"),
        Trial("history-instructions", standard, "history", projection="off"),
        Trial("history-projection", standard, "history", freshness="off"),
    ]
    edits = [
        Trial("edit-text", standard, "edit"),
        Trial("edit-range", standard, "edit", edit="range"),
        Trial("edit-patch", standard, "edit", edit="patch"),
    ]
    profiles = {
        "selected": selected, "history": history, "edit": edits, "all": selected + history + edits,
        "recovery": [trial for trial in selected if trial.recovery],
        "provenance": [trial for trial in selected if trial.provenance],
        "continuation": [Trial("resume-after-upgrade-with-correction", standard, "continuation", recovery="upgrade")],
    }
    return [
        replace(trial, name=trial.name if repeats == 1 else f"{trial.name}-{repeat}", repeat=repeat)
        for repeat in range(1, repeats + 1)
        for trial in (profiles[profile] if repeat % 2 else profiles[profile][::-1])
    ]


def history_balance(rows):
    checks = []
    for repeat in sorted({row["repeat"] for row in rows if row["group"] == "history"}):
        arms = {
            row["history"]: row
            for row in rows if row["group"] == "history" and row["repeat"] == repeat
            and row["freshness"] == row["projection"] == "off"
        }
        events, facts = (arms[name] for name in ("events", "facts"))
        first_events = next(iter(events["history_auxiliary"]), {})
        first_facts = next(iter(facts["history_auxiliary"]), {})
        facts_as_json = lambda rows: sorted(json.dumps(item, sort_keys=True) for item in rows)
        visible_equal = facts_as_json(events["first_visible_facts"]) == facts_as_json(facts["first_visible_facts"])
        checks.append({
            "repeat": repeat, "events": first_events, "facts": first_facts,
            "same_first_request_facts": visible_equal and bool(events["first_visible_facts"])
            and first_events.get("facts_revision") == first_facts.get("facts_revision"),
        })
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--initial-exe", type=Path, help="Older executable for the interrupted portion of a continuation trial")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--workers", type=int, default=3)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument(
        "--profile", choices=["all", "recovery", "selected", "edit", "history", "provenance", "continuation"],
        default="selected",
    )
    parser.add_argument("--model", default=os.environ.get("OPENAI_MODEL", "auto/deepseek-v4.1-flash-1017"))
    parser.add_argument("--effort", default="low")
    args = parser.parse_args()
    if args.repeats < 1 or args.workers < 1:
        parser.error("repeats and workers must be positive")
    args.exe, args.out = args.exe.resolve(), args.out.resolve()
    if args.initial_exe is not None:
        args.initial_exe = args.initial_exe.resolve()
    args.out.mkdir(parents=True)
    trials = trials_for(args.profile, args.repeats)
    grouped = {}
    for trial in trials:
        grouped.setdefault(trial.group, []).append(trial)
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        preparations = {
            group: pool.submit(prepare_group, args.out, group, group_trials, args)
            for group, group_trials in grouped.items()
        }
        groups = {group: result.result() for group, result in preparations.items()}
        observations = list(pool.map(lambda trial: run_trial(args, groups[trial.group], trial), trials))
    balance = history_balance(observations)
    report = {
        "model_route": args.model, "effort": args.effort, "profile": args.profile,
        "repeat_count": args.repeats, "exe_sha256": sha(args.exe.read_bytes()),
        "initial_exe_sha256": sha(args.initial_exe.read_bytes()) if args.initial_exe else None,
        "baselines": {group: results[3] for group, results in groups.items()},
        "observations": observations, "history_balance": balance,
        "pass": sum(row["passed"] for row in observations),
    }
    (args.out / "summary.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    return int(report["pass"] != len(observations) or any(not check["same_first_request_facts"] for check in balance))


if __name__ == "__main__":
    raise SystemExit(main())
