"""Acceptance check invoked from a SourceHarness workspace snapshot."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys


def main():
    variants = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
    policy = Path("policy.md")
    identity = hashlib.sha256(policy.read_bytes()).hexdigest() if policy.is_file() else "no-policy"
    config = variants.get(identity)
    if config is None:
        print("policy.md: unknown input version")
        return 1
    problems = []
    if config.get("app_output") is not None:
        app = subprocess.run([sys.executable, "-B", "app.py"], capture_output=True, timeout=10)
        if app.returncode != 0:
            problems.append(app.stderr.decode("utf-8", "replace").strip())
        elif app.stdout.decode("utf-8", "replace").strip() != config["app_output"]:
            problems.append("Program output does not match contract.md")
    for name, expected in config["expected_sha256"].items():
        file = Path(name)
        digest = hashlib.sha256(file.read_bytes()).hexdigest() if file.is_file() else None
        if digest != expected and digest != config.get("acceptable_maintainer_change", {}).get(name, expected):
            problems.append(f"{name}: content differs from the expected result")
    report = Path("evidence.md")
    rows = [line.split("=", 1) for line in report.read_text(encoding="utf-8").splitlines()] if report.is_file() else []
    fields = config["report_fields"]
    if len(rows) != len(fields) or any(len(row) != 2 for row in rows) or dict(rows) != fields:
        problems.append("evidence.md: missing or incorrect reported values")
    print("\n".join(problems) if problems else "PASS: outputs and reported values match")
    return bool(problems)


if __name__ == "__main__":
    sys.exit(main())
