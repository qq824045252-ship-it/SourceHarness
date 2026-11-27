use super::journal::Journal;
use serde_json::{json, Value};
use std::collections::HashMap;

pub const WARN_THRESHOLD: usize = 2;
pub const PAUSE_THRESHOLD: usize = 3;

#[derive(Debug, PartialEq)]
pub enum Action {
    Allow,
    Warn(Value),
    Pause(Value),
}

pub struct PendingOutcome<'a> {
    pub operation: &'a str,
    pub attempt: usize,
    pub name: &'a str,
    pub result: &'a Value,
}

/// Evaluate deterministic loop/thrashing signals within the current run.
///
/// Signals:
/// 1. `EXACT_REPEAT`: identical `(tool_name, args_hash)` repeated with no
///    intervening successful workspace mutation (`edit_receipt`).
/// 2. `ERROR_STREAK`: consecutive error results on the same `(tool_name, target, code)`.
/// 3. `VALIDATION_OSCILLATION`: repeated identical failed validation fingerprint
///    or returning to a previously failed `workspace_revision` for the same check.
pub fn evaluate(journal: &Journal, run: usize, pending: &PendingOutcome<'_>) -> Action {
    let mut attempts: HashMap<usize, (&str, &str)> = HashMap::new();
    let mut workspace_epoch: usize = 0;
    let mut exact_counts: HashMap<(&str, &str, usize), usize> = HashMap::new();

    let mut streak_key: Option<(String, String, String)> = None;
    let mut streak_count: usize = 0;

    let mut last_val_fail: Option<(String, Option<i64>, String, String)> = None;
    let mut same_val_fail_count: usize = 0;
    let mut failed_revisions: HashMap<(String, String), usize> = HashMap::new();
    let mut latest_val_count: usize = 0;

    for event in journal.events.iter().filter(|e| e.run == run) {
        match event.kind.as_str() {
            "tool_attempt" => {
                if let (Some(name), Some(args)) = (
                    event.data["name"].as_str(),
                    event.data["arguments"].as_str(),
                ) {
                    attempts.insert(event.seq, (name, args));
                }
            }
            "edit_receipt" => {
                if event.data["status"] == "ok"
                    && event.data["operation"].as_str() != Some(pending.operation)
                {
                    workspace_epoch += 1;
                }
            }
            "tool_result" => {
                if let Some((name, args)) = event.data["attempt"]
                    .as_u64()
                    .and_then(|v| usize::try_from(v).ok())
                    .and_then(|seq| attempts.get(&seq).copied())
                {
                    *exact_counts
                        .entry((name, args, workspace_epoch))
                        .or_insert(0) += 1;
                }
                if event.data["status"] == "error" {
                    let key = error_key(
                        event.data["name"].as_str().unwrap_or(""),
                        &event.data,
                    );
                    if streak_key.as_ref() == Some(&key) {
                        streak_count += 1;
                    } else {
                        streak_key = Some(key);
                        streak_count = 1;
                    }
                } else {
                    streak_key = None;
                    streak_count = 0;
                }
            }
            "validation_receipt" => {
                if event.data["passed"] == true {
                    last_val_fail = None;
                    same_val_fail_count = 0;
                } else {
                    let check = event.data["check"].as_str().unwrap_or("").to_owned();
                    let ws_rev = event.data["workspace_revision"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let rev_count = *failed_revisions
                        .entry((check.clone(), ws_rev))
                        .and_modify(|c| *c += 1)
                        .or_insert(1);
                    let sig = (
                        check,
                        event.data["exit_code"].as_i64(),
                        event.data["streams"]["stdout"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        event.data["streams"]["stderr"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                    );
                    if last_val_fail.as_ref() == Some(&sig) {
                        same_val_fail_count += 1;
                    } else {
                        last_val_fail = Some(sig);
                        same_val_fail_count = 1;
                    }
                    if event.data["operation"].as_str() == Some(pending.operation) {
                        latest_val_count = same_val_fail_count.max(rev_count);
                    }
                }
            }
            _ => {}
        }
    }

    let exact_count = attempts
        .get(&pending.attempt)
        .map(|&(name, args)| {
            let entry = exact_counts
                .entry((name, args, workspace_epoch))
                .or_insert(0);
            *entry += 1;
            *entry
        })
        .unwrap_or(0);

    let current_streak = if pending.result["status"] == "error" {
        let key = error_key(pending.name, pending.result);
        if streak_key.as_ref() == Some(&key) {
            streak_count + 1
        } else {
            1
        }
    } else {
        0
    };

    let candidates = [
        (
            exact_count,
            "EXACT_REPEAT",
            format!(
                "Repeated identical `{}` call {} times without any workspace change.",
                pending.name, exact_count
            ),
        ),
        (
            current_streak,
            "ERROR_STREAK",
            format!(
                "Consecutive `{}` rejection `{}` ({} times). Inspect fresh evidence or change approach before retrying.",
                pending.name,
                pending.result["code"].as_str().unwrap_or("ERROR"),
                current_streak
            ),
        ),
        (
            latest_val_count,
            "VALIDATION_OSCILLATION",
            format!(
                "Validation check failed with the same signature or reverted to a previously failed workspace revision ({} times).",
                latest_val_count
            ),
        ),
    ];

    let Some((count, kind, reason)) = candidates
        .into_iter()
        .max_by_key(|(count, _, _)| *count)
        .filter(|(count, _, _)| *count >= WARN_THRESHOLD)
    else {
        return Action::Allow;
    };

    if count >= PAUSE_THRESHOLD {
        Action::Pause(json!({
            "kind": kind,
            "count": count,
            "operation": pending.operation,
            "tool": pending.name,
            "reason": reason,
        }))
    } else {
        Action::Warn(json!({
            "kind": kind,
            "count": count,
            "reason": reason,
        }))
    }
}

fn error_key(name: &str, data: &Value) -> (String, String, String) {
    let target = data["source"]["path"]
        .as_str()
        .or_else(|| data["path"].as_str())
        .or_else(|| data["result_ref"].as_str())
        .unwrap_or("");
    let code = data["code"].as_str().unwrap_or("ERROR");
    (name.to_owned(), target.to_owned(), code.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_repeat_warns_on_second_and_pauses_on_third_and_resets_after_edit() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let args = journal.put(br#"{"path":"a.txt"}"#).unwrap();
        let ok = json!({"status":"ok","content":"hi"});

        let a1 = journal
            .append(1, "tool_attempt", json!({"operation":"1.1.1","name":"read","arguments":args}))
            .unwrap();
        assert_eq!(
            evaluate(&journal, 1, &PendingOutcome { operation: "1.1.1", attempt: a1, name: "read", result: &ok }),
            Action::Allow
        );
        journal
            .append(1, "tool_result", json!({"operation":"1.1.1","attempt":a1,"name":"read","status":"ok"}))
            .unwrap();

        let a2 = journal
            .append(1, "tool_attempt", json!({"operation":"1.2.1","name":"read","arguments":args}))
            .unwrap();
        assert!(matches!(
            evaluate(&journal, 1, &PendingOutcome { operation: "1.2.1", attempt: a2, name: "read", result: &ok }),
            Action::Warn(ref w) if w["kind"] == "EXACT_REPEAT" && w["count"] == 2
        ));
        journal
            .append(1, "tool_result", json!({"operation":"1.2.1","attempt":a2,"name":"read","status":"ok"}))
            .unwrap();

        let a3 = journal
            .append(1, "tool_attempt", json!({"operation":"1.3.1","name":"read","arguments":args}))
            .unwrap();
        assert!(matches!(
            evaluate(&journal, 1, &PendingOutcome { operation: "1.3.1", attempt: a3, name: "read", result: &ok }),
            Action::Pause(ref p) if p["kind"] == "EXACT_REPEAT" && p["count"] == 3
        ));

        // After a successful edit_receipt, the workspace epoch advances so reading again is allowed.
        journal
            .append(1, "edit_receipt", json!({"operation":"1.4.1","status":"ok"}))
            .unwrap();
        let a4 = journal
            .append(1, "tool_attempt", json!({"operation":"1.5.1","name":"read","arguments":args}))
            .unwrap();
        assert_eq!(
            evaluate(&journal, 1, &PendingOutcome { operation: "1.5.1", attempt: a4, name: "read", result: &ok }),
            Action::Allow
        );
    }

    #[test]
    fn error_streak_warns_then_pauses_on_same_target_and_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let err = json!({"status":"error","code":"TEXT_NOT_FOUND","source":{"kind":"workspace","path":"app.py"}});

        for (round, expected) in [(1, "allow"), (2, "warn"), (3, "pause")] {
            let op = format!("1.{round}.1");
            let args = journal.put(format!(r#"{{"round":{round}}}"#).as_bytes()).unwrap();
            let attempt = journal
                .append(1, "tool_attempt", json!({"operation":op,"name":"edit","arguments":args}))
                .unwrap();
            let action = evaluate(
                &journal,
                1,
                &PendingOutcome { operation: &op, attempt, name: "edit", result: &err },
            );
            match expected {
                "allow" => assert_eq!(action, Action::Allow),
                "warn" => assert!(matches!(action, Action::Warn(ref w) if w["kind"] == "ERROR_STREAK")),
                "pause" => assert!(matches!(action, Action::Pause(ref p) if p["kind"] == "ERROR_STREAK")),
                _ => unreachable!(),
            }
            journal
                .append(1, "tool_result", json!({
                    "operation":op,"attempt":attempt,"name":"edit","status":"error",
                    "code":"TEXT_NOT_FOUND","source":{"kind":"workspace","path":"app.py"}
                }))
                .unwrap();
        }
    }

    #[test]
    fn validation_oscillation_detects_repeated_failure_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let ok_res = json!({"status":"ok","passed":false});

        for (round, ws_rev, expected) in [(1, "rev-a", "allow"), (2, "rev-b", "warn"), (3, "rev-a", "pause")] {
            let op = format!("1.{round}.1");
            let args = journal.put(format!(r#"{{"check":"ci","r":{round}}}"#).as_bytes()).unwrap();
            let attempt = journal
                .append(1, "tool_attempt", json!({"operation":op,"name":"validate","arguments":args}))
                .unwrap();
            journal
                .append(1, "validation_receipt", json!({
                    "operation":op,"check":"ci","passed":false,"exit_code":1,
                    "workspace_revision":ws_rev,"streams":{"stdout":"out1","stderr":"err1"}
                }))
                .unwrap();
            let action = evaluate(
                &journal,
                1,
                &PendingOutcome { operation: &op, attempt, name: "validate", result: &ok_res },
            );
            match expected {
                "allow" => assert_eq!(action, Action::Allow),
                "warn" => assert!(matches!(action, Action::Warn(ref w) if w["kind"] == "VALIDATION_OSCILLATION")),
                "pause" => assert!(matches!(action, Action::Pause(ref p) if p["kind"] == "VALIDATION_OSCILLATION")),
                _ => unreachable!(),
            }
            journal
                .append(1, "tool_result", json!({"operation":op,"attempt":attempt,"name":"validate","status":"ok"}))
                .unwrap();
        }
    }
}
