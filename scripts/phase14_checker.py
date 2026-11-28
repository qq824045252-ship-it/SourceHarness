"""Declarative external checks for the Phase 1-4 conformance benchmark."""

import ast
import json
from pathlib import Path
import sys
import time

from check_qwen_benchmark import tree, workspace_check


def lint(files, _):
    errors = []
    for path, raw in sorted(files.items()):
        if not path.endswith(".py"):
            continue
        try:
            ast.parse(raw.decode("utf-8"))
        except (SyntaxError, UnicodeDecodeError) as error:
            errors.append(f"{path}: {error}")
    return errors


def markers(files, mode):
    errors = []
    for path, needles in mode["files"].items():
        if path not in files:
            errors.append(f"missing file: {path}")
            continue
        text = files[path].decode("utf-8", "replace")
        errors.extend(f"{path}: missing {needle!r}" for needle in needles if needle not in text)
    return errors


def absent(files, mode):
    return [f"unexpected file: {path}" for path in mode["paths"] if path in files]


def exact(files, mode):
    return [f"file contents differ: {path}" for path, text in mode["files"].items()
            if files.get(path) != text.encode("utf-8")]


def calls(module, function):
    for node in ast.walk(module):
        if not isinstance(node, ast.Call):
            continue
        name = getattr(node.func, "id", None) or getattr(node.func, "attr", None)
        if name == function:
            yield len(node.args) + len(node.keywords)


def signature(files, mode):
    source = files.get(mode["file"])
    if source is None:
        return [f"missing file: {mode['file']}"]
    try:
        module = ast.parse(source.decode("utf-8"))
    except (SyntaxError, UnicodeDecodeError) as error:
        return [f"{mode['file']}: {error}"]
    params = {node.name: [arg.arg for arg in node.args.args]
              for node in ast.walk(module) if isinstance(node, ast.FunctionDef)}
    errors = []
    if params.get(mode["function"]) != mode["params"]:
        errors.append(f"{mode['file']}: {mode['function']} parameters are {params.get(mode['function'])}, "
                      f"expected {mode['params']}")
    for path, arity in mode["callers"].items():
        text = files.get(path)
        if text is None:
            errors.append(f"missing file: {path}")
            continue
        try:
            caller = ast.parse(text.decode("utf-8"))
        except (SyntaxError, UnicodeDecodeError) as error:
            errors.append(f"{path}: {error}")
            continue
        for count in calls(caller, mode["function"]):
            if count != arity:
                errors.append(f"{path}: {mode['function']} called with {count} argument(s), expected {arity}")
    return errors


MODES = {"lint": lint, "markers": markers, "absent": absent, "exact": exact, "signature": signature}


def main():
    config = json.loads(Path(sys.argv[1]).read_bytes())
    delay = config.get("sleep")
    if delay:
        time.sleep(delay)
    files = tree(Path.cwd())
    errors = []
    if "workspace" in config:
        errors.extend(workspace_check(files, config["workspace"]))
    for mode in config.get("modes", []):
        errors.extend(MODES[mode["kind"]](files, mode))
    if errors:
        if config.get("hint"):
            print(config["hint"], file=sys.stderr)
        for message in errors[:5]:
            print(message, file=sys.stderr)
        return 1
    print("Check passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
