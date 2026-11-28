"""Independent checks for generated compact workspaces and Python repairs."""

from hashlib import sha256
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from qwen_benchmark_cases import encode


def tree(root):
    result = {}
    for path, directories, files in os.walk(root):
        directories[:] = sorted(name for name in directories if name not in (".sourceharness",".git","__pycache__"))
        if any((Path(path) / name).is_symlink() for name in directories):
            raise ValueError("Unexpected symbolic link")
        for name in sorted(files):
            file = Path(path) / name
            if file.is_symlink() or not file.is_file():
                raise ValueError("Unexpected non-regular file")
            result[file.relative_to(root).as_posix()] = file.read_bytes()
    return result


def digests(files):
    return {path:sha256(raw).hexdigest() for path,raw in files.items()}


def write(root, files):
    root.mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        path = root / name
        if data is None:
            path.unlink(missing_ok=True)
            continue
        path.parent.mkdir(parents=True,exist_ok=True)
        path.write_bytes(data.encode("utf-8") if isinstance(data,str) else data)


def run_behavior(files, spec):
    """Execute the candidate with a read-only copy of its files and JSON test vectors."""
    target = spec["path"]
    if target not in files:
        return "missing executable"
    with tempfile.TemporaryDirectory(prefix="qwen-judge-") as directory:
        write(Path(directory), files)
        env = {k: v for k, v in os.environ.items() if k in ("PATH", "SYSTEMROOT", "COMSPEC", "PATHEXT", "TEMP", "TMP")}
        env["PYTHONUTF8"] = "1"
        try:
            result = subprocess.run(
                [sys.executable, "-B", target],
                cwd=directory,
                input=b"".join(encode(value) + b"\n" for value in spec["inputs"]),
                capture_output=True,
                timeout=12,
                env=env,
            )
        except subprocess.TimeoutExpired:
            return "candidate timed out"
        if result.returncode != 0:
            return "candidate exited unsuccessfully"
        try:
            values = [json.loads(line) for line in result.stdout[:65536].splitlines()]
        except (ValueError, UnicodeError):
            return "candidate did not emit JSON lines"
        if encode(values) != encode(spec["outputs"]):
            return "candidate output differs on hidden vectors"
    return None


def workspace_check(files, spec):
    if "unavailable" in spec:
        now = time.time_ns()
        return [f"timestamp={now} line {now%397}: required upstream signature {spec['unavailable']} is missing"]
    exact = spec.get("exact",{})
    semantic = spec.get("json",{})
    behavior = spec.get("behavior")
    protected = spec.get("protected",{})
    allowed = set(exact) | set(semantic) | set(spec.get("allow",()))
    if behavior:
        allowed.add(behavior["path"])
    errors = []
    if "protected" in spec:
        actual = digests(files)
        errors.extend(f"protected file changed: {path}" for path,digest in sorted(protected.items())
                      if actual.get(path) != digest)
        errors.extend(f"unexpected file: {path}" for path in sorted(actual.keys()-protected.keys()-allowed))
    errors.extend(f"file contents differ: {path}" for path,expected in exact.items()
                  if files.get(path) != expected.encode("utf-8"))
    for path, expected in semantic.items():
        try:
            accepted = path in files and encode(json.loads(files[path])) == encode(expected)
        except (ValueError,UnicodeError):
            accepted = False
        if not accepted:
            errors.append(f"JSON value differs: {path}")
    if behavior:
        problem = run_behavior(files,behavior)
        if problem:
            errors.append(problem)
    return errors


def main():
    config = json.loads(Path(sys.argv[1]).read_bytes())
    errors = workspace_check(tree(Path.cwd()),config)
    if errors:
        hint = config.get("hint")
        if hint:
            print(hint,file=sys.stderr)
        for message in errors[:5]:
            print(message,file=sys.stderr)
        return 1
    print("Acceptance passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
