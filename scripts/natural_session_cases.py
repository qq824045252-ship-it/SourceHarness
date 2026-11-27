"""Projects, user turns, and permitted external changes for continuous sessions."""

from dataclasses import dataclass


@dataclass(frozen=True)
class Scenario:
    name: str
    initial: dict[str, str]
    prompts: tuple[str, ...]


CSV_HEADER = "sample_id,device_id,value,status\n"
WARMUP = "S-WARM-017,FT-02,812.037,WARMUP\n"
MAINTAINER_NOTE = "> Maintainer note: Keep degree-of-precision decisions in the report policy."


def policy(states, precision):
    return f"""# Sampling report policy

- Valid statuses: {states}.
- Compute statistics with decimal arithmetic.
- Round the mean to {precision} decimal places using round half up.
- Report instruments that have valid numeric samples. With no valid samples, write only the CSV header.
- Keep all input files unchanged.
"""


SAMPLE_REPORT = Scenario(
    "sample-report",
    {
        "sample_report.py": """import argparse

from reporting.csv_io import read_rows
from reporting.summary import summarize


def main():
    parser = argparse.ArgumentParser(description="Summarize instrument samples")
    parser.add_argument("--input", required=True, help="CSV to read")
    parser.add_argument("--output", required=True, help="Summary CSV to write")
    args = parser.parse_args()
    print(summarize(read_rows(args.input)))


if __name__ == "__main__":
    main()
""",
        "reporting/__init__.py": '"""Instrument sampling reports."""\n',
        "reporting/csv_io.py": """import csv


def read_rows(path):
    with open(path, newline="", encoding="utf-8") as stream:
        return list(csv.DictReader(stream))
""",
        "reporting/summary.py": "def summarize(rows):\n    return {'rows': len(rows)}\n",
        "README.md": """# Instrument sampling reports

The input examples are in data/day1.csv and data/day2.csv. See docs/report_policy.md
for the business rules.

    python sample_report.py --input data/day1.csv --output summary.csv

The summary CSV should use the columns device_id,count,min,max,mean.
""",
        "docs/report_policy.md": policy("OK", 2),
        "data/day1.csv": CSV_HEADER + """S-001,FT-01,1.00,OK
S-002,FT-01,1.01,OK
S-003,FT-02,-2.005,OK
""" + WARMUP + """S-004,FT-02,7.25,VALID
S-005,FT-03,93.3,OFFLINE
""",
        "data/day2.csv": CSV_HEADER + """S-002,FT-01,1.01,OK
S-006,FT-01,3.205,OK
S-007,FT-02,broken,OK
S-008,FT-02,1.999,VALID
S-009,FT-03,0.75,OK
S-010,FT-01,90.5,OFFLINE
""",
    },
    (
        """Turn this project skeleton into a usable instrument sampling report. Follow docs/report_policy.md.
Support specifying an input file and an output CSV path. Report the valid sample count, minimum,
maximum, and mean per instrument. Keep the raw inputs and policy intact. Update the runnable example
in README.md and add docs/usage.md explaining the inputs, output, and rules. Tell me what you verified.""",
        """An operator needs to process day1.csv and day2.csv together. Allow repeating --input while
keeping the first release's single-file usage. Count identical records with the same sample_id once.
Report a clear error for a sample_id with conflicting contents. Skip a bad numeric value and report the
sample_id and reason in an optional issues CSV requested with --issues. Keep the valid-status rule.
Update the usage instructions and tell me what you verified.""",
        """The current policy now accepts VALID records as well as OK and uses three decimal places
for the mean. Please make the report work across all three days, while keeping deduplication, conflict
detection, and bad-value reporting. Update the documentation and keep the maintainer note that was just
added to the README. Tell me what you verified on the delivered version.""",
        """Audit the first release. What were the sample_id and original value of the WARMUP row in
the original day1.csv, and why was it excluded? Compare the valid statuses and mean precision in the
first and current releases in docs/change-note.md. Include the historical snapshot references or event
numbers supporting the old record and policy. Keep the current program, data, and existing docs intact.
Tell me what you verified.""",
    ),
)

DAY3 = CSV_HEADER + """S-008,FT-02,1.999,VALID
S-011,FT-01,4.125,VALID
S-012,FT-02,-4.50,OK
S-013,FT-02,bad,VALID
S-014,FT-04,5.0005,VALID
S-015,FT-04,123.4,OFFLINE
"""

MARKDOWN_LINKS = Scenario(
    "markdown-links",
    {
        "README.md": """# Operator kit

- [Operator entry](docs/index.md)
- [Start the instrument](guides/quick-start.md)
""",
        "docs/index.md": """# Operator entry

- [Getting started](../guides/quick-start.md)
- [Sampling fields](../references/schema.md)
""",
        "guides/start.md": "# Start\n\nFollow the station startup sequence.\n",
        "references/schema.md": "# Sampling fields\n\nEach sample has an instrument identifier.\n",
    },
    (
        """Repair the internal Markdown links in the operator kit. Keep the link labels and content of
the linked documents. Tell me what you verified.""",
        """The sampling fields reference was moved. Update the documentation links for the new
location, preserve the getting-started links and linked document contents, and tell me what you verified.""",
    ),
)

CASES = {case.name: case for case in (SAMPLE_REPORT, MARKDOWN_LINKS)}


def seed(case, root):
    root.mkdir()
    for name, text in case.initial.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(text.encode())


def apply_external_changes(case, root, phase):
    if case.name == SAMPLE_REPORT.name:
        if phase == 3:
            (root / "docs/report_policy.md").write_bytes(policy("OK and VALID", 3).encode())
            (root / "data/day3.csv").write_bytes(DAY3.encode())
            with (root / "README.md").open("ab") as stream:
                stream.write(("\n\n" + MAINTAINER_NOTE + "\n").encode())
            return ["docs/report_policy.md", "data/day3.csv", "README.md"]
        if phase == 4:
            day1 = root / "data/day1.csv"
            raw = day1.read_bytes()
            if raw.count(WARMUP.encode()) != 1:
                raise ValueError("Cannot apply the phase-4 change to the expected WARMUP row")
            day1.write_bytes(raw.replace(WARMUP.encode(), b"", 1))
            return ["data/day1.csv"]
    elif case.name == MARKDOWN_LINKS.name and phase == 2:
        old, new = "references/schema.md", "references/fields.md"
        (root / old).rename(root / new)
        return [old, new]
    return []


def protected_paths(case, phase, paths):
    if case.name == SAMPLE_REPORT.name:
        if phase == 4:
            return [name for name in paths if name != "docs/change-note.md"]
        return ["docs/report_policy.md", *(name for name in paths if name.startswith("data/"))]
    fields = "references/schema.md" if phase == 1 else "references/fields.md"
    return ["guides/start.md", fields]
