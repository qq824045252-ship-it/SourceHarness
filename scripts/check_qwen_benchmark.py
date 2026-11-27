"""Independent checks for generated compact workspaces and Python repairs."""

from contextlib import contextmanager
import ctypes
import errno
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


@contextmanager
def socket_filter():
    seccomp = ctypes.CDLL("libseccomp.so.2")
    seccomp.seccomp_init.argtypes = [ctypes.c_uint32]
    seccomp.seccomp_init.restype = ctypes.c_void_p
    seccomp.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    seccomp.seccomp_rule_add.argtypes = [ctypes.c_void_p,ctypes.c_uint32,ctypes.c_int,ctypes.c_uint]
    seccomp.seccomp_export_bpf.argtypes = [ctypes.c_void_p,ctypes.c_int]
    seccomp.seccomp_release.argtypes = [ctypes.c_void_p]
    context = seccomp.seccomp_init(0x7fff0000)  # SCMP_ACT_ALLOW
    if not context:
        raise RuntimeError("Cannot initialize checker seccomp")
    try:
        with tempfile.TemporaryFile() as file:
            for name in (b"socket",b"io_uring_setup",b"ptrace",b"pidfd_getfd"):
                number = seccomp.seccomp_syscall_resolve_name(name)
                if number < 0 or seccomp.seccomp_rule_add(context,0x00050000 | errno.EPERM,number,0):
                    raise RuntimeError("Cannot install checker seccomp rule")
            if seccomp.seccomp_export_bpf(context,file.fileno()):
                raise RuntimeError("Cannot export checker seccomp")
            file.seek(0)
            yield file.fileno()
    finally:
        seccomp.seccomp_release(context)


def run_behavior(files, spec):
    """Execute the candidate with a read-only copy of its files and JSON test vectors."""
    target = spec["path"]
    if target not in files:
        return "missing executable"
    with (tempfile.TemporaryDirectory(prefix="qwen-judge-") as directory,
          socket_filter() as policy, tempfile.TemporaryFile() as stdout,
          tempfile.TemporaryFile() as stderr):
        write(Path(directory), files)
        command = [
            "/usr/bin/prlimit","--as=1073741824","--fsize=65536","--nproc=256","--nofile=128","--core=0","--",
            "/usr/bin/bwrap","--unshare-all","--share-net","--unshare-user","--disable-userns",
            "--cap-drop","ALL","--new-session","--die-with-parent","--clearenv",
            "--ro-bind","/usr/bin","/usr/bin","--ro-bind","/usr/lib","/usr/lib",
            "--ro-bind-try","/usr/lib64","/usr/lib64","--symlink","usr/bin","/bin",
            "--symlink","usr/lib","/lib","--symlink","usr/lib64","/lib64",
            "--dev","/dev","--size","16777216","--tmpfs","/tmp","--ro-bind",directory,"/work",
            "--setenv","HOME","/tmp","--setenv","PATH","/usr/bin:/bin","--setenv","LANG","C.UTF-8",
            "--seccomp",str(policy),"--chdir","/work","/usr/bin/python3","-B",target,
        ]
        try:
            result = subprocess.run(command,input=b"".join(encode(value)+b"\n" for value in spec["inputs"]),
                                    stdout=stdout,stderr=stderr,pass_fds=(policy,),timeout=12,
                                    env={"PATH":"/usr/bin:/bin"})
        except subprocess.TimeoutExpired:
            return "candidate timed out"
        if result.returncode != 0:
            stderr.seek(0)
            if stderr.read(64).startswith((b"bwrap:",b"prlimit:")):
                raise RuntimeError("The checker sandbox could not start")
            return "candidate exited unsuccessfully"
        stdout.seek(0)
        try:
            values = [json.loads(line) for line in stdout.read(65536).splitlines()]
        except (ValueError,UnicodeError):
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
    allowed = set(exact) | set(semantic)
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
