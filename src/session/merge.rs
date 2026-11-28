use super::journal::{put_blob, revision, Journal};
use super::rollback::Manifest;
use super::validation::{run_check_in_dir, Commands};
use super::{session_path, tools};
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub worker_name: String,
    pub main_root: PathBuf,
    pub worker_dir: PathBuf,
    pub base_revision: String,
    pub created_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailedCheck {
    pub check: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub preview: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeReport {
    pub status: String,
    pub code: Option<String>,
    pub base_revision: String,
    pub head_revision: String,
    pub merged_revision: Option<String>,
    pub head_changed_paths: Vec<String>,
    pub worker_changed_paths: Vec<String>,
    pub changed_paths: Vec<String>,
    pub conflicts: Vec<String>,
    pub validated_checks: Vec<String>,
    pub failed_checks: Vec<FailedCheck>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseReport {
    pub status: String,
    pub code: Option<String>,
    pub base_revision: String,
    pub head_revision: String,
    pub new_base_revision: Option<String>,
    pub head_changed_paths: Vec<String>,
    pub worker_changed_paths: Vec<String>,
    pub updated_paths: Vec<String>,
    pub conflicts: Vec<String>,
}

struct ThreeWayOutcome {
    all_paths: BTreeSet<String>,
    head_changed_paths: Vec<String>,
    worker_changed_paths: Vec<String>,
    merged_manifest: Manifest,
    conflicts: Vec<String>,
}

fn capture_manifest(ctx: &WorkspaceContext, blobs_dir: &Path) -> Result<(Manifest, String), String> {
    let (files, all) = tools::walk(ctx.root(), ctx.config().max_list_entries)?;
    if !all { return Err("Workspace exceeds manifest file limit".to_owned()); }
    let mut manifest = BTreeMap::new();
    for path in files {
        let rel = path.strip_prefix(ctx.root()).map_err(|e| e.to_string())?;
        if !tools::permitted(rel) { continue; }
        let meta = fs::metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_file() { continue; }
        let name = rel.to_str().ok_or("Workspace snapshot requires UTF-8 paths")?.replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = fs::read(&path).map_err(|e| format!("{name}: {e}"))?;
        let hash = put_blob(blobs_dir, &bytes)?;
        manifest.insert(name, hash);
    }
    let rev = put_blob(blobs_dir, &serde_json::to_vec(&manifest).map_err(|e| e.to_string())?)?;
    Ok((manifest, rev))
}

fn read_blob_from(primary: &Path, fallback: Option<&Path>, hash: &str) -> Result<Vec<u8>, String> {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid content hash".to_owned());
    }
    let bytes = match fs::read(primary.join(hash)) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(fb) = fallback { fs::read(fb.join(hash)).map_err(|e| e.to_string())? }
            else { return Err(err.to_string()); }
        }
        Err(err) => return Err(err.to_string()),
    };
    if revision(&bytes) != hash {
        return Err(format!("Content hash mismatch: {hash}"));
    }
    Ok(bytes)
}

