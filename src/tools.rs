use crate::model::{self, ToolCall};
use serde_json::{json, Value};

const NAME: &str = "add_numbers";
const LIMIT: i64 = 1_000_000;
pub fn add_schema() -> Value {
    json!({"type":"function","function":{"name":NAME,
        "description":"Compute the exact sum of two signed integers. Use when asked to perform an actual calculator operation.",
        "parameters":{"type":"object","properties":{"a":{"type":"integer","minimum":-LIMIT,"maximum":LIMIT},"b":{"type":"integer","minimum":-LIMIT,"maximum":LIMIT}},"required":["a","b"],"additionalProperties":false}}})
}
pub async fn smoke(execute: bool) -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        time::{Instant, SystemTime, UNIX_EPOCH},
    };
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    fs::create_dir_all("evidence")?;
    let path = format!("evidence/tools-{epoch}-{}.jsonl", std::process::id());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(&path)?;
    let mut ok = true;
    let cases = if execute {
        vec![
            (
                "E04",
                "Use the calculator tool to add 71023 and -2456. Actually call it.",
                false,
                true,
                false,
                (71023, -2456),
            ),
            (
                "E05",
                "Use the calculator tool to add -518 to 591. Actually call it.",
                false,
                true,
                true,
                (-518, 591),
            ),
        ]
    } else {
        vec![
        ("T02","Use add_numbers to add -90213 to 2756. Actually call the tool.",false,true,false,(-90213,2756)),
        ("T03","Add 7639 and -4321.",true,true,false,(7639,-4321)),
        ("T04","Use the available calculator to actually calculate the exact sum of 39022 and -613. The operation must be performed rather than answered from memory.",false,true,false,(39022,-613)),
        ("T05","Reply with exactly hello. Do not calculate anything.",false,false,false,(0,0)),
    ]
    };
    for (id, text, force, requires_call, stream, pair) in cases {
        let mut req = model::request(stream, text);
        req["tools"] = json!([add_schema()]);
        req["tool_choice"] = if force {
            json!({"type":"function","function":{"name":NAME}})
        } else {
            json!("auto")
        };
        let started = Instant::now();
        let (pass, detail) = match model::send(&req).await {
            Ok((reply, wire)) => {
                let got = match reply.calls.as_slice() {
                    [call] if call.name == NAME => args(call).ok(),
                    _ => None,
                };
                let exec_result = if execute && got == Some(pair) {
                    execute_add(&reply.calls[0]).ok()
                } else {
                    None
                };
                let pass = if requires_call {
                    got == Some(pair) && (!execute || exec_result == Some(pair.0 + pair.1))
                } else {
                    reply.calls.is_empty() && reply.text.trim() == "hello"
                };
                (
                    pass,
                    json!({"reply":reply,"response":model::safe_wire(&wire,stream).ok(),"args_valid":got.is_some(),"tool_result":exec_result}),
                )
            }
            Err(e) => (false, json!({"error_class":e.to_string()})),
        };
        ok &= pass;
        let status = if pass { "PASS" } else { "FAIL" };
        writeln!(
            out,
            "{}",
            json!({"test_id":id,"attempt_id":format!("{epoch}-{id}"),"request":req,"provider":"Magpie/OpenCode Go","platform":std::env::consts::OS,"elapsed_ms":started.elapsed().as_millis(),"status":status,"detail":detail})
        )?;
        println!("{id}: {status}");
    }
    out.sync_all()?;
    println!("Evidence: {path}");
    if ok {
        Ok(())
    } else {
        Err("Tool smoke did not pass".into())
    }
}
fn args(call: &ToolCall) -> Result<(i64, i64), &'static str> {
    if call.name != NAME {
        return Err("unknown_tool");
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct AddArgs {
        a: i64,
        b: i64,
    }
    let AddArgs { a, b } =
        serde_json::from_str(&call.arguments).map_err(|_| "invalid_arguments")?;
    if !(-LIMIT..=LIMIT).contains(&a) || !(-LIMIT..=LIMIT).contains(&b) {
        return Err("invalid_arguments");
    }
    Ok((a, b))
}
fn execute_add(call: &ToolCall) -> Result<i64, &'static str> {
    let (a, b) = args(call)?;
    Ok(a + b)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "fixture".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }
    #[test]
    fn t01_schema_matches_source() {
        let s = add_schema();
        assert_eq!(s["function"]["name"], NAME);
        assert_eq!(s["function"]["parameters"]["required"], json!(["a", "b"]));
        for k in ["a", "b"] {
            assert_eq!(
                s["function"]["parameters"]["properties"][k],
                json!({"type":"integer","minimum":-LIMIT,"maximum":LIMIT})
            );
        }
    }
    #[test]
    fn e01_real_signed_calculation() {
        assert_eq!(execute_add(&call(NAME, r#"{"a":5,"b":7}"#)), Ok(12));
        assert_eq!(execute_add(&call(NAME, r#"{"a":0,"b":-7}"#)), Ok(-7));
        assert_eq!(execute_add(&call(NAME, r#"{"a":-5,"b":5}"#)), Ok(0));
    }
    #[test]
    fn e02_invalid_and_e03_unknown() {
        for x in [
            r#"{"a":"5","b":7}"#,
            r#"{"a":1}"#,
            r#"{"a":1,"a":2,"b":3}"#,
            r#"{"a":1000001,"b":0}"#,
            r#"{"a":3,"b":4,"c":1}"#,
            "{",
        ] {
            assert_eq!(execute_add(&call(NAME, x)), Err("invalid_arguments"));
        }
        assert_eq!(
            execute_add(&call("unknown_tool", r#"{"a":5,"b":7}"#)),
            Err("unknown_tool")
        );
    }
}
