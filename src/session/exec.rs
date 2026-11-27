use super::journal::Journal;
use super::tools::{parse, rejected, Reply};
use crate::workspace::WorkspaceContext;
use serde::Deserialize;
use serde_json::Value;
use std::{fs::File, io, path::Path};

const RUNTIME: &[&str] = &["/usr/bin", "/usr/lib", "/usr/lib64"];
pub(super) fn runtime_contains(path: &Path) -> bool {
    RUNTIME.iter().any(|directory| path.starts_with(directory))
}

pub(super) fn runtime_overlaps(path: &Path) -> bool {
    runtime_contains(path) || RUNTIME.iter().any(|directory| Path::new(directory).starts_with(path))
}

pub(super) fn open_snapshot_file(root: &Path, relative: &Path) -> io::Result<File> {
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;

        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }

        let directory = File::open(root)?;
        let path = CString::new(relative.as_os_str().as_bytes())?;
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
            mode: 0,
            resolve: 0x08 | 0x04, // RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS
        };
        // The directory descriptor anchors path resolution at the workspace.
        let fd = unsafe {
            libc::syscall(libc::SYS_openat2, directory.as_raw_fd(), path.as_ptr(),
                &how, std::mem::size_of::<OpenHow>())
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }
    #[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
    {
        let _ = (root, relative);
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    timeout_seconds: Option<u64>,
}