fn materialize_manifest(dest: &Path, manifest: &Manifest, blobs_dir: &Path, fallback: Option<&Path>) -> Result<(), String> {
    for (rel, hash) in manifest {
        if !tools::permitted(Path::new(rel)) {
            return Err(format!("Unsafe manifest path: {rel}"));
        }
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let bytes = read_blob_from(blobs_dir, fallback, hash)?;
        fs::write(&target, bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn spawn_worker(main_root: &Path, worker_name: &str, worker_dir: &Path) -> Result<WorkerInfo, String> {
    if worker_name.is_empty() || worker_name.len() > 64
        || !worker_name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err("Worker name must contain 1–64 letters, digits, underscores, or hyphens.".to_owned());
    }
    if worker_dir.exists() {
        return Err(format!("Worker directory already exists: {}", worker_dir.display()));
    }
    let main_ctx = WorkspaceContext::new(WorkspaceConfig::with_root(main_root))?;
    let main_blobs = main_ctx.root().join(".sourceharness/blobs");
    let (base_manifest, base_revision) = capture_manifest(&main_ctx, &main_blobs)?;

    fs::create_dir_all(worker_dir).map_err(|e| e.to_string())?;
    let worker_canonical = worker_dir.canonicalize().map_err(|e| e.to_string())?;
    materialize_manifest(&worker_canonical, &base_manifest, &main_blobs, None)?;

    let worker_blobs = worker_canonical.join(".sourceharness/blobs");
    put_blob(&worker_blobs, &serde_json::to_vec(&base_manifest).map_err(|e| e.to_string())?)?;
    for hash in base_manifest.values() {
        let bytes = read_blob_from(&main_blobs, None, hash)?;
        put_blob(&worker_blobs, &bytes)?;
    }

    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let info = WorkerInfo {
        worker_name: worker_name.to_owned(),
        main_root: main_ctx.root().to_path_buf(),
        worker_dir: worker_canonical.clone(),
        base_revision,
        created_ms: now_ms,
    };
    fs::write(
        worker_canonical.join(".sourceharness/worker.json"),
        serde_json::to_vec_pretty(&info).map_err(|e| e.to_string())?,
    ).map_err(|e| e.to_string())?;
    Ok(info)
}

pub fn merge_worker(
    main_root: &Path,
    worker_dir: &Path,
    commands: &Commands,
    main_session: Option<&str>,
) -> Result<MergeReport, String> {
    let main_ctx = WorkspaceContext::new(WorkspaceConfig::with_root(main_root))?;
    let worker_ctx = WorkspaceContext::new(WorkspaceConfig::with_root(worker_dir))?;
    commands.check_exec_boundary(main_ctx.root())?;
    let worker_meta: WorkerInfo = serde_json::from_slice(
        &fs::read(worker_ctx.root().join(".sourceharness/worker.json")).map_err(|e| format!("Missing worker metadata: {e}"))?
    ).map_err(|e| e.to_string())?;

    fs::create_dir_all(main_ctx.root().join(".sourceharness")).map_err(|e| e.to_string())?;
    let ws_lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(main_ctx.root().join(".sourceharness/workspace.lock")).map_err(|e| e.to_string())?;
    ws_lock.try_lock().map_err(|e| format!("Mainline workspace is locked by an active session or merge: {e}"))?;

    let main_blobs = main_ctx.root().join(".sourceharness/blobs");
    let worker_blobs = worker_ctx.root().join(".sourceharness/blobs");
    let base_manifest: Manifest = serde_json::from_slice(
        &read_blob_from(&main_blobs, Some(&worker_blobs), &worker_meta.base_revision)?
    ).map_err(|e| e.to_string())?;
    let (head_manifest, head_revision) = capture_manifest(&main_ctx, &main_blobs)?;
    let (worker_manifest, _worker_rev) = capture_manifest(&worker_ctx, &main_blobs)?;

    let ThreeWayOutcome {
        all_paths,
        head_changed_paths,
        worker_changed_paths,
        merged_manifest,
        conflicts,
    } = three_way_merge(&base_manifest, &head_manifest, &worker_manifest, &main_blobs, Some(&worker_blobs))?;

    if !conflicts.is_empty() {
        return Ok(MergeReport {
            status: "conflict".to_owned(),
            code: Some("MERGE_CONFLICT".to_owned()),
            base_revision: worker_meta.base_revision,
            head_revision,
            merged_revision: None,
            head_changed_paths,
            worker_changed_paths,
            changed_paths: Vec::new(),
            conflicts,
            validated_checks: Vec::new(),
            failed_checks: Vec::new(),
        });
    }

    let mut validated_checks = Vec::new();
    let mut failed_checks = Vec::new();
    if !commands.0.is_empty() {
        let staging = tempfile::Builder::new().prefix("merge-").tempdir_in(main_ctx.root().join(".sourceharness")).map_err(|e| e.to_string())?;
        materialize_manifest(staging.path(), &merged_manifest, &main_blobs, Some(&worker_blobs))?;
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = commands.0.iter().map(|(name, cfg)| {
                let name = name.clone();
                let staging_path = staging.path();
                (name, scope.spawn(move || run_check_in_dir(staging_path, cfg)))
            }).collect();
            handles.into_iter().map(|(name, h)| {
                let res = h.join().unwrap_or_else(|_| Err("Merge validation thread panicked".to_owned()));
                (name, res)
            }).collect()
        });
        for (name, outcome) in results {
            let (passed, exit_code, timed_out, preview) = outcome?;
            if passed {
                validated_checks.push(name);
            } else {
                failed_checks.push(FailedCheck { check: name, exit_code, timed_out, preview });
            }
        }
        if !failed_checks.is_empty() {
            return Ok(MergeReport {
                status: "validation_failed".to_owned(),
                code: Some("MERGE_VALIDATION_FAILED".to_owned()),
                base_revision: worker_meta.base_revision,
                head_revision,
                merged_revision: None,
                head_changed_paths,
                worker_changed_paths,
                changed_paths: Vec::new(),
                conflicts: Vec::new(),
                validated_checks,
                failed_checks,
            });
        }
    }

    let changed_paths = apply_manifest_diff(
        main_ctx.root(),
        &all_paths,
        &head_manifest,
        &merged_manifest,
        &main_blobs,
        Some(&worker_blobs),
    )?;

    let merged_revision = put_blob(&main_blobs, &serde_json::to_vec(&merged_manifest).map_err(|e| e.to_string())?)?;
    if let Some(sess_name) = main_session {
        let sdir = session_path(main_ctx.root(), sess_name)?;
        if sdir.exists() {
            let mut journal = Journal::open(&sdir)?;
            let run = journal.events.last().map(|e| e.run).unwrap_or(0);
            journal.append(run, "merge_receipt", json!({
                "status": "ok",
                "worker": worker_meta.worker_name,
                "base_revision": worker_meta.base_revision,
                "head_revision": head_revision,
                "workspace_revision": merged_revision,
                "changed_paths": changed_paths,
                "validated_checks": validated_checks,
            }))?;
        }
    }

    Ok(MergeReport {
        status: "ok".to_owned(),
        code: None,
        base_revision: worker_meta.base_revision,
        head_revision,
        merged_revision: Some(merged_revision),
        head_changed_paths,
        worker_changed_paths,
        changed_paths,
        conflicts: Vec::new(),
        validated_checks,
        failed_checks: Vec::new(),
    })
}

