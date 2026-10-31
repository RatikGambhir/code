//! The parts of the Debug Adapter Protocol Hangar uses, as typed messages.
//!
//! <https://microsoft.github.io/debug-adapter-protocol/specification>
//!
//! Lines and columns are one-based on the wire (the client says so in
//! `initialize`); the rest of Hangar is zero-based, and converts at the edge.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Capabilities {
    pub supports_configuration_done_request: bool,
    pub supports_function_breakpoints: bool,
    pub supports_conditional_breakpoints: bool,
    pub supports_hit_conditional_breakpoints: bool,
    pub supports_log_points: bool,
    pub supports_evaluate_for_hovers: bool,
    pub supports_set_variable: bool,
    pub supports_restart_request: bool,
    pub supports_terminate_request: bool,
    pub support_terminate_debuggee: bool,
    pub supports_completions_request: bool,
    pub supports_stepping_granularity: bool,
    pub exception_breakpoint_filters: Vec<ExceptionBreakpointsFilter>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExceptionBreakpointsFilter {
    pub filter: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Source {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_reference: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceBreakpoint {
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionBreakpoint {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
}

/// The adapter's view of a breakpoint we set.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Breakpoint {
    pub id: Option<i64>,
    pub verified: bool,
    pub message: Option<String>,
    pub source: Option<Source>,
    pub line: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Thread {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StackFrame {
    pub id: i64,
    pub name: String,
    pub source: Option<Source>,
    pub line: u32,
    pub column: u32,
    pub presentation_hint: Option<String>,
}

impl StackFrame {
    pub fn path(&self) -> Option<&str> {
        self.source.as_ref()?.path.as_deref()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Scope {
    pub name: String,
    pub presentation_hint: Option<String>,
    pub variables_reference: i64,
    pub named_variables: Option<i64>,
    pub expensive: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Variable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub variables_reference: i64,
    pub evaluate_name: Option<String>,
    pub named_variables: Option<i64>,
    pub indexed_variables: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct EvaluateResponse {
    pub result: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub variables_reference: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetVariableResponse {
    pub value: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub variables_reference: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StoppedEvent {
    pub reason: String,
    pub description: Option<String>,
    pub thread_id: Option<i64>,
    pub all_threads_stopped: bool,
    pub text: Option<String>,
    pub hit_breakpoint_ids: Vec<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OutputEvent {
    pub category: Option<String>,
    pub output: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BreakpointEvent {
    pub reason: String,
    pub breakpoint: Breakpoint,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThreadEvent {
    pub reason: String,
    pub thread_id: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProcessEvent {
    pub name: String,
    pub system_process_id: Option<u32>,
}

/// Something the adapter told us without being asked.
#[derive(Clone, Debug)]
pub enum Event {
    Initialized,
    Stopped(StoppedEvent),
    Continued { thread_id: Option<i64>, all_threads: bool },
    Exited { exit_code: i64 },
    Terminated,
    Output(OutputEvent),
    Breakpoint(BreakpointEvent),
    Thread(ThreadEvent),
    Process(ProcessEvent),
    /// Any other event, by name.
    Other { event: String, body: Value },
    /// The adapter process went away.
    AdapterExited,
}

impl Event {
    pub fn parse(event: &str, body: Value) -> Event {
        fn decode<T: for<'de> Deserialize<'de> + Default>(body: Value) -> T {
            serde_json::from_value(body).unwrap_or_default()
        }
        match event {
            "initialized" => Event::Initialized,
            "stopped" => Event::Stopped(decode(body)),
            "continued" => Event::Continued {
                thread_id: body.get("threadId").and_then(Value::as_i64),
                all_threads: body.get("allThreadsContinued").and_then(Value::as_bool).unwrap_or(true),
            },
            "exited" => Event::Exited {
                exit_code: body.get("exitCode").and_then(Value::as_i64).unwrap_or(0),
            },
            "terminated" => Event::Terminated,
            "output" => Event::Output(decode(body)),
            "breakpoint" => Event::Breakpoint(decode(body)),
            "thread" => Event::Thread(decode(body)),
            "process" => Event::Process(decode(body)),
            _ => Event::Other { event: event.to_string(), body },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_events() {
        let stopped = Event::parse(
            "stopped",
            json!({"reason": "breakpoint", "threadId": 7, "allThreadsStopped": true, "hitBreakpointIds": [2]}),
        );
        match stopped {
            Event::Stopped(s) => {
                assert_eq!(s.reason, "breakpoint");
                assert_eq!(s.thread_id, Some(7));
                assert_eq!(s.hit_breakpoint_ids, vec![2]);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(Event::parse("exited", json!({"exitCode": 3})), Event::Exited { exit_code: 3 }));
        assert!(matches!(Event::parse("module", json!({})), Event::Other { .. }));
    }

    #[test]
    fn serializes_breakpoints_without_empty_fields() {
        let bp = SourceBreakpoint { line: 4, condition: Some("x > 1".into()), ..Default::default() };
        assert_eq!(serde_json::to_value(bp).unwrap(), json!({"line": 4, "condition": "x > 1"}));
    }

    #[test]
    fn decodes_variables() {
        let v: Variable = serde_json::from_value(json!({
            "name": "nums", "value": "size=3", "type": "alloc::vec::Vec<int>", "variablesReference": 5
        }))
        .unwrap();
        assert_eq!(v.type_name.as_deref(), Some("alloc::vec::Vec<int>"));
        assert_eq!(v.variables_reference, 5);
    }
}
