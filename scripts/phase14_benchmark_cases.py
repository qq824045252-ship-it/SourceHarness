"""Reproducible Phase 1-4 workspaces for the 200-task evaluation.

Every case is a deterministic script of CLI actions (agent runs, branch, worker spawn and
worker merge) over a generated workspace, together with the external checks and the
expectations the runner scores. Four categories of 25 cases keep the corpus at 100.
"""

from dataclasses import dataclass
from hashlib import sha256
import json
import random


SIZES = (1024, 4096, 16384)
WORDS = ("alder", "birch", "cedar", "elm", "fir", "hazel", "juniper", "maple", "olive", "rowan")


@dataclass
class Case:
    id: str
    category: str
    shape: str
    variant: int
    files: dict
    checks: dict
    actions: list


def encode(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def fixture(value):
    return encode(value).decode() + "\n"


def apply(text, pairs):
    for old, new in pairs:
        if text.count(old) != 1:
            raise ValueError(f"anchor {old!r} appears {text.count(old)} times")
        text = text.replace(old, new)
    return text


def pad(rng, lines, target):
    while len("\n".join(lines)) + 1 < target:
        lines.append(f"# {rng.randrange(16 ** 6):06x} {rng.choice(WORDS)} {rng.choice(WORDS)} {rng.choice(WORDS)}")
    return lines


def spec(files, exact=None, values=None, allow=()):
    exact, values = exact or {}, values or {}
    allowed = set(exact) | set(values) | set(allow)
    result = {"exact": exact, "json": values,
              "protected": {path: sha256(raw.encode("utf-8")).hexdigest()
                            for path, raw in files.items() if path not in allowed}}
    if allow:
        result["allow"] = sorted(allowed - set(exact) - set(values))
    return result


def changesets(pairs_by_file):
    return [["edit", {"files": [{"path": path,
                                 "changes": [{"old_text": old, "new_text": new} for old, new in pairs]}
                                for path, pairs in pairs_by_file.items()]}]]


def single(path, pairs):
    return [["edit", {"path": path, "changes": [{"old_text": old, "new_text": new} for old, new in pairs]}]]


def agent(root, session, prompt, script, expect, resume=False):
    return {"kind": "agent", "root": root, "session": session, "prompt": prompt,
            "script": script, "expect": expect, "resume": resume}


def spawn(name):
    return {"kind": "spawn", "name": name}


def rebase(worker, expect):
    return {"kind": "rebase", "worker": worker, "expect": expect}


def merge(worker, expect):
    return {"kind": "merge", "from": worker, "session": "main", "expect": expect}


def branch(source, target, event, lesson, expect):
    return {"kind": "branch", "from": source, "to": target, "event": event, "lesson": lesson, "expect": expect}


def package(rng, variant, spaced_cli=False):
    token = f"svc-{rng.randrange(100000, 999999)}"
    target = SIZES[variant % 3]
    call = "compute( int(argv[0]) )" if spaced_cli else "compute(int(argv[0]))"
    files = {
        "pkg/__init__.py": "",
        "pkg/api.py": "\n".join(pad(rng, [f'"""Arithmetic helpers for {token}."""', "",
                                          "def compute(x):", '    """Double the value."""',
                                          "    return x * 2", ""], target)) + "\n",
        "pkg/service_a.py": "\n".join(pad(rng, ["from pkg.api import compute", "", "",
                                                "def run_a(items):",
                                                "    return [compute(item) for item in items]", ""], target)) + "\n",
        "pkg/service_b.py": "\n".join(pad(rng, ["from pkg.api import compute", "", "",
                                                "def run_b(items):",
                                                "    total = sum(compute(item) for item in items)",
                                                "    return total", ""], target)) + "\n",
        "pkg/cli.py": "\n".join(pad(rng, ["import sys", "", "from pkg.api import compute", "", "",
                                          "def main(argv):", f"    return {call}", ""], target)) + "\n",
        "README.md": f"# {token}\n\nRun `python -m pkg.cli`.\n",
    }
    pairs = {
        "pkg/api.py": [("def compute(x):", "def compute_total(x):")],
        "pkg/service_a.py": [("import compute", "import compute_total"), ("compute(item)", "compute_total(item)")],
        "pkg/service_b.py": [("import compute", "import compute_total"), ("compute(item)", "compute_total(item)")],
        "pkg/cli.py": [("import compute", "import compute_total"),
                       ("compute(int(argv[0]))", "compute_total(int(argv[0]))")],
    }
    return token, files, pairs


def service_app(rng, variant):
    token = f"job-{rng.randrange(100000, 999999)}"
    target = SIZES[variant % 3]
    files = {
        "app/__init__.py": "",
        "app/api.py": "def compute(x):\n    return x * 2\n",
        "app/service.py": "\n".join(pad(rng, ["from app.api import compute", "", "",
                                              "def total(items):",
                                              "    return sum(compute(item) for item in items)", ""], target)) + "\n",
        "app/report.py": f'import json\n\n\ndef summary(total):\n    return {{"total": total, "label": "{token}"}}\n',
        "data/input.json": fixture([1, -2, 3, 4]),
        "README.md": f"# {token}\n",
    }
    pairs = {"app/service.py": [("compute(item) for item in items",
                                "compute(item) for item in items if item > 0")]}
    return token, files, pairs


def fanout_checks(sleep):
    return {
        "lint": {"sleep": sleep, "modes": [{"kind": "lint"}]},
        "types": {"sleep": sleep, "modes": [{"kind": "signature", "file": "app/api.py", "function": "compute",
                                             "params": ["x"], "callers": {"app/service.py": 1}}]},
        "unit": {"sleep": sleep, "modes": [{"kind": "markers", "files": {"app/service.py": ["if item > 0"]}}]},
        "integration": {"sleep": sleep, "modes": [{"kind": "markers",
                                                   "files": {"app/report.py": ['"total"'],
                                                             "data/input.json": ["-2"]}}]},
    }


def atomic_rename(rng, variant):
    token, files, pairs = package(rng, variant)
    expected = {path: apply(files[path], changes) for path, changes in pairs.items()}
    prompt = (f"Rename compute to compute_total across the pkg package: the definition in pkg/api.py and every "
              "caller. Apply the whole change in one atomic edit(files=[...]) call, then run the acceptance check.")
    script = [changesets(pairs), [["validate", {"check": "acceptance"}]], "Acceptance passed."]
    expect = {"workspace": spec(files, exact=expected),
              "invariants": {"changesets": 1, "tx_files": 4, "no_untracked_edits": True,
                             "validated": ["acceptance"], "tool_errors": 0}}
    return files, {"acceptance": {"workspace": spec(files, exact=expected)}}, [
        agent("main", "main", prompt, script, expect)]


def atomic_abort(rng, variant):
    token, files, pairs = package(rng, variant, spaced_cli=True)
    renamed = {path: apply(files[path], changes) for path, changes in pairs.items() if path != "pkg/cli.py"}
    renamed["pkg/cli.py"] = apply(files["pkg/cli.py"], [("import compute", "import compute_total"),
                                                       ("compute( int(argv[0]) )", "compute_total( int(argv[0]) )")])
    prompt = ("Update compute to compute_total in pkg/api.py, pkg/service_a.py, pkg/service_b.py and pkg/cli.py as "
              "one atomic edit(files=[...]). If any anchor is absent, leave the workspace untouched and report "
              '{"status":"blocked"}.')
    script = [changesets(pairs), [["read", {"path": "pkg/cli.py"}]], '{"status":"blocked"}']
    # Either the preflight aborts with zero writes, or the whole rename lands: a half-applied state never counts.
    expect = {"any_of": [{"workspace": spec(files), "unchanged": True},
                         {"workspace": spec(files, exact=renamed)}],
              "invariants": {"changesets": 0, "no_edits": True, "errors": ["TEXT_NOT_FOUND"]}}
    return files, {"acceptance": {"workspace": spec(files)}}, [
        agent("main", "main", prompt, script, expect)]


def atomic_rollback(rng, variant):
    token, files, pairs = package(rng, variant)
    expected = {path: apply(files[path], changes) for path, changes in pairs.items()}
    stale = {
        "pkg/api.py": [("def compute(x):", "def compute_stale(x):")],
        "pkg/service_a.py": [("import compute", "import compute_stale"), ("compute(item)", "compute_stale(item)")],
        "pkg/service_b.py": [("import compute", "import compute_stale"), ("compute(item)", "compute_stale(item)")],
        "pkg/cli.py": [("import compute", "import compute_stale"),
                       ("compute(int(argv[0]))", "compute_stale(int(argv[0]))")],
    }
    prompt = ("Rename compute to compute_total across pkg/api.py and its three callers. If a first attempt turns out "
              "wrong, roll it back before applying the final atomic edit, then run acceptance.")
    script = [changesets(stale), [["validate", {"check": "acceptance"}]], [["rollback", {}]],
              changesets(pairs), [["validate", {"check": "acceptance"}]], "Acceptance passed."]
    expect = {"workspace": spec(files, exact=expected),
              "invariants": {"changesets": 2, "rollback_checkpoint": 1, "rollback_paths": 4,
                             "validated": ["acceptance"], "tool_errors": 0}}
    return files, {"acceptance": {"workspace": spec(files, exact=expected)}}, [
        agent("main", "main", prompt, script, expect)]


def validate_fanout(rng, variant):
    token, files, pairs = service_app(rng, variant)
    expected = {path: apply(files[path], changes) for path, changes in pairs.items()}
    prompt = ("app/service.py must total only the positive items. Fix it, then run every configured check "
              "(lint, types, unit, integration) and report the outcome.")
    script = [single("app/service.py", pairs["app/service.py"]),
              [["validate", {"check": "lint"}], ["validate", {"check": "types"}],
               ["validate", {"check": "unit"}], ["validate", {"check": "integration"}]],
              "Acceptance passed."]
    expect = {"workspace": spec(files, exact=expected),
              "invariants": {"validated": ["lint", "types", "unit", "integration"],
                             "parallel": {"min_ratio": 1.8}, "tool_errors": 0}}
    return files, fanout_checks(0.35), [agent("main", "main", prompt, script, expect)]


def write_barrier(rng, variant):
    token, files, pairs = service_app(rng, variant)
    expected = {path: apply(files[path], changes) for path, changes in pairs.items()}
    prompt = ("Read app/api.py and app/service.py, run lint and types, then make app/service.py total only positive "
              "items and confirm the change by reading it again.")
    script = [[["read", {"path": "app/api.py"}], ["read", {"path": "app/service.py"}],
               ["validate", {"check": "lint"}], ["validate", {"check": "types"}]],
              single("app/service.py", pairs["app/service.py"]),
              [["read", {"path": "app/service.py"}]],
              "Acceptance passed."]
    expect = {"workspace": spec(files, exact=expected),
              "invariants": {"order": True, "validated": ["lint", "types"], "tool_errors": 0}}
    return files, fanout_checks(0.0), [agent("main", "main", prompt, script, expect)]


def release(rng, variant):
    token = f"rel-{rng.randrange(100000, 999999)}"
    files = {
        "pkg/version.py": 'VERSION = "1.0.0"\n',
        "README.md": f"# {token}\n\nRelease 1.0.0 is current.\n",
        "CHANGELOG.md": "# Changelog\n\n## 1.0.0\n\n- Initial release.\n",
    }

    def pairs(old, new):
        return {path: [(old, new)] for path in ("pkg/version.py", "README.md", "CHANGELOG.md")}

    def bumped(version):
        return {path: apply(files[path], changes) for path, changes in pairs("1.0.0", version).items()}

    return token, files, pairs, bumped


def branch_rewind(rng, variant):
    token, files, pairs, bumped = release(rng, variant)
    right = bumped("1.1.0")
    prompt = ("Bump the release from 1.0.0 to 1.1.0 in pkg/version.py, README.md and CHANGELOG.md, then run "
              "acceptance.")
    attempt = [changesets(pairs("1.0.0", "9.9.9")), [["validate", {"check": "acceptance"}]], '{"status":"blocked"}']
    retry = [changesets(pairs("1.0.0", "1.1.0")), [["validate", {"check": "acceptance"}]], "Acceptance passed."]
    checks = {"acceptance": {"workspace": spec(files, exact=right)}}
    actions = [
        agent("main", "main", prompt, attempt,
              {"any_of": [{"workspace": spec(files, exact=bumped("9.9.9"))},
                          {"workspace": spec(files, exact=right)}]}),
        branch("main", "alt", 1, "Acceptance pins the exact release string; keep 1.1.0.",
               {"prefix_events": 1, "restored": spec(files)}),
        agent("main", "alt", None, retry,
              {"workspace": spec(files, exact=right), "invariants": {"validated": ["acceptance"],
                                                                     "prefix_request": True}},
              resume=True),
    ]
    return files, checks, actions


def branch_green(rng, variant):
    token, files, pairs, bumped = release(rng, variant)
    right = bumped("1.1.0")
    prompt = ("Bump the release from 1.0.0 to 1.1.0 in pkg/version.py, README.md and CHANGELOG.md, then run "
              "acceptance.")
    attempt = [changesets(pairs("1.0.0", "1.1.0")), [["validate", {"check": "acceptance"}]],
               changesets(pairs("1.1.0", "9.9.9")), [["validate", {"check": "acceptance"}]], '{"status":"blocked"}']
    retry = [[["validate", {"check": "acceptance"}]], "Acceptance passed."]
    checks = {"acceptance": {"workspace": spec(files, exact=right)}}
    actions = [
        agent("main", "main", prompt, attempt,
              {"any_of": [{"workspace": spec(files, exact=bumped("9.9.9"))},
                          {"workspace": spec(files, exact=right)}]}),
        branch("main", "alt", "green", "Resume from the last passing acceptance instead of replaying the dead end.",
               {"restored": spec(files, exact=right)}),
        agent("main", "alt", None, retry,
              {"workspace": spec(files, exact=right), "invariants": {"validated": ["acceptance"],
                                                                     "prefix_request": True}},
              resume=True),
    ]
    return files, checks, actions


def branch_ab(rng, variant):
    token, files, pairs, bumped = release(rng, variant)
    right = bumped("1.1.0")
    prompt = ("Bump the release from 1.0.0 to 1.1.0 in pkg/version.py, README.md and CHANGELOG.md, then run "
              "acceptance.")
    attempt = [changesets(pairs("1.0.0", "9.9.9")), [["validate", {"check": "acceptance"}]], '{"status":"blocked"}']
    partial = [single("pkg/version.py", pairs("1.0.0", "2.0.0")["pkg/version.py"]),
               [["validate", {"check": "acceptance"}]], '{"status":"blocked"}']
    retry = [changesets(pairs("1.0.0", "1.1.0")), [["validate", {"check": "acceptance"}]], "Acceptance passed."]
    checks = {"acceptance": {"workspace": spec(files, exact=right)}}
    actions = [
        agent("main", "main", prompt, attempt,
              {"any_of": [{"workspace": spec(files, exact=bumped("9.9.9"))},
                          {"workspace": spec(files, exact=right)}]}),
        branch("main", "alt_b", 1, "Try bumping only the version file.", {"prefix_events": 1}),
        agent("main", "alt_b", None, partial,
              {"any_of": [{"workspace": spec(files, exact={"pkg/version.py": bumped("2.0.0")["pkg/version.py"]})},
                          {"workspace": spec(files, exact=right)}]},
              resume=True),
        branch("main", "alt_a", 1, "Bump every reference to the release string atomically.", {"prefix_events": 1}),
        agent("main", "alt_a", None, retry,
              {"workspace": spec(files, exact=right), "invariants": {"validated": ["acceptance"],
                                                                     "prefix_request": True}},
              resume=True),
    ]
    return files, checks, actions


def worker_modules(rng, variant):
    token = f"mod-{rng.randrange(100000, 999999)}"
    files = {
        "pkg/__init__.py": "",
        "pkg/util.py": "def double(x):\n    return x * 2\n",
        "pkg/alpha.py": "from pkg.util import double\n\n\ndef alpha(items):\n    return [double(item) for item in items]\n",
        "pkg/beta.py": ("from pkg.util import double\n\n\ndef beta(items):\n"
                        "    return sum(double(item) for item in items)\n"),
        "README.md": f"# {token}\n",
    }
    return token, files


def worker_disjoint(rng, variant):
    token, files = worker_modules(rng, variant)
    main_pairs = {"pkg/alpha.py": [("double(item) for item in items]",
                                  "double(item) + 1 for item in items]")]}
    worker_pairs = {"pkg/beta.py": [("sum(double(item) for item in items)",
                                     "sum(double(item) for item in items) + 1")]}
    main_state = {**files, **{p: apply(files[p], c) for p, c in main_pairs.items()}}
    worker_state = {**files, **{p: apply(files[p], c) for p, c in worker_pairs.items()}}
    merged = {**files, **{p: apply(files[p], c) for p, c in main_pairs.items()},
              **{p: apply(files[p], c) for p, c in worker_pairs.items()}}
    checks = {"acceptance": {"workspace": spec(files, exact=merged)}}
    actions = [
        spawn("w1"),
        agent("main", "main", "In pkg/alpha.py add one to each doubled value. Leave pkg/beta.py alone.",
              [changesets(main_pairs), "Done."], {"workspace": spec(files, exact=main_state)}),
        agent("w1", "work", "In pkg/beta.py add one to the summed doubles. Leave pkg/alpha.py alone.",
              [changesets(worker_pairs), "Done."], {"workspace": spec(files, exact=worker_state)}),
        merge("w1", {"status": "ok", "head_changed_paths": ["pkg/alpha.py"],
                     "worker_changed_paths": ["pkg/beta.py"], "workspace": spec(files, exact=merged)}),
    ]
    return files, checks, actions


def worker_shared_file(rng, variant):
    token, files = worker_modules(rng, variant)
    files["pkg/config.py"] = ("LIMIT = 10\n\n"
                              "# retry budget for the worker pool\n# keep in sync with ops\n\n\n"
                              "def retries():\n    return 3\n\n"
                              "# request timeout in seconds\n# keep in sync with ops\n\n\n"
                              "def timeout():\n    return 30\n")
    main_pairs = {"pkg/config.py": [("def retries():\n    return 3", "def retries():\n    return 5")]}
    worker_pairs = {"pkg/config.py": [("def timeout():\n    return 30", "def timeout():\n    return 45")]}
    merged = {**files, "pkg/config.py": apply(apply(files["pkg/config.py"], main_pairs["pkg/config.py"]),
                                              worker_pairs["pkg/config.py"])}
    checks = {"acceptance": {"workspace": spec(files, exact=merged)}}
    actions = [
        spawn("w1"),
        agent("main", "main", "Raise the retry budget in pkg/config.py from 3 to 5.",
              [changesets(main_pairs), "Done."],
              {"workspace": spec(files, exact={**files, "pkg/config.py":
                                               apply(files["pkg/config.py"], main_pairs["pkg/config.py"])})}),
        agent("w1", "work", "Raise the request timeout in pkg/config.py from 30 to 45.",
              [changesets(worker_pairs), "Done."],
              {"workspace": spec(files, exact={**files, "pkg/config.py":
                                               apply(files["pkg/config.py"], worker_pairs["pkg/config.py"])})}),
        merge("w1", {"status": "ok", "changed_paths": ["pkg/config.py"], "workspace": spec(files, exact=merged)}),
    ]
    return files, checks, actions


def worker_write_skew(rng, variant):
    token = f"api-{rng.randrange(100000, 999999)}"
    files = {
        "pkg/__init__.py": "",
        "pkg/api.py": "def compute(x):\n    return x * 2\n",
        "pkg/service_a.py": ("from pkg.api import compute\n\n\ndef run_a(items):\n"
                             "    return [compute(item) for item in items]\n"),
        "pkg/service_b.py": ("from pkg.api import compute\n\n\ndef run_b(items):\n"
                             "    return sum(compute(item) for item in items)\n"),
        "README.md": f"# {token}\n",
    }
    head_pairs = {
        "pkg/api.py": [("def compute(x):", "def compute(x, scale=1):"), ("return x * 2", "return x * 2 * scale")],
        "pkg/service_a.py": [("compute(item)", "compute(item, scale=2)")],
    }
    stale_pairs = {"pkg/service_b.py": [("sum(compute(item) for item in items)",
                                         "sum(compute(item) for item in items) + 1")]}
    fixed_pairs = {"pkg/service_b.py": [("compute(item) for item in items",
                                         "compute(item, scale=1) for item in items")]}
    head_state = {**files, **{p: apply(files[p], c) for p, c in head_pairs.items()}}
    merged = {**head_state, **{p: apply(files[p], c) for p, c in stale_pairs.items()},
              "pkg/service_b.py": apply(apply(files["pkg/service_b.py"], stale_pairs["pkg/service_b.py"]),
                                        fixed_pairs["pkg/service_b.py"])}
    checks = {
        "acceptance": {"workspace": spec(files, exact=merged)},
        "signature": {"modes": [{"kind": "signature", "file": "pkg/api.py", "function": "compute",
                                 "params": ["x", "scale"],
                                 "callers": {"pkg/service_a.py": 2, "pkg/service_b.py": 2}}]},
    }
    actions = [
        spawn("w2"),
        agent("main", "main", "Give compute in pkg/api.py an explicit scale parameter (default 1) and pass "
                              "scale=2 from pkg/service_a.py.",
              [changesets(head_pairs), "Done."], {"workspace": spec(files, exact=head_state)}),
        agent("w2", "work", "In pkg/service_b.py add one to the summed doubles. Leave pkg/api.py alone.",
              [changesets(stale_pairs), "Done."],
              {"workspace": spec(files, exact={**files, "pkg/service_b.py":
                                               apply(files["pkg/service_b.py"], stale_pairs["pkg/service_b.py"])})}),
        merge("w2", {"status": "validation_failed", "code": "MERGE_VALIDATION_FAILED",
                     "head_changed_paths": ["pkg/api.py", "pkg/service_a.py"], "unchanged": True,
                     "failed_checks": ["acceptance", "signature"]}),
        rebase("w2", {"status": "ok", "head_changed_paths": ["pkg/api.py", "pkg/service_a.py"],
                      "updated_paths": ["pkg/api.py", "pkg/service_a.py"]}),
        agent("w2", "work", "The merge was rejected because pkg/service_b.py still calls compute with one argument. "
                            "Your workspace has been rebased onto mainline; inspect pkg/api.py and update "
                            "pkg/service_b.py so every caller passes scale explicitly.",
              [changesets(fixed_pairs), "Done."],
              {"workspace": spec(files, exact=merged)}),
        merge("w2", {"status": "ok", "workspace": spec(files, exact=merged)}),
    ]
    return files, checks, actions


def worker_conflict(rng, variant):
    token, files = worker_modules(rng, variant)
    files["notes.md"] = f"# Release notes {token}\n\n- Build passes.\n- Deploy is manual.\n"
    main_pairs = {"notes.md": [("- Deploy is manual.", "- Deploy is automated.")]}
    worker_pairs = {"notes.md": [("- Deploy is manual.", "- Deploy is scripted.\n- Rollback is tested.")]}
    main_state = {**files, "notes.md": apply(files["notes.md"], main_pairs["notes.md"])}
    worker_state = {**files, "notes.md": apply(files["notes.md"], worker_pairs["notes.md"])}
    fixed_pairs = {"notes.md": [("- Deploy is automated.", "- Deploy is automated.\n- Rollback is tested.")]}
    merged = {**main_state, "notes.md": apply(main_state["notes.md"], fixed_pairs["notes.md"])}
    checks = {"acceptance": {"workspace": spec(files, exact=merged)}}
    actions = [
        spawn("w1"),
        agent("main", "main", "Update notes.md so deploy is automated.",
              [changesets(main_pairs), "Done."], {"workspace": spec(files, exact=main_state)}),
        agent("w1", "work", "Update notes.md so deploy is scripted and add a rollback line.",
              [changesets(worker_pairs), "Done."],
              {"workspace": spec(files, allow=["notes.md"]),
               "markers": {"notes.md": ["- Deploy is scripted.", "- Rollback"]}}),
        merge("w1", {"status": "conflict", "code": "MERGE_CONFLICT", "conflicts": ["notes.md"],
                     "unchanged": True}),
        spawn("w2"),
        agent("w2", "work", "The merge conflicted on notes.md. Add the line `- Rollback is tested.` after the Deploy "
                            "line in notes.md.",
              [[["read", {"path": "notes.md"}]], changesets(fixed_pairs), "Done."],
              {"workspace": spec(files, exact={"notes.md": apply(main_state["notes.md"],
                                                                 fixed_pairs["notes.md"])})}),
        merge("w2", {"status": "ok", "workspace": spec(files, exact=merged)}),
    ]
    return files, checks, actions


SHAPES = {
    "atomic": (("atomic-rename", atomic_rename, 10), ("atomic-abort", atomic_abort, 8),
               ("atomic-rollback", atomic_rollback, 7)),
    "parallel": (("validate-fanout", validate_fanout, 13), ("write-barrier", write_barrier, 12)),
    "branch": (("branch-rewind", branch_rewind, 10), ("branch-green", branch_green, 5),
               ("branch-ab", branch_ab, 10)),
    "worker": (("worker-disjoint", worker_disjoint, 8), ("worker-shared-file", worker_shared_file, 5),
               ("worker-write-skew", worker_write_skew, 8), ("worker-conflict", worker_conflict, 4)),
}


def corpus():
    for category, shapes in SHAPES.items():
        for shape, builder, count in shapes:
            for variant in range(count):
                identifier = f"{category}.{shape}.{variant:02d}"
                rng = random.Random(int.from_bytes(sha256(identifier.encode()).digest()[:8]))
                files, checks, actions = builder(rng, variant)
                yield Case(identifier, category, shape, variant, files, checks, actions)


def manifest(cases):
    return [{"id": case.id, "category": case.category, "shape": case.shape, "variant": case.variant,
             "actions": len(case.actions),
             "sha256": sha256(encode({"files": case.files, "checks": case.checks, "actions": case.actions})).hexdigest()}
            for case in cases]