fn three_way_merge(
    base_manifest: &Manifest,
    head_manifest: &Manifest,
    worker_manifest: &Manifest,
    primary_blobs: &Path,
    fallback_blobs: Option<&Path>,
) -> Result<ThreeWayOutcome, String> {
    let all_paths: BTreeSet<String> = base_manifest.keys()
        .chain(head_manifest.keys())
        .chain(worker_manifest.keys())
        .cloned()
        .collect();

    let mut head_changed_paths = Vec::new();
    let mut worker_changed_paths = Vec::new();
    let mut merged_manifest: Manifest = BTreeMap::new();
    let mut conflicts = Vec::new();

    for path in &all_paths {
        let (b, h, w) = (base_manifest.get(path), head_manifest.get(path), worker_manifest.get(path));
        if h != b { head_changed_paths.push(path.clone()); }
        if w != b { worker_changed_paths.push(path.clone()); }

        if w == b {
            if let Some(hh) = h { merged_manifest.insert(path.clone(), hh.clone()); }
        } else if h == b {
            if let Some(wh) = w { merged_manifest.insert(path.clone(), wh.clone()); }
        } else if h == w {
            if let Some(hh) = h { merged_manifest.insert(path.clone(), hh.clone()); }
        } else {
            match (h, w) {
                (Some(hh), Some(wh)) => {
                    let b_bytes = match b {
                        Some(bh) => read_blob_from(primary_blobs, fallback_blobs, bh)?,
                        None => Vec::new(),
                    };
                    let h_bytes = read_blob_from(primary_blobs, fallback_blobs, hh)?;
                    let w_bytes = read_blob_from(primary_blobs, fallback_blobs, wh)?;
                    match (std::str::from_utf8(&b_bytes), std::str::from_utf8(&h_bytes), std::str::from_utf8(&w_bytes)) {
                        (Ok(bs), Ok(hs), Ok(ws)) => match diffy::merge(bs, hs, ws) {
                            Ok(merged) => {
                                let mh = put_blob(primary_blobs, merged.as_bytes())?;
                                merged_manifest.insert(path.clone(), mh);
                            }
                            Err(_) => conflicts.push(path.clone()),
                        },
                        _ => conflicts.push(path.clone()),
                    }
                }
                _ => conflicts.push(path.clone()),
            }
        }
    }

    Ok(ThreeWayOutcome {
        all_paths,
        head_changed_paths,
        worker_changed_paths,
        merged_manifest,
        conflicts,
    })
}

