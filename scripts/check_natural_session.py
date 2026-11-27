"""Independent behavior checks for a delivered natural-session workspace."""

import csv
from decimal import Decimal, InvalidOperation, ROUND_HALF_UP
import json
import os
from pathlib import Path
import random
import re
import shlex
import subprocess
import sys
import tempfile

from natural_session_cases import CSV_HEADER, MAINTAINER_NOTE, MARKDOWN_LINKS, WARMUP
from session_eval_cases import sha, workspace_files


SUMMARY_FIELDS = ["device_id", "count", "min", "max", "mean"]
MARKDOWN_LINK = re.compile(r"\[([^\]]+)\]\(([^)\s]+)\)")


def expected_summary(paths, phase):
    states = {"OK"} if phase < 3 else {"OK", "VALID"}
    seen, devices, bad = set(), {}, set()
    for path in paths:
        with path.open(newline="", encoding="utf-8") as stream:
            for row in csv.DictReader(stream):
                sample = row["sample_id"]
                if row["status"] not in states or (phase >= 2 and sample in seen):
                    continue
                seen.add(sample)
                try:
                    value = Decimal(row["value"])
                    if not value.is_finite():
                        raise InvalidOperation
                except InvalidOperation:
                    bad.add(sample)
                    continue
                devices.setdefault(row["device_id"], []).append(value)
    increment = Decimal(1).scaleb(-2 if phase < 3 else -3)
    rows = {
        device: (len(values), min(values), max(values),
                 (sum(values) / len(values)).quantize(increment, rounding=ROUND_HALF_UP))
        for device, values in devices.items()
    }
    return rows, bad


def compare_output(path, expected, precision):
    with path.open(newline="", encoding="utf-8-sig") as stream:
        reader = csv.DictReader(stream)
        if reader.fieldnames != SUMMARY_FIELDS:
            return False
        actual = {}
        for row in reader:
            device = row["device_id"]
            if device in actual or not re.fullmatch(rf"[+-]?\d+\.\d{{{precision}}}", row["mean"]):
                return False
            actual[device] = (int(row["count"]), *(Decimal(row[key]) for key in ("min", "max", "mean")))
    return actual == expected


def run_cli(root, arguments):
    # Run the delivered program without provider credentials.
    env = {key: val for key, val in os.environ.items() if not key.endswith(("_KEY", "_TOKEN"))}
    return subprocess.run([sys.executable, "-B", "sample_report.py", *arguments],
                          cwd=root, capture_output=True, timeout=6, env=env)


def report_check(root, inputs, output, phase, issues=None, arguments=None):
    expected, bad = expected_summary([root / name for name in inputs], phase)
    target = root / output
    target.unlink(missing_ok=True)
    if arguments is None:
        arguments = [part for name in inputs for part in ("--input", name)] + ["--output", output]
        if issues:
            arguments += ["--issues", issues]
    if issues:
        (root / issues).unlink(missing_ok=True)
    result = run_cli(root, arguments)
    if result.returncode:
        details = result.stderr.decode("utf-8", "replace").strip().splitlines()
        return "command failed" + (f" ({details[-1][:220]})" if details else "")
    if not target.is_file() or not compare_output(target, expected, 2 if phase < 3 else 3):
        return "summary values or CSV format differ"
    if issues:
        with (root / issues).open(newline="", encoding="utf-8-sig") as stream:
            rows = list(csv.DictReader(stream))
        if ({row.get("sample_id") for row in rows} != bad
                or len(rows) != len(bad) or any(not (row.get("reason") or "").strip() for row in rows)):
            return "issues CSV must identify each bad value and its reason"
    return None


