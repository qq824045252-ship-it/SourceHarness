use super::journal::{Event, Journal};
use super::tools::{parse, rejected, restore_file, workspace_path, Reply, ResolvePath};
use super::validation::workspace_version;
use crate::budget::truncate_utf8_bytes;
use crate::workspace::WorkspaceContext;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

type Manifest = BTreeMap<String, String>;

#[derive(Deserialize)]
struct Args { paths: Option<Vec<String>>, to_event: Option<usize>, dry_run: Option<bool> }

fn load_manifest(journal: &Journal, hash: &str) -> Result<Manifest, String> {
    serde_json::from_slice(&journal.get(hash)?).map_err(|err| err.to_string())
}

fn checkpoint(event: &Event) -> Option<&str> {
    match event.kind.as_str() {
        "user" => event.data["workspace_revision_at_start"].as_str(),
        "validation_receipt" if event.data["passed"] == true && event.data["workspace_changed_during_check"] == false => {
            event.data["workspace_revision"].as_str()
        }
        _ => None,
    }
}

fn event_checkpoint(event: &Event) -> Option<&str> {
    match event.kind.as_str() {
        "user" => event.data["workspace_revision_at_start"].as_str(),
        "validation_receipt" => event.data["workspace_revision"].as_str(),
        "exec_receipt" => event.data["before_workspace_revision"].as_str(),
        _ => None,
    }
}

