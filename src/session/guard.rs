use super::journal::{revision, Journal};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::OnceLock;

const PAUSE_THRESHOLD: usize = 3;

pub(super) fn compact_candidate(tool: &str, args: &Value, result: &Value) -> bool {
    if tool == "validate" && result["passed"] == false {
        return result["workspace_changed_during_check"] != true && result["input_revisions_stable"] != false;
    }
    result["status"] == "error" || (tool == "exec" && empty_lookup(args, result))
}

fn empty_lookup(args: &Value, result: &Value) -> bool {
    if result["status"] != "ok" || result["preview"] != "" || result["output_complete"] != true {
        return false;
    }
    let Some(command) = args["command"].as_str() else { return false };
    let mut words = command.split_ascii_whitespace();
    let program = words.next().and_then(|name| name.rsplit(['/', '\\']).next());
    // Quiet grep can find a match cleanly; simple find lookups can return empty with exit 0.
    program == Some("find")
        && !command.contains([';', '|', '&', '<', '>', '$', '\x60', '\n'])
        && !words.any(|w| matches!(w, "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-quit"))
}

pub(super) fn compact_signal(
    journal: &Journal, run: usize, round: usize, tool: &str, args: &Value, result: &Value, workspace: &str,
) -> Value {
    if tool == "validate" && result["passed"] == false {
        return validation_signal(journal, run, round, result, workspace);
    }
    let stall = match tool {
        "exec" if matches!(result["code"].as_str(), Some("EXEC_FAILED" | "EXEC_TIMEOUT")) || empty_lookup(args, result) => {
            Some("EXEC_WITHOUT_PROGRESS")
        }
        "edit" => Some("EDIT_ERROR_STREAK"),
        _ => None,
    };
    let detail = json!({"tool":tool,"args":args,"code":result["code"],"exit":result["exit_code"],"streams":result["streams"]});
    let diagnostic = revision(detail.to_string().as_bytes());
    let fingerprint = revision(format!("{workspace}/{diagnostic}").as_bytes());
    let mut exact = HashSet::from([round as u64]);
    let mut stalled = exact.clone();
    let (mut same_exact, mut same_stall) = (true, stall.is_some());
    for event in journal.events.iter().rev().take_while(|e| e.run == run) {
        if matches!(event.kind.as_str(), "steer" | "paused" | "edit_receipt")
            || (event.kind == "rollback_receipt" && event.data["from"] != event.data["target"])
            || (event.kind == "validation_receipt" && event.data["passed"] == true)
        {
            break;
        }
        if event.kind != "tool_result" || event.data["name"] != tool { continue; }
        let Some(prior) = event.data["round"].as_u64() else { continue };
        if prior == round as u64 { continue; }
        let prev = &event.data["guard"];
        same_exact &= prev["fingerprint"] == fingerprint;
        same_stall &= prev["stall"].as_str() == stall && prev["workspace_revision"] == workspace;
        if same_exact { exact.insert(prior); }
        if same_stall { stalled.insert(prior); }
        if !same_exact && !same_stall { break; }
    }
    let (mut kind, mut count, mut state) = ("REPEATED_FAILURE", exact.len(), guard_state(exact.len(), PAUSE_THRESHOLD));
    if let Some(stall) = stall {
        let stalled_state = guard_state(stalled.len(), if tool == "exec" { 4 } else { PAUSE_THRESHOLD });
        if stalled_state > state {
            kind = stall;
            count = stalled.len();
            state = stalled_state;
        }
    }
    json!({"kind":kind,"state":state.label(),"count":count,"workspace_revision":workspace,
        "diagnostic":diagnostic,"fingerprint":fingerprint,"stall":stall})
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum GuardState { Observe, Warn, Pause }

impl GuardState {
    fn label(&self) -> &'static str {
        match self { Self::Observe => "observe", Self::Warn => "warn", Self::Pause => "pause" }
    }
}

fn guard_state(count: usize, pause: usize) -> GuardState {
    if count >= pause { GuardState::Pause } else if count >= pause - 1 { GuardState::Warn } else { GuardState::Observe }
}

fn validation_conditions(result: &Value) -> Value {
    json!({"check":result["check"],"program":result["program_revision"],"inputs":result["input_revisions"]})
}

fn normalized_stream(journal: &Journal, hash: &Value) -> Value {
    static VOLATILE: OnceLock<Regex> = OnceLock::new();
    let Some(bytes) = hash.as_str().and_then(|h| journal.get(h).ok()) else { return hash.clone() };
    let Ok(text) = std::str::from_utf8(&bytes) else { return hash.clone() };
    if text.len() > 8192 { return hash.clone(); }
    let volatile = VOLATILE.get_or_init(|| Regex::new(concat!(
        r"(?i)\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?",
        r"|(?P<label>\b(?:timestamp|line|column|pid)[ \t:=#]+)\d+\b",
    )).expect("valid diagnostic pattern"));
    json!(revision(volatile.replace_all(text, "$label<volatile>").as_bytes()))
}