def readme_example(root, phase):
    readme = (root / "README.md").read_text(encoding="utf-8")
    examples = 0
    for line in readme.splitlines():
        line = line.strip().strip("\x60")
        prefix = re.match(r"^(?:python(?:3|\.exe)?|py)\s+(?:-B\s+)?(?:\./)?sample_report\.py\s+", line)
        if not prefix:
            continue
        arguments = shlex.split(line[prefix.end():])
        options = {flag: [] for flag in ("--input", "--output", "--issues")}
        if len(arguments) % 2:
            raise ValueError("An example needs --input and --output option values")
        for flag, path in zip(arguments[::2], arguments[1::2]):
            target = Path(path)
            if flag not in options or target.is_absolute() or ".." in target.parts or target.drive:
                raise ValueError("The README example must use project-relative input and output paths")
            options[flag].append(path)
        if not options["--input"] or len(options["--output"]) != 1 or len(options["--issues"]) > 1:
            raise ValueError("The README example needs at least one input and one output")
        output = options["--output"][0]
        issues = next(iter(options["--issues"]), None)
        if any(path.startswith("data/") or Path(path).suffix.lower() != ".csv" for path in [output, *options["--issues"]]):
            raise ValueError("The README example must write report CSVs outside data/")
        error = report_check(root, options["--input"], output, phase, issues, arguments)
        if error:
            raise ValueError(f"README example {examples + 1}: {error}")
        examples += 1
    if not examples:
        raise ValueError("README has no runnable sample_report.py example")
    return {"examples_run": examples}


def sample_checks(root, phase):
    failures, observed = {}, {}

    def check(name, fn):
        try:
            result = fn()
            if isinstance(result, str):
                failures[name] = result
            elif isinstance(result, dict):
                observed[name] = result
        except (OSError, ValueError, TypeError, ArithmeticError, subprocess.TimeoutExpired) as error:
            failures[name] = str(error)[:360]

    check("single_file", lambda: report_check(root, ["data/day1.csv"], "single.csv", phase))
    empty = root / "empty-valid.csv"
    empty.write_bytes((CSV_HEADER + "E-1,FT-87,1.0,OFFLINE\n").encode())
    check("no_valid_rows", lambda: report_check(root, ["empty-valid.csv"], "empty-summary.csv", phase))
    if phase >= 2:
        inputs = ["data/day1.csv", "data/day2.csv"]
        if phase >= 3:
            inputs.append("data/day3.csv")
        check("combined", lambda: report_check(root, inputs, "combined.csv", phase, "issues.csv"))

        rows = ["H-4,FT-78,1.0045,OK", "H-5,FT-78,1.0055,OK", "H-7,FT-79,not-a-number,OK",
                "H-8,FT-79,NaN,OK", "H-9,FT-79,-5.5555,VALID", "H-10,FT-80,9,OFFLINE"]
        random.Random(8421).shuffle(rows)
        for name, batch in (("holdout-a.csv", rows[:3]), ("holdout-b.csv", rows[3:] + [rows[0]])):
            (root / name).write_bytes((CSV_HEADER + "\n".join(batch) + "\n").encode())
        check("unseen_order_and_values", lambda: report_check(
            root, ["holdout-a.csv", "holdout-b.csv"], "holdout-summary.csv", phase, "holdout-issues.csv"))

        (root / "conflict-a.csv").write_bytes((CSV_HEADER + "CLASH-4,FT-77,1.0,OK\n").encode())
        (root / "conflict-b.csv").write_bytes((CSV_HEADER + "CLASH-4,FT-77,2.0,OK\n").encode())

        def check_conflict():
            result = run_cli(root, ["--input", "conflict-a.csv", "--input", "conflict-b.csv",
                                    "--output", "conflict-summary.csv"])
            message = (result.stdout + result.stderr).decode("utf-8", "replace")
            if not result.returncode or "CLASH-4" not in message or not re.search(r"conflict|duplicate|冲突|重复", message, re.I):
                return "a conflicting ID must cause a nonzero exit and a message identifying the ID"

        check("conflict", check_conflict)

    def check_docs():
        usage = (root / "docs/usage.md").read_text(encoding="utf-8")
        keys = ["sample_id", "device_id", "status", "OK", "--input", "--output"]
        if phase >= 2:
            keys.append("--issues")
        if phase >= 3:
            keys.append("VALID")
            if not re.search(r"\b(?:three|3)\s+decimal|三位小数", usage, re.I):
                return "usage should explain the current mean precision"
            if MAINTAINER_NOTE not in (root / "README.md").read_text(encoding="utf-8"):
                return "maintainer note was changed or removed"
        if any(key not in usage for key in keys):
            return "usage is missing input, output, or rule details"

    check("usage", check_docs)
    check("readme_examples", lambda: readme_example(root, phase))
    return failures, observed