pub(super) fn run(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str, raw_args: &Value) -> Reply {
    let args: Args = match parse(raw_args) { Ok(a) => a, Err(e) => return Ok(e) };
    let path_filter = match args.paths {
        None => None,
        Some(list) if list.is_empty() => {
            return Ok(rejected("INVALID_ARGUMENT", "Supply one or more workspace-relative paths, or omit paths to rollback all touched files."));
        }
        Some(list) => {
            let mut set = BTreeSet::new();
            for p in list {
                match workspace_path(ctx, &p, ResolvePath::MissingParents) {
                    Ok((_, rel)) => { set.insert(rel); }
                    Err(e) => return Ok(e),
                }
            }
            Some(set)
        }
    };
    if args.dry_run != Some(true) {
        if let Some(saved) = journal.events.iter().rev().find(|e| e.kind == "rollback_receipt" && e.data["operation"] == operation) {
            return Ok(saved.data.clone());
        }
    }
    let actual = workspace_version(ctx, journal)?;
    let existing_intent = if args.dry_run == Some(true) { None }
        else { journal.events.iter().find(|e| e.kind == "rollback_intent" && e.data["operation"] == operation).map(|e| e.data.clone()) };
    let plan = if let Some(saved) = existing_intent { saved } else {
        let entry = match args.to_event {
            Some(seq) => journal.events.iter().find(|e| e.run == run && e.seq == seq).and_then(|e| event_checkpoint(e).map(|h| (e, h)))
                .ok_or_else(|| rejected("INVALID_ARGUMENT", "to_event must reference a user, exec_receipt, or validation_receipt event in the current run.")),
            None => journal.events.iter().rev().filter(|e| e.run == run).find_map(|e| checkpoint(e).map(|h| (e, h)))
                .ok_or_else(|| rejected("NO_CHECKPOINT", "This run has no recorded workspace checkpoint.")),
        };
        let entry = match entry { Ok(v) => v, Err(err) => return Ok(err) };
        let (entry_seq, target) = (entry.0.seq, entry.1.to_owned());
        let original = load_manifest(journal, &target)?;
        let (mut expected, mut touched) = (original.clone(), BTreeSet::new());
        let scan_start = if entry.0.kind == "exec_receipt" { entry_seq - 1 } else { entry_seq };
        for event in &journal.events[scan_start..] {
            match event.kind.as_str() {
                "edit_receipt" if event.data["status"] == "ok" => {
                    let path = event.data["source"]["path"].as_str().ok_or("Edit receipt has no path")?;
                    if path_filter.as_ref().is_none_or(|f| f.contains(path))
                        && event.data["before_revision"].as_str() != expected.get(path).map(String::as_str) { return Ok(conflict(Some(path))); }
                    expected.insert(path.to_owned(), event.data["revision"].as_str().ok_or("Edit receipt has no revision")?.to_owned());
                    touched.insert(path.to_owned());
                }
                "exec_receipt" => {
                    let (Some(bh), Some(ah)) = (event.data["before_workspace_revision"].as_str(), event.data["workspace_revision"].as_str()) else { continue };
                    if bh == ah { continue; }
                    let (bm, am) = (load_manifest(journal, bh)?, load_manifest(journal, ah)?);
                    for path in bm.keys().chain(am.keys()).filter(|p| bm.get(*p) != am.get(*p)).cloned().collect::<BTreeSet<_>>() {
                        if path_filter.as_ref().is_none_or(|f| f.contains(&path)) && bm.get(&path) != expected.get(&path) { return Ok(conflict(Some(&path))); }
                        match am.get(&path) { Some(r) => { expected.insert(path.clone(), r.clone()); } None => { expected.remove(&path); } }
                        touched.insert(path);
                    }
                }
                "rollback_receipt" => {
                    let restored = load_manifest(journal, event.data["target"].as_str().ok_or("Rollback has no target")?)?;
                    if let Some(paths) = event.data["paths"].as_array() {
                        for p in paths.iter().filter_map(Value::as_str) {
                            match restored.get(p) { Some(r) => { expected.insert(p.to_owned(), r.clone()); } None => { expected.remove(p); } }
                            if expected.get(p) == original.get(p) { touched.remove(p); }
                        }
                    } else { expected = restored; touched.clear(); }
                }
                _ => {}
            }
        }
        let current = load_manifest(journal, &actual)?;
        let scoped: Vec<String> = touched.into_iter().filter(|p| path_filter.as_ref().is_none_or(|f| f.contains(p))).collect();
        for path in &scoped { if current.get(path) != expected.get(path) { return Ok(conflict(Some(path))); } }
        let paths: Vec<String> = scoped.into_iter().filter(|p| current.get(p) != original.get(p)).collect();
        if args.dry_run == Some(true) {
            let mut parts = Vec::new();
            for p in &paths {
                let (cur, tgt) = (current.get(p).and_then(|h| journal.get(h).ok()).unwrap_or_default(), original.get(p).and_then(|h| journal.get(h).ok()).unwrap_or_default());
                let patch = match (std::str::from_utf8(&cur), std::str::from_utf8(&tgt)) {
                    (Ok(a), Ok(b)) => diffy::create_patch(a, b).to_string(),
                    _ => format!("--- {p}\n+++ {p}\n(binary change)\n"),
                };
                parts.push(format!("=== {p} ===\n{patch}"));
            }
            let raw = if parts.is_empty() { "No changes to revert.".to_owned() } else { parts.join("\n") };
            let (clip, clipped) = truncate_utf8_bytes(&raw, 3072);
            let preview = if clipped { format!("{clip}\n... (truncated)") } else { raw };
            return Ok(json!({"status":"ok","dry_run":true,"checkpoint_event":entry_seq,"target":target,"paths":paths,"preview":preview}));
        }
        let plan = json!({"operation":operation,"from":actual,"target":target,"checkpoint_event":entry_seq,"paths":paths});
        journal.append(run, "rollback_intent", plan.clone())?;
        plan
    };

    let target = plan["target"].as_str().ok_or("Rollback has no checkpoint")?;
    let baseline = load_manifest(journal, target)?;
    let from = load_manifest(journal, plan["from"].as_str().ok_or("Rollback has no source")?)?;
    let current = load_manifest(journal, &actual)?;
    let plan_paths: Vec<String> = plan["paths"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    for path in &plan_paths {
        let obs = current.get(path);
        if obs != from.get(path) && obs != baseline.get(path) {
            return Err(format!("Rollback paused because {path} changed. Inspect it before resuming."));
        }
    }
    let mut changed = Vec::new();
    for (index, path) in plan_paths.iter().enumerate() {
        let (before, after) = (from.get(path).map(String::as_str), baseline.get(path).map(String::as_str));
        if after == before { continue; }
        let result = restore_file(ctx, journal, run, &format!("{operation}.restore.{index}"), path, before, after)?;
        if result["status"] != "ok" { return Err(format!("Rollback paused at {path}. {}", result["reason"].as_str().unwrap_or("File changed."))); }
        changed.push(path.clone());
        if std::env::var("SOURCE_HARNESS_TEST_INTERRUPT").as_deref() == Ok("during_rollback") { std::process::exit(86); }
    }
    let final_manifest = load_manifest(journal, &workspace_version(ctx, journal)?)?;
    for path in &plan_paths {
        if final_manifest.get(path) != baseline.get(path) {
            return Err(format!("Rollback paused because {path} differs from the checkpoint. Inspect the workspace before resuming."));
        }
    }
    let recorded_from = if changed.is_empty() { target } else { plan["from"].as_str().unwrap_or(target) };
    let result = json!({"status":"ok","operation":operation,"target":target,"from":recorded_from, "checkpoint_event":plan["checkpoint_event"],"paths":changed});
    journal.append(run, "rollback_receipt", result.clone())?;
    Ok(result)
}

fn conflict(path: Option<&str>) -> Value {
    let mut result = rejected("ROLLBACK_CONFLICT", "A file changed outside this run's recorded edits. The checkpoint was left intact.");
    result["path"] = json!(path);
    result
}