fn validation_signal(journal: &Journal, run: usize, round: usize, result: &Value, workspace: &str) -> Value {
    let conditions = validation_conditions(result);
    let failed_revision = revision(format!("{workspace}/{conditions}").as_bytes());
    let detail = json!({"conditions":conditions,"exit":result["exit_code"],"timed_out":result["timed_out"],
        "streams":[normalized_stream(journal, &result["streams"]["stdout"]), normalized_stream(journal, &result["streams"]["stderr"])]});
    let diagnostic = revision(detail.to_string().as_bytes());
    let mut repeated_rev = HashSet::from([round as u64]);
    let mut consecutive = repeated_rev.clone();
    let mut same_diag = true;
    for event in journal.events.iter().rev().take_while(|e| e.run == run) {
        match event.kind.as_str() {
            "steer" | "paused" => break,
            "rollback_receipt" if event.data["from"] != event.data["target"] => same_diag = false,
            "validation_receipt" if event.data["passed"] == true
                && event.data["workspace_changed_during_check"] != true
                && validation_conditions(&event.data) == conditions =>
            {
                if event.data["workspace_revision"] == workspace { break; }
                same_diag = false;
            }
            "tool_result" if event.data["name"] == "validate" => {
                let Some(prior) = event.data["round"].as_u64() else { continue };
                if prior == round as u64 { continue; }
                let prev = &event.data["guard"];
                if prev["failed_revision"] == failed_revision { repeated_rev.insert(prior); }
                if prev["check"] == result["check"] {
                    same_diag &= prev["diagnostic"] == diagnostic;
                    if same_diag { consecutive.insert(prior); }
                }
            }
            _ => {}
        }
    }
    let count = repeated_rev.len().max(consecutive.len());
    json!({"kind":"VALIDATION_OSCILLATION","state":guard_state(count, PAUSE_THRESHOLD).label(),
        "count":count,"workspace_revision":workspace,"check":result["check"],
        "diagnostic":diagnostic,"failed_revision":failed_revision})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_detects_failed_workspace_oscillation_across_different_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        for (round, ws, expected) in [(1, "a", "observe"), (2, "b", "observe"), (3, "a", "warn"), (4, "b", "warn"), (5, "a", "pause")] {
            let result = json!({"check":"test","program_revision":"runner","passed":false,"exit_code":1,"streams":{"stderr":format!("diagnostic {round}")}});
            let signal = compact_signal(&journal, 1, round, "validate", &json!({}), &result, ws);
            assert_eq!(signal["state"], expected);
            journal.append(1, "tool_result", json!({"name":"validate", "round":round, "guard":signal})).unwrap();
        }
        journal.append(1, "steer", json!({"text":"Try a different approach."})).unwrap();
        let after = compact_signal(&journal, 1, 6, "validate", &json!({}), &json!({"check":"test","program_revision":"runner","passed":false,"exit_code":1}), "a");
        assert_eq!(after["state"], "observe");
    }

    #[test]
    fn compact_detects_repeated_failure_and_resets_after_edit() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let args = json!({"path":"a.txt"});
        let err = json!({"status":"error","code":"FILE_NOT_FOUND"});
        for (round, expected) in [(1, "observe"), (2, "warn"), (3, "pause")] {
            let signal = compact_signal(&journal, 1, round, "read", &args, &err, "ws-1");
            assert_eq!(signal["kind"], "REPEATED_FAILURE");
            assert_eq!(signal["state"], expected);
            journal.append(1, "tool_result", json!({"name":"read","round":round,"guard":signal})).unwrap();
        }
        journal.append(1, "edit_receipt", json!({"operation":"1.4.1","status":"ok"})).unwrap();
        let reset = compact_signal(&journal, 1, 4, "read", &args, &err, "ws-1");
        assert_eq!(reset["state"], "observe");
        assert_eq!(reset["count"], 1);
    }

    #[test]
    fn compact_detects_edit_error_streak_across_different_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        for (round, code, kind, expected) in [
            (1, "TEXT_NOT_FOUND", "REPEATED_FAILURE", "observe"),
            (2, "INVALID_ARGUMENT", "EDIT_ERROR_STREAK", "warn"),
            (3, "AMBIGUOUS_MATCH", "EDIT_ERROR_STREAK", "pause"),
        ] {
            let res = json!({"status":"error","code":code});
            let signal = compact_signal(&journal, 1, round, "edit", &json!({"path":"app.py","round":round}), &res, "ws-1");
            assert_eq!(signal["kind"], kind);
            assert_eq!(signal["state"], expected);
            journal.append(1, "tool_result", json!({"name":"edit","round":round,"guard":signal})).unwrap();
        }
    }

    #[test]
    fn compact_detects_noisy_validation_oscillation_and_ignores_empty_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        for (round, noise, expected) in [
            (1, "timestamp=100 line 12: missing", "observe"),
            (2, "timestamp=200 line 88: missing", "warn"),
            (3, "timestamp=300 line 99: missing", "pause"),
        ] {
            let err_hash = journal.put(noise.as_bytes()).unwrap();
            let res = json!({"check":"ci","program_revision":"p1","passed":false,"exit_code":1,"streams":{"stdout":"","stderr":err_hash}});
            let signal = compact_signal(&journal, 1, round, "validate", &json!({}), &res, &format!("ws-{round}"));
            assert_eq!(signal["kind"], "VALIDATION_OSCILLATION");
            assert_eq!(signal["state"], expected);
            journal.append(1, "tool_result", json!({"name":"validate","round":round,"guard":signal})).unwrap();
            journal.append(1, "rollback_receipt", json!({"from":"base","target":"base","paths":[]})).unwrap();
        }
    }
}