def audit_note(raw):
    text = raw.decode("utf-8", "replace")
    sample, _, value, _ = WARMUP.rstrip("\n").split(",")
    old_rules = re.search(r"\b(?:first|original|old|initial|v1|phase\s*1)\b|第一|旧版|原始", text, re.I)
    new_rules = re.search(r"\b(?:current|updated|new|v3|phase\s*3)\b|当前|新版", text, re.I)
    return (
        sample in text and value in text and "WARMUP" in text
        and bool(re.search(r"excluded|not (?:counted|included|valid|eligible)|排除|不计|无效", text, re.I))
        and bool(old_rules and new_rules and "OK" in text and "VALID" in text
                 and re.search(r"\b(?:two|2)\s+decimal|两位小数|2\s*位小数", text, re.I)
                 and re.search(r"\b(?:three|3)\s+decimal|三位小数|3\s*位小数", text, re.I))
    )


def link_checks(root, phase):
    failures = {}
    targets = {
        "README.md": {"Operator entry": "docs/index.md", "Start the instrument": "guides/start.md"},
        "docs/index.md": {"Getting started": "guides/start.md",
                          "Sampling fields": f"references/{'schema' if phase == 1 else 'fields'}.md"},
    }
    for name, expected in targets.items():
        path = root / name
        links = MARKDOWN_LINK.findall(path.read_text(encoding="utf-8")) if path.is_file() else []
        actual = {label: (path.parent / link.split("#", 1)[0]).resolve() for label, link in links}
        if len(links) != len(expected) or actual != {label: root / target for label, target in expected.items()}:
            failures[name] = "link targets or labels differ"
        elif any(not path.is_file() for path in actual.values()):
            failures[name] = "link target is missing"
    return failures


def grade_workspace(root, config):
    files = workspace_files(root)
    phase = config["phase"]
    protected = [name for name, digest in config["protected"].items()
                 if name not in files or sha(files[name]) != digest]
    failures, observed = {}, {}
    if config["case"] == MARKDOWN_LINKS.name:
        failures = link_checks(root.resolve(), phase)
    elif phase == 4:
        note = "docs/change-note.md"
        if not audit_note(files.get(note, b"")):
            failures[note] = "the old record, exclusion reason, or rule comparison is incomplete"
        unexpected = set(files) - set(config["protected"]) - {note}
        if unexpected:
            failures["unexpected_paths"] = sorted(unexpected)
    else:
        if set(name for name in files if name.startswith("data/")) - set(config["protected"]):
            failures["unexpected_data"] = "the source data directory contains additional files"
        with tempfile.TemporaryDirectory(prefix="natural-check-") as directory:
            work = Path(directory)
            for name, content in files.items():
                path = work / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)
            checks, observed = sample_checks(work, phase)
            failures.update(checks)
    return {"passed": not protected and not failures, "changed_protected_paths": protected,
            "failures": failures, "observed": observed}


if __name__ == "__main__":
    report = grade_workspace(Path.cwd(), json.loads(Path(sys.argv[1]).read_bytes()))
    print(json.dumps(report, ensure_ascii=True, separators=(",", ":")))
    raise SystemExit(int(not report["passed"]))
