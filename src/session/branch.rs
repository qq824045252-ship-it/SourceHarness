use super::journal::{get_blob, put_blob, read_events, revision, Journal};
use super::rollback::reconstruct_manifest_at;
use super::{session_path, tools};
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchInfo {
    pub session: String,
    pub parent_session: String,
    pub fork_event: usize,
    pub workspace_revision: String,
    pub restored_workspace: bool,
    pub lesson: Option<String>,
    pub created_ms: u64,
}

pub fn read_branch_info(dir: &Path) -> Option<BranchInfo> {
    let raw = fs::read(dir.join("branch.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub fn branch_session(
    root: &Path,
    from_session: &str,
    to_session: &str,
    to_event: usize,
    lesson: Option<&str>,
    restore_workspace: bool,
) -> Result<BranchInfo, String> {
    if from_session == to_session {
        return Err("Target branch session name must differ from the source session.".to_owned());
    }
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root))?;
    let from_dir = session_path(ctx.root(), from_session)?;
    let to_dir = session_path(ctx.root(), to_session)?;
    if !from_dir.join("events.jsonl").is_file() {
        return Err(format!("Source session `{from_session}` has no recorded events."));
    }
    if to_dir.exists() {
        return Err(format!("Target branch session `{to_session}` already exists."));
    }
    let _ws_lock = if restore_workspace {
        fs::create_dir_all(ctx.root().join(".sourceharness")).map_err(|e| e.to_string())?;
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(ctx.root().join(".sourceharness/workspace.lock")).map_err(|e| e.to_string())?;
        lock.try_lock().map_err(|e| format!("Workspace already has an open SourceHarness session: {e}"))?;
        Some(lock)
    } else {
        None
    };

    let (events, _) = read_events(&from_dir)?;
    if to_event == 0 || to_event > events.len() {
        return Err(format!("Branch event {to_event} is out of range (1..={}).", events.len()));
    }
    let (_, target_manifest) = reconstruct_manifest_at(&events, &from_dir, to_event, false)?;
    let to_blobs = to_dir.join("blobs");
    fs::create_dir_all(&to_blobs).map_err(|e| e.to_string())?;
    let from_blobs = from_dir.join("blobs");
    if from_blobs.is_dir() {
        for entry in fs::read_dir(&from_blobs).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.path().is_file() {
                let dst = to_blobs.join(entry.file_name());
                if !dst.exists() && fs::hard_link(entry.path(), &dst).is_err() {
                    fs::copy(entry.path(), &dst).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    let ws_rev = put_blob(&to_blobs, &serde_json::to_vec(&target_manifest).map_err(|e| e.to_string())?)?;

    fs::create_dir_all(&to_dir).map_err(|e| e.to_string())?;
    {
        let mut out = fs::File::create(to_dir.join("events.jsonl")).map_err(|e| e.to_string())?;
        for event in &events[..to_event] {
            let mut line = serde_json::to_vec(event).map_err(|e| e.to_string())?;
            line.push(b'\n');
            out.write_all(&line).map_err(|e| e.to_string())?;
        }
        out.sync_all().map_err(|e| e.to_string())?;
    }

    if restore_workspace {
        let (files, all) = tools::walk(ctx.root(), ctx.config().max_list_entries)?;
        if !all { return Err("Workspace exceeds snapshot file limit".to_owned()); }
        let mut live = BTreeSet::new();
        for path in files {
            let rel = path.strip_prefix(ctx.root()).map_err(|e| e.to_string())?;
            if tools::permitted(rel) {
                if let Some(name) = rel.to_str() {
                    live.insert(name.replace(std::path::MAIN_SEPARATOR, "/"));
                }
            }
        }
        for rel in &live {
            if !target_manifest.contains_key(rel) {
                let target = ctx.root().join(rel);
                if target.is_file() {
                    fs::remove_file(target).map_err(|e| e.to_string())?;
                }
            }
        }
        for (rel, hash) in &target_manifest {
            if !tools::permitted(Path::new(rel)) {
                return Err(format!("Unsafe branch snapshot path: {rel}"));
            }
            let target = ctx.root().join(rel);
            let same = fs::read(&target).ok().is_some_and(|b| revision(&b) == *hash);
            if !same {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let bytes = get_blob(&to_dir, hash)?;
                fs::write(&target, bytes).map_err(|e| e.to_string())?;
            }
        }
    }

    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let clean_lesson = lesson.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    if let Some(note) = &clean_lesson {
        let last_run = events[..to_event].iter().rev().find(|e| e.kind == "user").map(|e| e.run).unwrap_or(0);
        let last_resp_final = events[..to_event].iter().rev()
            .find(|e| e.run == last_run && e.kind == "model_response")
            .and_then(|e| e.data["final_text"].as_str());
        let last_round = events[..to_event].iter().rev()
            .find(|e| e.run == last_run && e.kind == "model_response")
            .and_then(|e| e.data["round"].as_u64())
            .unwrap_or(0) as usize;
        if last_run > 0 && last_resp_final.is_none() {
            let mut journal = Journal::open(&to_dir)?;
            let text = format!("Branched from {from_session} at event #{to_event}. Lesson from prior branch: {note}");
            journal.append(last_run, "steer", json!({"text": text, "received_ms": now_ms, "after_round": last_round}))?;
        }
    }

    let info = BranchInfo {
        session: to_session.to_owned(),
        parent_session: from_session.to_owned(),
        fork_event: to_event,
        workspace_revision: ws_rev,
        restored_workspace: restore_workspace,
        lesson: clean_lesson,
        created_ms: now_ms,
    };
    let meta = serde_json::to_vec_pretty(&info).map_err(|e| e.to_string())?;
    fs::write(to_dir.join("branch.json"), meta).map_err(|e| e.to_string())?;
    Ok(info)
}
