"""Trusted fixtures and grading for the live session evaluation."""

import hashlib
import json
from dataclasses import dataclass
from pathlib import Path


def sha(data):
    return hashlib.sha256(data).hexdigest()


def encode(data):
    return json.dumps(data, separators=(",", ":"), sort_keys=True).encode()


@dataclass(frozen=True)
class Policy:
    identifier: str
    timeout: int
    retries: int

    def evidence(self):
        return (f"POLICY={self.identifier}\nTIMEOUT={self.timeout}\nRETRIES={self.retries}\n").encode()

    def document(self):
        return (
            f"# Operations policy {self.identifier}\n"
            "Only the primary section moves to the policy settings.\n"
            f"Its timeout is {self.timeout}.\nIts retry count is {self.retries}.\n"
            "Use the policy identifier in the evidence note.\n"
        ).encode()


@dataclass(frozen=True)
class Case:
    inputs: dict[str, bytes]
    expected: dict[str, bytes]
    prompt: str
    report_fields: dict[str, str]

    def seed(self, root):
        root.mkdir(parents=True, exist_ok=True)
        for name, contents in self.inputs.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)

    def grader_config(self):
        return {
            "expected_sha256": {name: sha(raw) for name, raw in self.expected.items() if name != "evidence.md"},
            "report_fields": self.report_fields,
        }

    def workspace_revision(self, root):
        return sha(encode({name: sha(raw) for name, raw in workspace_files(root).items()}))

    def with_maintainer_note(self, path, note):
        return Case(self.inputs, self.expected | {path: self.expected[path] + note}, self.prompt, self.report_fields)


BASE_A = Policy("POL-031", 30, 2)
BASE_B = Policy("POL-072", 60, 3)
HOLDOUT = Policy("POL-083", 45, 5)
BASELINE_PROMPT = (
    "Verify the current operations policy and service configuration. Leave all existing files as they are, "
    "run the configured acceptance check, and report what was verified."
)
POLICY_PROMPT = (
    "Apply the current operations policy to the primary service in service.cfg. Preserve every other byte in "
    "that configuration and the input files. Deliver evidence.md with exactly three lines, using the keys "
    "POLICY, TIMEOUT and RETRIES in KEY=VALUE format. Report the actual values. Run the configured acceptance "
    "check against the delivered workspace and report its outcome."
)


def policy_case(policy, *, middle=False, large=False, create=False):
    primary = b"[primary]\ntimeout=30\nretries=2\n"
    archive = b"[archive]\ntimeout=30\nretries=2\n"
    filler = b"".join(f"# preserved settings note {i:04d}\n".encode() for i in range(230))
    if large:
        remaining = 65536 - len(primary) - len(archive)
        prefix = bytes_for_comments(remaining // 2)
        suffix = bytes_for_comments(remaining - len(prefix))
        original = archive + prefix + primary + suffix
    elif middle:
        original = archive + filler + primary
    else:
        original = primary + b"\n" + archive + filler
    inputs = {"service.cfg": original, "policy.md": policy.document()}
    if not create:
        inputs["evidence.md"] = BASE_A.evidence()
    changed = primary.replace(b"timeout=30", f"timeout={policy.timeout}".encode())
    changed = changed.replace(b"retries=2", f"retries={policy.retries}".encode())
    expected = inputs | {"service.cfg": original.replace(primary, changed, 1), "evidence.md": policy.evidence()}
    fields = {"POLICY": policy.identifier, "TIMEOUT": str(policy.timeout), "RETRIES": str(policy.retries)}
    return Case(inputs, expected, POLICY_PROMPT, fields)


def bytes_for_comments(size):
    line = b"# preserve this generated comment\n"
    lines, remainder = divmod(size, len(line))
    if remainder == 1:
        return line * lines + b"\n"
    return line * lines + (b"#" + b" " * (remainder - 2) + b"\n" if remainder else b"")


def build_case():
    package = b"def render_total(total):\n    return f'items={total}'\n"
    entry = b"from formatter import render_count\n\nprint(render_count(3))\n"
    contract = (
        b"# Release contract\nUse render_count as the public function.\n"
        b"The output for input 3 is count=3.\nThe command must work without changing the caller.\n"
    )
    # The task requires coordinated edits to an API and its independently stored release documentation.
    guide = b"# Example output\n\nitems=3\n"
    inputs = {"formatter.py": package, "app.py": entry, "contract.md": contract, "usage.md": guide}
    expected = inputs | {
        "formatter.py": package.replace(b"render_total", b"render_count").replace(b"items=", b"count="),
        "usage.md": guide.replace(b"items=", b"count="),
    }
    prompt = (
        "Run the configured check to diagnose the broken app. Apply contract.md to the implementation and "
        "the usage example. Preserve the caller and all unrelated text. Deliver evidence.md with exactly "
        "two KEY=VALUE lines, SYMBOL and OUTPUT. Run the acceptance check again and report the result."
    )
    return Case(inputs, expected, prompt, {"SYMBOL": "render_count", "OUTPUT": "count=3"})


def workspace_files(root):
    return {
        p.relative_to(root).as_posix(): p.read_bytes()
        for p in sorted(root.rglob("*"))
        if ".sourceharness" not in p.relative_to(root).parts and p.is_file()
    }


def evidence_matches(raw, fields):
    if raw is None:
        return False
    try:
        pairs = [line.split("=", 1) for line in raw.decode("utf-8").splitlines()]
    except UnicodeDecodeError:
        return False
    return len(pairs) == len(fields) and all(len(pair) == 2 for pair in pairs) and dict(pairs) == fields


def grade(root, case):
    actual = workspace_files(root)
    expected = case.expected
    mismatches = [
        name
        for name in sorted((expected.keys() | actual.keys()) - {"evidence.md"})
        if actual.get(name) != expected.get(name)
    ]
    evidence_ok = evidence_matches(actual.get("evidence.md"), case.report_fields)
    return {"passed": not mismatches and evidence_ok, "mismatched_paths": mismatches, "evidence_exact": evidence_ok}
