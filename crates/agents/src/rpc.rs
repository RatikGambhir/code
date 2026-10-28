//! The App Server's JSON-RPC dialect: newline-delimited JSON objects over
//! stdio, shaped like JSON-RPC 2.0 but without the `"jsonrpc"` member.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestId::Int(i) => write!(f, "{i}"),
            RequestId::Str(s) => f.write_str(s),
        }
    }
}

#[derive(Clone, Debug, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[error("{message} (code {code})")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INTERNAL_ERROR: i64 = -32603;

    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: Self::METHOD_NOT_FOUND,
            message: format!("{method} is not supported by this client"),
            data: None,
        }
    }
}

/// One decoded line from the server.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Response {
        id: RequestId,
        result: Result<Value, RpcError>,
    },
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

#[derive(Deserialize)]
struct Raw {
    id: Option<RequestId>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
    error: Option<RpcError>,
}

impl Message {
    pub fn parse(line: &str) -> Result<Self, serde_json::Error> {
        let raw: Raw = serde_json::from_str(line)?;
        Ok(match (raw.id, raw.method) {
            (Some(id), Some(method)) => Message::Request {
                id,
                method,
                params: raw.params,
            },
            (Some(id), None) => Message::Response {
                id,
                result: match raw.error {
                    Some(error) => Err(error),
                    None => Ok(raw.result.unwrap_or(Value::Null)),
                },
            },
            (None, Some(method)) => Message::Notification {
                method,
                params: raw.params,
            },
            (None, None) => {
                return Err(serde::de::Error::custom(
                    "message has neither an id nor a method",
                ))
            }
        })
    }
}

pub fn request(id: i64, method: &str, params: Value) -> Value {
    serde_json::json!({ "id": id, "method": method, "params": params })
}

pub fn notification(method: &str, params: Option<Value>) -> Value {
    match params {
        Some(params) => serde_json::json!({ "method": method, "params": params }),
        None => serde_json::json!({ "method": method }),
    }
}

pub fn response(id: &RequestId, result: Result<Value, RpcError>) -> Value {
    match result {
        Ok(result) => serde_json::json!({ "id": id, "result": result }),
        Err(error) => serde_json::json!({ "id": id, "error": error }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_each_message_kind() {
        let response = Message::parse(r#"{"id":1,"result":{"ok":true}}"#).unwrap();
        assert_eq!(response, Message::Response { id: RequestId::Int(1), result: Ok(json!({"ok": true})) });

        let error = Message::parse(r#"{"error":{"code":-32600,"message":"nope"},"id":5}"#).unwrap();
        let Message::Response { result: Err(err), .. } = error else { panic!() };
        assert_eq!(err.code, RpcError::INVALID_REQUEST);

        let request = Message::parse(
            r#"{"id":"abc","method":"item/commandExecution/requestApproval","params":{"x":1}}"#,
        )
        .unwrap();
        assert!(matches!(request, Message::Request { id: RequestId::Str(_), .. }));

        let notification =
            Message::parse(r#"{"method":"turn/started","params":{},"emittedAtMs":1}"#).unwrap();
        assert!(matches!(notification, Message::Notification { .. }));

        assert!(Message::parse(r#"{"params":{}}"#).is_err());
        assert!(Message::parse("not json").is_err());
    }

    #[test]
    fn outgoing_messages_have_no_jsonrpc_member() {
        let value = request(3, "initialize", json!({}));
        assert!(value.get("jsonrpc").is_none());
        assert_eq!(notification("initialized", None), json!({"method": "initialized"}));
        assert_eq!(
            response(&RequestId::Str("r".into()), Ok(json!({"decision": "accept"}))),
            json!({"id": "r", "result": {"decision": "accept"}})
        );
    }
}
