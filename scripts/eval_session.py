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
    BASE_A, BASE_B, BASELINE_PROMPT, HOLDOUT, Case, Policy, build_case, grade, policy_case, sha,
)
from session_wire_audit import feedback_audit, request_context

MAINTAINER_NOTE = b"\n# Maintainer note, preserve it.\n"
PROVENANCE_PROMPT = (
    "Keep all workspace files as they are. Determine whether the tool definitions, validation configuration "
    "and workspace changed since the previous successful verification. Run the newly configured acceptance "
    "check against the current workspace. Report the check name and put TOOLS=changed or TOOLS=same, "
    "VALIDATION=changed or VALIDATION=same, and WORKSPACE=changed or WORKSPACE=same on separate lines."
)
INPUTS_PROMPT = (
    "Leave all workspace files unchanged. Compare the external validation input contents against the previous "
    "successful check. Run the currently configured acceptance check. In your answer put INPUTS=changed or "
    "INPUTS=same and CHECK=passed or CHECK=failed on separate lines."
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
    notices: str | None = None
    recovery: str = ""
    provenance: bool = False
    validation_drift: bool = False
    receipt: str = "versioned"
    repeat: int = 1

    def flags(self):
        flags = [
            "--edit-mode", self.edit, "--history-mode", self.history,
            "--freshness-instructions", self.freshness, "--stale-projection", self.projection,
            "--history-budget", "8192",
        ]
        if self.receipt != "versioned":
            flags.extend(["--receipt-mode", self.receipt])
        if self.notices is not None:
            flags.extend(["--workspace-change-notices", self.notices])
        return flags


def read_events(root):
    journal = root / ".sourceharness/sessions/work/events.jsonl"
    if not journal.exists():
        return []
    return [json.loads(line) for line in journal.read_text(encoding="utf-8").splitlines()]


def visible_facts(root, request):
    blob = root / ".sourceharness/sessions/work/blobs" / request["input"]
    wire = json.loads(blob.read_bytes())
    needle = "A validation receipt establishes the workspace and conditions it actually tested.\n"
    candidates = [wire.get("instructions", "")]
    for item in reversed(wire.get("input", [])):
        if isinstance(item, dict) and isinstance(item.get("content"), str):
            candidates.append(item["content"])
    for text in candidates:
        _, marker, tail = text.partition(needle)
        if marker:
            facts, _ = json.JSONDecoder().raw_decode(tail)
            return facts if isinstance(facts, list) else [row for bucket in facts.values() for row in bucket]
    return []


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


def request_cache(root, events):
    blobs = root / ".sourceharness/sessions/work/blobs"
    requests = {event["seq"] for event in events if event["kind"] == "model_request"}
    measured = {}
    for event in events:
        data = event["data"]
        if event["kind"] not in ("model_response", "model_rejected") or data["request"] not in requests:
            continue
        response = json.loads((blobs / data["raw_response"]).read_bytes())
        usage = response.get("usage") or {}
        cached = (usage.get("input_tokens_details") or {}).get("cached_tokens")
        if cached is not None:
            measured[data["request"]] = cached
    return {
        "known_cached_input_tokens": sum(measured.values()),
        "requests_with_cache_usage": len(measured),
        "requests_without_cache_usage": len(requests) - len(measured),
    }


def execute(args, checks, root, trial, prompt, interrupt=False, continuation=False, executable=None, label=None):
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
    suffix = label or ("continue" if continuation else "resume" if prompt is None else "start")
    (root.parent / f"{root.name}-{suffix}.stdout.txt").write_text(stdout, encoding="utf-8")
    (root.parent / f"{root.name}-{suffix}.stderr.txt").write_text(stderr, encoding="utf-8")
    return {"exit": exit_code, "seconds": round(time.monotonic() - start, 2), "final": stdout.strip(), "error": stderr.strip()}


def summarize(root, case, events, executions):
    independent = grade(root, case)
    revision = case.workspace_revision(root)
    receipts = [e["data"] for e in events if e["kind"] == "validation_receipt"]
    validated = any(r["passed"] and r["workspace_revision"] == revision for r in receipts)
    attempts = {e["seq"] for e in events if e["kind"] == "tool_attempt"}
    calls = [e["data"] for e in events if e["kind"] == "tool_result" and e["data"].get("attempt") in attempts]
    cost = request_cost(events)
    return {
        "passed": bool(executions[-1]["exit"] == 0 and independent["passed"] and validated),
        "independent": independent,
        "active_revision": revision,
        "active_revision_validated": validated,
        "validation_receipts": receipts,
        "tool_calls": dict(Counter(call["name"] for call in calls)),
        "tool_accounting": {
            "attempts_started": len(attempts),
            "results_recorded": len(calls),
            "rejections": sum(call["status"] == "error" for call in calls),
            "without_recorded_result": len(attempts - {call["attempt"] for call in calls}),
            "validation_checks_failed": sum(not receipt["passed"] for receipt in receipts),
        },
        "errors": [call["code"] for call in calls if call["status"] == "error"],
        "model_rounds": sum(e["kind"] == "model_response" for e in events),
        "cost": cost,
        "cache": request_cache(root, events),
        "tokens": None if cost["unknown_usage_attempts"] else cost["known_usage"]["total_tokens"],
        "history_auxiliary": [
            e["data"]["history_auxiliary"]
            for e in events if e["kind"] == "model_request" and "history_auxiliary" in e["data"]
        ],
        "request_context": request_context(root, events),
        "executions": executions,
    }


def write_checks(directory, cases):
    variants = {}
    for case in cases:
        config = case.grader_config()
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
        "inputs": [str(checker), str(checker.with_name("session_eval_cases.py")), str(oracle)],
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
    checks = write_checks(directory, cases + ([base] if base else []))
    if not base:
        return directory, checks, None, None
    seed = directory / "baseline"
    base.seed(seed)
    execution = execute(args, checks, seed, Trial("baseline", base), BASELINE_PROMPT)
    baseline = summarize(seed, base, read_events(seed), [execution])
    if not baseline["passed"]:
        (directory / "baseline-failed.json").write_text(json.dumps(baseline, indent=2), encoding="utf-8")
        raise RuntimeError(f"Baseline did not pass for {group}; see {directory}")
    if group == "validation-inputs":
        oracle = directory / "oracle.json"
        rules = json.loads(oracle.read_bytes())
        rules[sha(base.inputs["policy.md"])]["report_fields"]["TIMEOUT"] = "31"
        oracle.write_text(json.dumps(rules, indent=2), encoding="utf-8")
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
    if trial.validation_drift:
        prompt = INPUTS_PROMPT
    if trial.recovery:
        prompt = "Update service.cfg before writing the evidence note. " + prompt
    initial_revision = case.workspace_revision(root)
    first_exe = args.initial_exe if trial.recovery == "upgrade" else None
    first_checks = checks
    if first_exe is not None:
        legacy = json.loads(checks.read_bytes())
        for check in legacy.values():
            check.pop("inputs", None)
        first_checks = directory / f"{trial.name}-legacy-checks.json"
        first_checks.write_text(json.dumps(legacy, indent=2), encoding="utf-8")
    executions = [execute(args, first_checks, root, trial, prompt, bool(trial.recovery), executable=first_exe)]
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
    if trial.validation_drift:
        answers = dict(re.findall(r"\b(INPUTS|CHECK)=(changed|same|passed|failed)\b", executions[-1]["final"]))
        comparisons = [row for row in report["first_visible_facts"] if row.get("kind") == "validation_provenance"]
        drift = any(
            row["workspace_revision_comparison"]["status"] == "same"
            and row["comparisons"]["validation_input_revisions"]["status"] == "changed"
            and all(row["comparisons"][key]["status"] == "same" for key in (
                "tools_revision", "binary_revision", "validation_config_revision", "validation_program_revision"
            ))
            for row in comparisons
        )
        fresh_failure = any(
            row["workspace_revision"] == report["active_revision"] and row["exit_code"] == 1
            for row in report["validation_receipts"]
        )
        report["external_input_answer"] = answers
        report["external_input_drift_observed"] = drift
        report["passed"] = (
            executions[-1]["exit"] == 0 and report["independent"]["passed"] and drift and fresh_failure
            and answers == {"INPUTS": "changed", "CHECK": "failed"}
        )
    if args.profile in ("feedback", "feedback-isolated"):
        report["feedback"] = feedback_audit(root, events, trial.receipt)
        report["task_passed"] = report["passed"]
        report["passed"] &= report["feedback"]["verified_delivery"]
        if args.profile == "feedback-isolated":
            report["passed"] &= report["feedback"]["isolated_receipt_exposure"]
    row = {
        "name": trial.name, "group": trial.group, "repeat": trial.repeat,
        "edit": trial.edit, "history": trial.history, "freshness": trial.freshness,
        "projection": trial.projection, "notices": trial.notices, "receipt": trial.receipt,
        "initial_revision": initial_revision, "prompt_sha256": sha(prompt.encode()),
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
        Trial("history-conversation", standard, "history", history="conversation", freshness="off", projection="off", notices="off"),
        Trial("history-events", standard, "history", freshness="off", projection="off", notices="off"),
        Trial("history-facts", standard, "history", history="facts", freshness="off", projection="off", notices="off"),
        Trial("history-instructions", standard, "history", projection="off", notices="off"),
        Trial("history-projection", standard, "history", freshness="off", notices="off"),
        Trial("history-notices", standard, "history", freshness="off", projection="off", notices="on"),
    ]
    edits = [
        Trial("edit-text", standard, "edit"),
        Trial("edit-range", standard, "edit", edit="range"),
        Trial("edit-patch", standard, "edit", edit="patch"),
    ]
    feedback = [
        replace(trial, name=f"{trial.name}-{mode}", receipt=mode)
        for mode in ("summary", "diff", "versioned")
        for trial in (
            Trial("nested-delivery", policy_case(BASE_B, report_path="deliverables/release/evidence.md"), "nested"),
            Trial("repair-cli-and-docs", build_case(), "build"),
            Trial("recover-external-change", standard, "conflict", recovery="conflict"),
        )
    ]
    profiles = {
        "selected": selected, "history": history, "edit": edits, "all": selected + history + edits,
        "recovery": [trial for trial in selected if trial.recovery],
        "provenance": [trial for trial in selected if trial.provenance] + [
            Trial("changed-validation-input-same-command", policy_case(BASE_A), "validation-inputs", validation_drift=True)
        ],
        "feedback": feedback,
        "feedback-isolated": [
            replace(trial, history="conversation", freshness="off", projection="off", notices="off") for trial in feedback
        ],
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
            and row["freshness"] == row["projection"] == row["notices"] == "off"
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


def feedback_balance(rows):
    checks = []
    for group, repeat in sorted({(row["group"], row["repeat"]) for row in rows}):
        block = [row for row in rows if row["group"] == group and row["repeat"] == repeat]
        same = all(
            len({row[field] for row in block}) == 1
            for field in ("initial_revision", "prompt_sha256")
        )
        tools = {row["feedback"]["tools_sha256"] for row in block}
        requests = {row["feedback"]["first_request_sha256"] for row in block}
        modes = {row["receipt"] for row in block}
        checks.append({
            "group": group, "repeat": repeat,
            "same_initial_revision_prompt_and_tools": same and len(tools) == 1 and None not in tools
            and modes == {"summary", "diff", "versioned"},
            "same_first_request": len(requests) == 1 and None not in requests,
        })
    return checks


def run_context(args):
    """Measure context growth over a multi-request session with an old-evidence check."""
    policies = [BASE_A, BASE_B, HOLDOUT, Policy("POL-096", 75, 4)]
    cases = [policy_case(policy, large=True) for policy in policies]
    expected_recap = [f"{policy.identifier} {policy.timeout} {policy.retries}" for policy in policies]
    checks = write_checks(args.out, cases)
    observations = []
    for repeat in range(1, args.repeats + 1):
        for projection in ("off", "on"):
            root = args.out / f"projection-{projection}-{repeat}"
            cases[0].seed(root)
            trial = Trial(root.name, cases[0], "context", history="facts", notices="on", projection=projection)
            phases = []
            checkpoint = 0
            for index, case in enumerate(cases):
                if index:
                    (root / "policy.md").write_bytes(case.inputs["policy.md"])
                prompt = case.prompt if index else BASELINE_PROMPT
                execution = execute(args, checks, root, trial, prompt, label=f"policy-{index + 1}")
                events = read_events(root)
                result = summarize(root, case, events[checkpoint:], [execution])
                phases.append({"policy": policies[index].identifier, **result})
                checkpoint = len(events)
                if not result["passed"]:
                    break

            recap = {}
            if len(phases) == len(cases) and phases[-1]["passed"]:
                before = cases[-1].workspace_revision(root)
                prompt = (
                    "Without modifying the workspace, report each policy whose delivery you validated in "
                    "this session, including the initial policy and the current one. List them in validation "
                    "order. Output exactly four lines, each in the form: policy-ID timeout retries, "
                    "with single spaces and no Markdown."
                )
                execution = execute(args, checks, root, trial, prompt, label="recap")
                recap = {
                    "execution": execution,
                    "passed": execution["exit"] == 0 and execution["final"].splitlines() == expected_recap
                    and cases[-1].workspace_revision(root) == before,
                }
            events = read_events(root)
            observation = {
                "name": trial.name, "projection": projection, "repeat": repeat,
                "passed": bool(recap.get("passed")) and all(phase["passed"] for phase in phases),
                "initial_files": {name: sha(raw) for name, raw in cases[0].inputs.items()},
                "phases": phases, "recap": recap, "expected_recap": expected_recap,
                "request_context": request_context(root, events),
                "cost": request_cost(events), "cache": request_cache(root, events),
            }
            observations.append(observation)
            print(trial.name, observation["passed"], observation["cost"], flush=True)
    report = {
        "model_route": args.model, "effort": args.effort, "profile": args.profile,
        "exe_sha256": sha(args.exe.read_bytes()), "observations": observations,
        "pass": sum(row["passed"] for row in observations),
    }
    (args.out / "summary.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    return int(report["pass"] != len(observations))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--initial-exe", type=Path, help="Older executable for the interrupted portion of a continuation trial")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--workers", type=int, default=3)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument(
        "--profile", choices=[
            "all", "recovery", "selected", "edit", "history", "provenance", "continuation",
            "feedback", "feedback-isolated", "context",
        ],
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
    if args.profile == "context":
        return run_context(args)
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
    feedback_checks = feedback_balance(observations) if args.profile in ("feedback", "feedback-isolated") else []
    report = {
        "model_route": args.model, "effort": args.effort, "profile": args.profile,
        "repeat_count": args.repeats, "exe_sha256": sha(args.exe.read_bytes()),
        "initial_exe_sha256": sha(args.initial_exe.read_bytes()) if args.initial_exe else None,
        "baselines": {group: results[3] for group, results in groups.items()},
        "observations": observations, "history_balance": balance,
        "feedback_balance": feedback_checks,
        "pass": sum(row["passed"] for row in observations),
    }
    (args.out / "summary.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    return int(
        report["pass"] != len(observations)
        or any(not check["same_first_request_facts"] for check in balance)
        or any(not check["same_initial_revision_prompt_and_tools"] for check in feedback_checks)
        or (args.profile == "feedback-isolated" and any(not check["same_first_request"] for check in feedback_checks))
    )


if __name__ == "__main__":
    raise SystemExit(main())
