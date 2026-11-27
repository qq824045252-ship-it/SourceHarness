"""Re-run the two published fixtures with the compact session profile."""
import argparse
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
    args = parser.parse_args()
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
            return row

    def compact_launch(arm, url, prompt, phase):
        command = default_launch(arm, url, prompt, phase)
        return command + ["--tool-profile", "compact"]

    paired.Models = CompactModels
    paired.launch = compact_launch
    token = getpass.getpass("OpenCode Go test credential (hidden): ")
    if not re.fullmatch(r"oc_sk_[A-Za-z0-9_]+", token):
        raise SystemExit("Missing or malformed test credential")
    with ThreadPoolExecutor(max_workers=len(cases)) as workers:
        reports = list(workers.map(
            lambda case: paired.run_arm(token, "sourceharness", case, 1, 2, args.out / case),
            cases,
        ))
    result = {
        "model": paired.MODEL,
        "effort": paired.EFFORT,
        "endpoint": paired.ENDPOINT,
        "limits": {"requests_per_phase": paired.MAX_ATTEMPTS, "phase_seconds": paired.PHASE_TIMEOUT},
        "source_base": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "executable_sha256": hashlib.sha256(args.exe.read_bytes()).hexdigest(),
        "baseline": "pi-comparison-2026-11-27, pair 1 of each fixture",
        "trials": [
            {"case": case, "pair": 1, "arm": "compact", "expected_phases": 2, "phases": rows}
            for case, rows in zip(cases, reports)
        ],
    }
    paired.save(args.out / "results.json", result)
    with (args.out / "requests.jsonl").open("w", encoding="utf-8") as stream:
        for case in cases:
            requests = json.loads((args.out / case / "sourceharness" / "requests.json").read_text())
            for row in requests:
                stream.write(json.dumps(dict(case=case, arm="compact", **row)) + "\n")


if __name__ == "__main__":
    main()
