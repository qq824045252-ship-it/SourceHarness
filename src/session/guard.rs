use super::journal::{revision, Journal};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const WARN_THRESHOLD: usize = 2;
const PAUSE_THRESHOLD: usize = 3;

pub(super) fn compact_signal(
    journal: &Journal, run: usize, round: usize, tool: &str, args: &Value,
    result: &Value, workspace: &str,
) -> Value {
    let validation = tool == "validate" && result["passed"] == false;
    let conditions = json!({"check":result["check"],"program":result["program_revision"],"inputs":result["input_revisions"]});
    let failed_revision = validation.then(|| revision(format!("{workspace}/{conditions}").as_bytes()));
    let detail = if validation {
        json!({"conditions":conditions,"exit":result["exit_code"],"timed_out":result["timed_out"],"streams":result["streams"]})
    } else {
        json!({"tool":tool,"args":args,"code":result["code"],"exit":result["exit_code"],"streams":result["streams"]})
    };
    let diagnostic = revision(detail.to_string().as_bytes());
    let fingerprint = revision(format!("{workspace}/{diagnostic}").as_bytes());
    let mut exact = HashSet::from([round as u64]);
    let mut repeated_revision = exact.clone();
    let mut consecutive_validation = exact.clone();
    let mut same_diagnostic = validation;
    for event in journal.events.iter().rev().take_while(|event| event.run == run) {
        if matches!(event.kind.as_str(), "steer" | "rollback_receipt" | "paused")
            || (validation && event.kind == "validation_receipt" && event.data["passed"] == true
                && event.data["check"] == result["check"])
        {
            break;
        }
        if event.kind != "tool_result" {
            continue;
        }
        let Some(prior_round) = event.data["round"].as_u64() else { continue };
        let previous = &event.data["guard"];
        if previous["fingerprint"] == fingerprint {
            exact.insert(prior_round);
        }
        if failed_revision.as_deref().is_some_and(|hash| previous["failed_revision"] == hash) {
            repeated_revision.insert(prior_round);
        }
        if same_diagnostic && event.data["name"] == "validate" {
            same_diagnostic = previous["diagnostic"] == diagnostic;
            if same_diagnostic {
                consecutive_validation.insert(prior_round);
            }
        }
    }
    let count = exact.len().max(consecutive_validation.len()).max(repeated_revision.len());
    let state = match count {
        PAUSE_THRESHOLD.. => "pause",
        WARN_THRESHOLD.. => "warn",
        _ => "observe",
    };
    json!({
        "kind":if validation { "VALIDATION_OSCILLATION" } else { "REPEATED_FAILURE" },
        "state":state,"count":count,"workspace_revision":workspace,
        "diagnostic":diagnostic,"fingerprint":fingerprint,"failed_revision":failed_revision
    })
}

pub struct PendingOutcome<'a> {
    pub operation: &'a str,
    pub attempt: usize,
    pub name: &'a str,
    pub result: &'a Value,
}

/// Report deterministic signs of repeated work within the current run.
///
/// Signals:
/// 1. `EXACT_REPEAT`: identical `(tool_name, args_hash)` repeated with no
///    intervening successful workspace mutation (`edit_receipt`).
/// 2. `ERROR_STREAK`: consecutive error results on the same `(tool_name, target, code)`.
/// 3. `VALIDATION_OSCILLATION`: repeated identical failed validation fingerprint
///    or returning to a previously failed `workspace_revision` for the same check.
pub fn evaluate(journal: &Journal, run: usize, pending: &PendingOutcome<'_>) -> Option<Value> {
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
                "Repeated identical `{}` call {} times without an intervening recorded edit.",
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

    let (count, kind, reason) = candidates
        .into_iter()
        .max_by_key(|(count, _, _)| *count)
        .filter(|(count, _, _)| *count >= WARN_THRESHOLD)?;

    Some(json!({"kind": kind, "count": count, "reason": reason}))
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
    fn compact_detects_failed_workspace_oscillation_across_different_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        for (round, workspace, expected) in [
            (1, "a", "observe"),
            (2, "b", "observe"),
            (3, "a", "warn"),
            (4, "b", "warn"),
            (5, "a", "pause"),
        ] {
            let result = json!({"check":"test","program_revision":"runner","passed":false,
                "exit_code":1,"streams":{"stderr":format!("diagnostic {round}")}});
            let signal = compact_signal(&journal, 1, round, "validate", &json!({}), &result, workspace);
            assert_eq!(signal["state"], expected);
            journal.append(1, "tool_result", json!({
                "name":"validate", "round":round, "guard":signal
            })).unwrap();
        }
        journal.append(1, "steer", json!({"text":"Try a different approach."})).unwrap();
        let result = json!({"check":"test","program_revision":"runner","passed":false,"exit_code":1});
        let after_steer = compact_signal(&journal, 1, 6, "validate", &json!({}), &result, "a");
        assert_eq!(after_steer["state"], "observe");
    }

    #[test]
    fn exact_repeat_warns_and_resets_after_edit() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let args = journal.put(br#"{"path":"a.txt"}"#).unwrap();
        let ok = json!({"status":"ok","content":"hi"});

        let a1 = journal
            .append(1, "tool_attempt", json!({"operation":"1.1.1","name":"read","arguments":args}))
            .unwrap();
        assert_eq!(
            evaluate(&journal, 1, &PendingOutcome { operation: "1.1.1", attempt: a1, name: "read", result: &ok }),
            None
        );
        journal
            .append(1, "tool_result", json!({"operation":"1.1.1","attempt":a1,"name":"read","status":"ok"}))
            .unwrap();

        let a2 = journal
            .append(1, "tool_attempt", json!({"operation":"1.2.1","name":"read","arguments":args}))
            .unwrap();
        assert!(matches!(
            evaluate(&journal, 1, &PendingOutcome { operation: "1.2.1", attempt: a2, name: "read", result: &ok }),
            Some(ref w) if w["kind"] == "EXACT_REPEAT" && w["count"] == 2
        ));
        journal
            .append(1, "tool_result", json!({"operation":"1.2.1","attempt":a2,"name":"read","status":"ok"}))
            .unwrap();

        let a3 = journal
            .append(1, "tool_attempt", json!({"operation":"1.3.1","name":"read","arguments":args}))
            .unwrap();
        assert!(matches!(
            evaluate(&journal, 1, &PendingOutcome { operation: "1.3.1", attempt: a3, name: "read", result: &ok }),
            Some(ref p) if p["kind"] == "EXACT_REPEAT" && p["count"] == 3
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
            None
        );
    }

    #[test]
    fn error_streak_warns_on_same_target_and_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let err = json!({"status":"error","code":"TEXT_NOT_FOUND","source":{"kind":"workspace","path":"app.py"}});

        for (round, expected) in [(1, false), (2, true), (3, true)] {
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
            if expected {
                let warning = action.unwrap();
                assert_eq!(warning["kind"], "ERROR_STREAK");
                assert_eq!(warning["count"], round);
            } else {
                assert_eq!(action, None);
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

        for (round, ws_rev, expected) in [(1, "rev-a", false), (2, "rev-b", true), (3, "rev-a", true)] {
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
            if expected {
                let warning = action.unwrap();
                assert_eq!(warning["kind"], "VALIDATION_OSCILLATION");
                assert_eq!(warning["count"], round);
            } else {
                assert_eq!(action, None);
            }
            journal
                .append(1, "tool_result", json!({"operation":op,"attempt":attempt,"name":"validate","status":"ok"}))
                .unwrap();
        }
    }
}
