"""Re-run the two published fixtures with the compact session profile."""
import argparse
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
import getpass
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runner-dir", type=Path, required=True)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--case", choices=["markdown-links", "sample-report"])
    parser.add_argument("--repeats", type=int, default=1)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    args.out.mkdir(parents=True)
    sys.path.insert(0, str(args.runner_dir.resolve()))
    import compare_pi_public as paired

    paired.SH = args.exe.resolve()
    repo = Path(__file__).resolve().parents[3]
    cases = [args.case] if args.case else ["markdown-links", "sample-report"]
    default_launch = paired.launch
    default_models = paired.Models

    class CompactModels(default_models):
        def __init__(self):
            super().__init__()
            self.previous = None

        def begin(self, raw, body):
            row = super().begin(raw, body)
            previous = self.previous
            self.previous = body
            row["previous_input_count"] = len(previous["input"]) if previous else None
            row["append_only"] = (
                previous["input"] == body["input"][:len(previous["input"])]
                and previous["instructions"] == body["instructions"]
                and previous["tools"] == body["tools"]
            ) if previous else None
            row["replayed_output_bytes"] = sum(
                len(item.get("output", "").encode())
                for item in body["input"] if item.get("type") == "function_call_output"
            )
            start = row["previous_input_count"] or 0
            row["new_budget_hints"] = sum(
                "\n[Budget for this task:" in item.get("output", "")
                for item in body["input"][start:] if item.get("type") == "function_call_output"
            )
            return row

    def compact_launch(arm, url, prompt, phase):
        command = default_launch(arm, url, prompt, phase)
        return command + ["--tool-profile", "compact"]

    paired.Models = CompactModels
    paired.launch = compact_launch
    token = getpass.getpass("OpenCode Go test credential (hidden): ")
    if not re.fullmatch(r"oc_sk_[A-Za-z0-9_]+", token):
        raise SystemExit("Missing or malformed test credential")
    jobs = [
        (case, repeat, args.out / (case if args.repeats == 1 else f"{case}-{repeat:02d}"))
        for case in cases for repeat in range(1, args.repeats + 1)
    ]

    def run_trial(job):
        case, repeat, directory = job
        rows = paired.run_arm(token, "sourceharness", case, repeat, 2, directory)
        workspace = directory / "sourceharness" / "workspace"
        events = paired.session_events(workspace)
        blobs = workspace / ".sourceharness/sessions/work/blobs"
        runs = [event["run"] for event in events if event["kind"] == "user"]
        outcomes = {
            event["data"]["operation"]: event["data"].get("code")
            for event in events if event["kind"] == "tool_result"
        }
        for row, run in zip(rows, runs):
            row["tool_errors"] = dict(Counter(
                event["data"]["code"] for event in events if event["run"] == run
                and event["kind"] == "tool_result" and event["data"].get("code")
            ))
            rounds = defaultdict(list)
            for event in events:
                if event["run"] != run or event["kind"] != "tool_attempt":
                    continue
                data = event["data"]
                raw = (blobs / data["arguments"]).read_bytes()
                call = {
                    "name": data["name"], "argument_bytes": len(raw),
                    "code": outcomes.get(data["operation"]),
                }
                if data["name"] == "read":
                    try:
                        arguments = json.loads(raw)
                    except (ValueError, UnicodeError):
                        arguments = None
                    if isinstance(arguments, dict) and isinstance(arguments.get("paths"), list):
                        call["batch_paths"] = len(arguments["paths"])
                rounds[data["round"]].append(call)
            row["tool_rounds"] = [{"round": number, "calls": calls} for number, calls in rounds.items()]
        return {"case": case, "pair": repeat, "arm": "compact", "expected_phases": 2, "phases": rows}

    with ThreadPoolExecutor(max_workers=min(3, len(jobs))) as workers:
        trials = list(workers.map(run_trial, jobs))
    result = {
        "model": paired.MODEL,
        "effort": paired.EFFORT,
        "endpoint": paired.ENDPOINT,
        "limits": {"requests_per_phase": paired.MAX_ATTEMPTS, "phase_seconds": paired.PHASE_TIMEOUT},
        "source_base": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "executable_sha256": hashlib.sha256(args.exe.read_bytes()).hexdigest(),
        "baseline": "pi-comparison-2026-11-27, pair 1 of each fixture",
        "trials": trials,
    }
    paired.save(args.out / "results.json", result)
    with (args.out / "requests.jsonl").open("w", encoding="utf-8") as stream:
        for case, repeat, directory in jobs:
            requests = json.loads((directory / "sourceharness" / "requests.json").read_text())
            for row in requests:
                stream.write(json.dumps(dict(case=case, trial=repeat, arm="compact", **row)) + "\n")


if __name__ == "__main__":
    main()
