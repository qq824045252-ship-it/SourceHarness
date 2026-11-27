use super::journal::{Event, Journal};
use super::tools::{rejected, restore_file, Reply};
use super::validation::workspace_version;
use crate::workspace::WorkspaceContext;
use serde_json::{json, Value};
use std::collections::BTreeMap;

type Manifest = BTreeMap<String, String>;

fn load_manifest(journal: &Journal, hash: &str) -> Result<Manifest, String> {
    serde_json::from_slice(&journal.get(hash)?).map_err(|err| err.to_string())
}

fn checkpoint(event: &Event) -> Option<&str> {
    match event.kind.as_str() {
        "user" => event.data["workspace_revision_at_start"].as_str(),
        "validation_receipt" if event.data["passed"] == true
            && event.data["workspace_changed_during_check"] == false =>
        {
            event.data["workspace_revision"].as_str()
        }
        _ => None,
    }
}

pub(super) fn run(
    ctx: &WorkspaceContext,
    journal: &mut Journal,
    run: usize,
    operation: &str,
    args: &Value,
) -> Reply {
    if !args.as_object().is_some_and(|args| args.is_empty()) {
        return Ok(rejected("INVALID_ARGUMENT", "Call rollback with no arguments."));
    }
    if let Some(saved) = journal.events.iter().rev().find(|event| {
        event.kind == "rollback_receipt" && event.data["operation"] == operation
    }) {
        return Ok(saved.data.clone());
    }

    let actual = workspace_version(ctx, journal)?;
    let plan = if let Some(saved) = journal.events.iter().find(|event| {
        event.kind == "rollback_intent" && event.data["operation"] == operation
    }) {
        saved.data.clone()
    } else {
        let Some((entry, target)) = journal.events.iter().rev()
            .filter(|event| event.run == run)
            .find_map(|event| checkpoint(event).map(|hash| (event, hash)))
        else {
            return Ok(rejected("NO_CHECKPOINT", "This run has no recorded workspace checkpoint."));
        };
        let original = load_manifest(journal, target)?;
        let mut expected = original.clone();
        for event in &journal.events[entry.seq..] {
            match event.kind.as_str() {
                "edit_receipt" if event.data["status"] == "ok" => {
                    let path = event.data["source"]["path"].as_str().ok_or("Edit receipt has no path")?;
                    if event.data["before_revision"].as_str() != expected.get(path).map(String::as_str) {
                        return Ok(conflict(Some(path)));
                    }
                    let revision = event.data["revision"].as_str().ok_or("Edit receipt has no revision")?;
                    expected.insert(path.to_owned(), revision.to_owned());
                }
                "rollback_receipt" => {
                    let revision = event.data["target"].as_str().ok_or("Rollback has no target")?;
                    expected = load_manifest(journal, revision)?;
                }
                _ => {}
            }
        }
        let current = load_manifest(journal, &actual)?;
        if current != expected {
            let path = current.keys().chain(expected.keys())
                .find(|path| current.get(*path) != expected.get(*path)).map(String::as_str);
            return Ok(conflict(path));
        }
        let plan = json!({"operation":operation,"from":actual,"target":target,"checkpoint_event":entry.seq});
        journal.append(run, "rollback_intent", plan.clone())?;
        plan
    };

    let target = plan["target"].as_str().ok_or("Rollback has no checkpoint")?;
    let baseline = load_manifest(journal, target)?;
    let from = load_manifest(journal, plan["from"].as_str().ok_or("Rollback has no source")?)?;
    let current = load_manifest(journal, &actual)?;
    for path in current.keys().chain(from.keys()).chain(baseline.keys()) {
        let observed = current.get(path);
        if observed != from.get(path) && observed != baseline.get(path) {
            // Leave the intent pending so resume preserves the original target.
            return Err(format!("Rollback paused because {path} changed. Inspect it before resuming."));
        }
    }

    let mut changed = Vec::new();
    for (index, (path, before)) in from.iter().enumerate() {
        let after = baseline.get(path);
        if after == Some(before) {
            continue;
        }
        let step = format!("{operation}.restore.{index}");
        let result = restore_file(ctx, journal, run, &step, path, before, after.map(String::as_str))?;
        if result["status"] != "ok" {
            let reason = result["reason"].as_str().unwrap_or("File changed.");
            return Err(format!("Rollback paused at {path}. {reason}"));
        }
        changed.push(path);
        if std::env::var("SOURCE_HARNESS_TEST_INTERRUPT").as_deref() == Ok("during_rollback") {
            std::process::exit(86);
        }
    }
    if workspace_version(ctx, journal)? != target {
        return Err("Rollback paused because the current workspace differs from the checkpoint. Inspect the workspace before resuming.".to_owned());
    }
    let result = json!({
        "status":"ok", "operation":operation, "target":target, "from":plan["from"],
        "checkpoint_event":plan["checkpoint_event"], "paths":changed
    });
    journal.append(run, "rollback_receipt", result.clone())?;
    Ok(result)
}

fn conflict(path: Option<&str>) -> Value {
    let mut result = rejected("ROLLBACK_CONFLICT", "The workspace changed outside this run's recorded edits. The checkpoint was left intact.");
    result["path"] = json!(path);
    result
}