fn apply_manifest_diff(
    dest_root: &Path,
    all_paths: &BTreeSet<String>,
    current_manifest: &Manifest,
    target_manifest: &Manifest,
    primary_blobs: &Path,
    fallback_blobs: Option<&Path>,
) -> Result<Vec<String>, String> {
    let mut changed_paths = Vec::new();
    for path in all_paths {
        let (cur, next) = (current_manifest.get(path), target_manifest.get(path));
        if cur == next { continue; }
        if !tools::permitted(Path::new(path)) {
            return Err(format!("Unsafe merge path: {path}"));
        }
        let target = dest_root.join(path);
        match next {
            Some(nh) => {
                let parent = target.parent().ok_or("Missing merge target parent")?;
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                let bytes = read_blob_from(primary_blobs, fallback_blobs, nh)?;
                let mut temp = tempfile::Builder::new().prefix(".sourceharness-write-").tempfile_in(parent).map_err(|e| e.to_string())?;
                temp.write_all(&bytes).map_err(|e| e.to_string())?;
                temp.as_file().sync_all().map_err(|e| e.to_string())?;
                temp.persist(&target).map_err(|e| e.error.to_string())?;
            }
            None => {
                if target.is_file() {
                    fs::remove_file(&target).map_err(|e| e.to_string())?;
                }
            }
        }
        changed_paths.push(path.clone());
    }
    Ok(changed_paths)
}

pub fn rebase_worker(main_root: &Path, worker_dir: &Path) -> Result<RebaseReport, String> {
    let main_ctx = WorkspaceContext::new(WorkspaceConfig::with_root(main_root))?;
    let worker_ctx = WorkspaceContext::new(WorkspaceConfig::with_root(worker_dir))?;
    let worker_meta_path = worker_ctx.root().join(".sourceharness/worker.json");
    let mut worker_meta: WorkerInfo = serde_json::from_slice(
        &fs::read(&worker_meta_path).map_err(|e| format!("Missing worker metadata: {e}"))?
    ).map_err(|e| e.to_string())?;

    fs::create_dir_all(worker_ctx.root().join(".sourceharness")).map_err(|e| e.to_string())?;
    let ws_lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(worker_ctx.root().join(".sourceharness/workspace.lock")).map_err(|e| e.to_string())?;
    ws_lock.try_lock().map_err(|e| format!("Worker workspace is locked by an active session: {e}"))?;

    let main_blobs = main_ctx.root().join(".sourceharness/blobs");
    let worker_blobs = worker_ctx.root().join(".sourceharness/blobs");
    let base_manifest: Manifest = serde_json::from_slice(
        &read_blob_from(&worker_blobs, Some(&main_blobs), &worker_meta.base_revision)?
    ).map_err(|e| e.to_string())?;
    let (head_manifest, head_revision) = capture_manifest(&main_ctx, &worker_blobs)?;
    let (worker_manifest, _worker_rev) = capture_manifest(&worker_ctx, &worker_blobs)?;

    let outcome = three_way_merge(
        &base_manifest,
        &head_manifest,
        &worker_manifest,
        &worker_blobs,
        Some(&main_blobs),
    )?;

    if !outcome.conflicts.is_empty() {
        return Ok(RebaseReport {
            status: "conflict".to_owned(),
            code: Some("REBASE_CONFLICT".to_owned()),
            base_revision: worker_meta.base_revision,
            head_revision,
            new_base_revision: None,
            head_changed_paths: outcome.head_changed_paths,
            worker_changed_paths: outcome.worker_changed_paths,
            updated_paths: Vec::new(),
            conflicts: outcome.conflicts,
        });
    }

    let updated_paths = apply_manifest_diff(
        worker_ctx.root(),
        &outcome.all_paths,
        &worker_manifest,
        &outcome.merged_manifest,
        &worker_blobs,
        Some(&main_blobs),
    )?;

    let old_base = worker_meta.base_revision.clone();
    worker_meta.base_revision = head_revision.clone();
    let meta_parent = worker_meta_path.parent().ok_or("Missing worker metadata parent")?;
    let mut temp = tempfile::Builder::new().prefix(".sourceharness-worker-").tempfile_in(meta_parent).map_err(|e| e.to_string())?;
    temp.write_all(&serde_json::to_vec_pretty(&worker_meta).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(&worker_meta_path).map_err(|e| e.error.to_string())?;

    Ok(RebaseReport {
        status: "ok".to_owned(),
        code: None,
        base_revision: old_base,
        head_revision: head_revision.clone(),
        new_base_revision: Some(head_revision),
        head_changed_paths: outcome.head_changed_paths,
        worker_changed_paths: outcome.worker_changed_paths,
        updated_paths,
        conflicts: Vec::new(),
    })
}