pub(super) fn run(
    ctx: &WorkspaceContext,
    journal: &mut Journal,
    run: usize,
    operation: &str,
    args: &Value,
) -> Reply {
    let args: Args = match parse(args) {
        Ok(args) => args,
        Err(error) => return Ok(error),
    };
    let timeout = args.timeout_seconds.unwrap_or(15);
    if args.command.trim().is_empty() || args.command.len() > 4096 || !(1..=60).contains(&timeout) {
        return Ok(rejected("INVALID_ARGUMENT", "Supply a command of at most 4096 bytes and a timeout of 1–60 seconds."));
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        linux::run(ctx, journal, run, operation, &args.command, timeout)
    }
    #[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
    {
        let _ = (ctx, journal, run, operation);
        Ok(rejected("EXEC_UNAVAILABLE", "Sandboxed exec requires 64-bit Linux and Bubblewrap. Use read for file inspection."))
    }
}

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod linux {
    use super::*;
    use crate::budget::truncate_utf8_bytes;
    use serde_json::json;
    use std::fs::File;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const SNAPSHOT_LIMIT: u64 = 48 * 1024 * 1024;
    const STREAM_LIMIT: u64 = 512 * 1024;
    const BWRAP: &str = "/usr/bin/bwrap";

    fn socket_filter() -> io::Result<File> {
        const LOAD: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
        const EQ: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
        const GE: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
        const RETURN: u16 = 0x06;
        #[cfg(target_arch = "x86_64")]
        const NATIVE_ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
        #[cfg(target_arch = "aarch64")]
        const NATIVE_ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64

        let mut filter = tempfile::tempfile()?;
        let mut emit = |code: u16, yes: u8, no: u8, value: u32| -> io::Result<()> {
            filter.write_all(&code.to_ne_bytes())?;
            filter.write_all(&[yes, no])?;
            filter.write_all(&value.to_ne_bytes())
        };
        emit(LOAD, 0, 0, 4)?; // seccomp_data.arch
        emit(EQ, 1, 0, NATIVE_ARCH)?;
        emit(RETURN, 0, 0, libc::SECCOMP_RET_KILL_PROCESS)?;
        emit(LOAD, 0, 0, 0)?; // seccomp_data.nr
        // Reject the alternate x32 syscall ABI, which shares the x86_64 arch value.
        emit(GE, 0, 1, 0x4000_0000)?;
        emit(RETURN, 0, 0, libc::SECCOMP_RET_KILL_PROCESS)?;
        for call in [
            libc::SYS_socket,
            libc::SYS_io_uring_setup,
            libc::SYS_ptrace,
            libc::SYS_pidfd_getfd,
        ] {
            emit(EQ, 0, 1, call as u32)?;
            emit(RETURN, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32)?;
        }
        emit(RETURN, 0, 0, libc::SECCOMP_RET_ALLOW)?;
        filter.seek(SeekFrom::Start(0))?;
        Ok(filter)
    }

    pub(super) fn run(
        ctx: &WorkspaceContext,
        journal: &mut Journal,
        run: usize,
        operation: &str,
        command: &str,
        timeout: u64,
    ) -> Reply {
        if let Some(receipt) = journal.events.iter().find(|e| {
            e.kind == "exec_receipt" && e.data["operation"] == operation
        }) {
            return Ok(receipt.data.clone());
        }
        if !Path::new(BWRAP).is_file() {
            return Ok(rejected("EXEC_UNAVAILABLE", "Install Bubblewrap at /usr/bin/bwrap to enable sandboxed exec."));
        }
        let config_dir = ctx.root().join(".sourceharness");
        let snapshot = tempfile::Builder::new().prefix("exec-").tempdir_in(config_dir)
            .map_err(|err| err.to_string())?;
        let workspace_revision = match super::super::validation::copy_workspace(
            ctx, journal, snapshot.path(), Some(SNAPSHOT_LIMIT),
        ) {
            Ok(revision) => revision,
            Err(reason) => return Ok(rejected("EXEC_SNAPSHOT_UNAVAILABLE", &reason)),
        };
        journal.append(run, "exec_intent", json!({
            "operation":operation, "workspace_revision":workspace_revision, "timeout_seconds":timeout
        }))?;

        let mut stdout = tempfile::tempfile().map_err(|err| err.to_string())?;
        let mut stderr = tempfile::tempfile().map_err(|err| err.to_string())?;
        let filter = socket_filter().map_err(|err| err.to_string())?;
        let fd = filter.as_raw_fd();
        let mut child = Command::new(BWRAP);
        child.env_clear().args([
            "--unshare-all", "--unshare-user", "--share-net", "--disable-userns",
            "--cap-drop", "ALL", "--die-with-parent", "--new-session", "--clearenv",
        ]);
        for directory in RUNTIME {
            child.args(["--ro-bind-try", directory, directory]);
        }
        child.args([
            "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib", "/lib",
            "--symlink", "usr/lib64", "/lib64", "--proc", "/proc", "--remount-ro", "/proc",
            "--dev", "/dev", "--size", "16777216", "--tmpfs", "/tmp",
            "--size", "67108864", "--tmpfs", "/work", "--ro-bind",
        ])
        .arg(snapshot.path()).arg("/input")
        .args([
            "--symlink", "/usr/bin/python3", "/tools/python",
            "--chdir", "/work", "--setenv", "HOME", "/tmp", "--setenv", "PATH", "/tools:/usr/bin:/bin",
            "--setenv", "LANG", "C.UTF-8", "--setenv", "PYTHONDONTWRITEBYTECODE", "1",
            "--seccomp", &fd.to_string(), "--", "/bin/sh", "-c",
            "cp -a /input/. . && exec /bin/sh -c \"$1\"", "sourceharness-exec", command,
        ])
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().map_err(|err| err.to_string())?)
        .stderr(stderr.try_clone().map_err(|err| err.to_string())?);

        // Bubblewrap consumes the filter fd before starting the command. The
        // limits apply to the sandbox and its descendants, not the CLI.
        unsafe {
            child.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                for (resource, cap) in [
                    (libc::RLIMIT_CORE, 0),
                    (libc::RLIMIT_FSIZE, 8 * 1024 * 1024),
                    (libc::RLIMIT_NOFILE, 256),
                    (libc::RLIMIT_NPROC, 256),
                    (libc::RLIMIT_AS, 1024 * 1024 * 1024),
                ] {
                    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                    if libc::getrlimit(resource, &mut limit) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    limit.rlim_max = limit.rlim_max.min(cap);
                    limit.rlim_cur = limit.rlim_cur.min(limit.rlim_max);
                    if libc::setrlimit(resource, &limit) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = match child.spawn() {
            Ok(child) => child,
            Err(err) => return Ok(rejected("EXEC_UNAVAILABLE", &format!("Sandbox launch failed: {err}"))),
        };
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let (code, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (status.code(), false),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => {
                    let _ = child.kill();
                    child.wait().map_err(|err| err.to_string())?;
                    other.map_err(|err| err.to_string())?;
                    break (None, true);
                }
            }
        };
        let (out, out_clipped) = capture(&mut stdout).map_err(|err| err.to_string())?;
        let (err, err_clipped) = capture(&mut stderr).map_err(|err| err.to_string())?;
        let complete = !out_clipped && !err_clipped;
        let mut text = String::from_utf8_lossy(&out).into_owned();
        if !err.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("stderr:\n");
            text.push_str(&String::from_utf8_lossy(&err));
        }
        let (preview, clipped) = truncate_utf8_bytes(&text, 2048);
        let folded = clipped || !complete;
        let artifact = journal.artifact(run, json!({"kind":"exec","operation":operation}), text.as_bytes())?;
        let mut receipt = json!({
            "status":if code == Some(0) { "ok" } else { "error" },
            "operation":operation, "workspace_revision":workspace_revision, "exit_code":code, "timed_out":timed_out,
            "streams":{"stdout":journal.put(&out)?,"stderr":journal.put(&err)?},
            "preview":preview, "output_complete":complete, "result_ref":artifact.result_ref,
            "delivery":if folded {"folded"} else {"inline"}
        });
        if code != Some(0) {
            let (code, reason) = if timed_out {
                ("EXEC_TIMEOUT", "Command timed out.")
            } else if text.starts_with("stderr:\nbwrap:") {
                ("EXEC_UNAVAILABLE", "Sandbox setup failed.")
            } else {
                ("EXEC_FAILED", "Command failed.")
            };
            receipt["code"] = json!(code);
            receipt["reason"] = json!(reason);
        }
        journal.append(run, "exec_receipt", receipt.clone())?;
        Ok(receipt)
    }

    fn capture(file: &mut File) -> io::Result<(Vec<u8>, bool)> {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(STREAM_LIMIT + 1).read_to_end(&mut bytes)?;
        let clipped = bytes.len() > STREAM_LIMIT as usize;
        bytes.truncate(STREAM_LIMIT as usize);
        Ok((bytes, clipped))
    }
}
