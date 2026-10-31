//! A Debug Adapter Protocol client.
//!
//! One [`DapClient`] talks to one adapter process over stdio, framed like the
//! Language Server Protocol (`Content-Length` headers), so it shares the
//! editor's codec. Requests return futures usable from GPUI's executor;
//! events arrive on a channel. Requests the adapter makes of us (reverse
//! requests such as `runInTerminal`) are refused, since we do not advertise
//! them.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, OnceLock,
    },
};

use anyhow::{anyhow, Context as _, Result};
use cockpit_editor::lsp::codec;
use futures::channel::oneshot;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::protocol::*;

/// A failed request, with the adapter's message.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct RequestError {
    pub command: String,
    pub message: String,
}

type Pending = Arc<Mutex<Option<HashMap<i64, oneshot::Sender<Result<Value, RequestError>>>>>>;

pub struct DapClient {
    name: String,
    writer: Mutex<Box<dyn Write + Send>>,
    pending: Pending,
    seq: AtomicI64,
    capabilities: OnceLock<Capabilities>,
    child: Mutex<Option<Child>>,
}

/// How to start a debug adapter.
#[derive(Clone, Debug)]
pub struct AdapterCommand {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

impl DapClient {
    /// Starts the adapter with `cwd` as its working directory.
    pub fn spawn(adapter: &AdapterCommand, cwd: &Path) -> Result<(Arc<Self>, async_channel::Receiver<Event>)> {
        let mut child = Command::new(&adapter.program)
            .args(&adapter.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", adapter.program))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            let name = adapter.name.clone();
            std::thread::Builder::new()
                .name(format!("dap-{name}-stderr"))
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        tracing::debug!(target: "cockpit_debugger::adapter", "{name}: {line}");
                    }
                })
                .ok();
        }
        let (client, events) = Self::connect(adapter.name.clone(), stdout, stdin);
        *client.child.lock() = Some(child);
        Ok((client, events))
    }

    /// Wires a client to an already-running adapter's streams.
    pub fn connect(
        name: impl Into<String>,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> (Arc<Self>, async_channel::Receiver<Event>) {
        let (events_tx, events_rx) = async_channel::unbounded();
        let client = Arc::new(Self {
            name: name.into(),
            writer: Mutex::new(Box::new(writer)),
            pending: Arc::new(Mutex::new(Some(HashMap::new()))),
            seq: AtomicI64::new(1),
            capabilities: OnceLock::new(),
            child: Mutex::new(None),
        });
        let weak = Arc::downgrade(&client);
        let pending = client.pending.clone();
        std::thread::Builder::new()
            .name(format!("dap-{}", client.name))
            .spawn(move || {
                let mut reader = BufReader::new(reader);
                while let Ok(Some(message)) = codec::read_message(&mut reader) {
                    dispatch(message, &pending, &events_tx, &weak);
                }
                for (_, waiter) in pending.lock().take().into_iter().flatten() {
                    let _ = waiter.send(Err(RequestError {
                        command: String::new(),
                        message: "the debug adapter exited".into(),
                    }));
                }
                let _ = events_tx.send_blocking(Event::AdapterExited);
            })
            .expect("spawning the DAP reader thread");
        (client, events_rx)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities.get().cloned().unwrap_or_default()
    }

    fn send(&self, message: &Value) -> Result<()> {
        codec::write_message(&mut **self.writer.lock(), message).with_context(|| format!("writing to {}", self.name))
    }

    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Sends a request and resolves to its `body` (`null` when it has none).
    pub fn request(
        &self,
        command: &str,
        arguments: Value,
    ) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        let rx = self.request_raw(command, arguments);
        let command = command.to_string();
        async move {
            let rx = rx?;
            let body = rx
                .await
                .map_err(|_| anyhow!("{command}: cancelled"))?
                .map_err(|err| anyhow!("{command}: {}", err.message))?;
            Ok(body)
        }
    }

    /// Like [`Self::request`], decoding the body.
    pub fn request_as<T: DeserializeOwned + Send + 'static>(
        &self,
        command: &str,
        arguments: Value,
    ) -> impl std::future::Future<Output = Result<T>> + Send + 'static {
        let request = self.request(command, arguments);
        let command = command.to_string();
        async move {
            let body = request.await?;
            serde_json::from_value(body).with_context(|| format!("decoding the {command} response"))
        }
    }

    fn request_raw(&self, command: &str, arguments: Value) -> Result<oneshot::Receiver<Result<Value, RequestError>>> {
        let seq = self.next_seq();
        let (tx, rx) = oneshot::channel();
        match self.pending.lock().as_mut() {
            Some(map) => map.insert(seq, tx),
            None => return Err(anyhow!("{} has exited", self.name)),
        };
        let sent = self.send(&json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        }));
        if let Err(err) = sent {
            if let Some(map) = self.pending.lock().as_mut() {
                map.remove(&seq);
            }
            return Err(err);
        }
        Ok(rx)
    }

    // --- typed requests ----------------------------------------------------

    pub async fn initialize(&self) -> Result<Capabilities> {
        let capabilities: Capabilities = self
            .request_as(
                "initialize",
                json!({
                    "clientID": "hangar",
                    "clientName": "Hangar",
                    "adapterID": self.name,
                    "locale": "en-US",
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path",
                    "supportsVariableType": true,
                    "supportsVariablePaging": false,
                    "supportsRunInTerminalRequest": false,
                    "supportsProgressReporting": false,
                }),
            )
            .await?;
        let _ = self.capabilities.set(capabilities.clone());
        Ok(capabilities)
    }

    /// `launch`. Adapters may answer only after `configurationDone`, so the
    /// caller sets breakpoints once `initialized` arrives and awaits this
    /// afterwards.
    pub fn launch(&self, arguments: Value) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("launch", arguments)
    }

    pub fn set_breakpoints(
        &self,
        path: &Path,
        breakpoints: &[SourceBreakpoint],
    ) -> impl std::future::Future<Output = Result<Vec<Breakpoint>>> + Send + 'static {
        let request = self.request(
            "setBreakpoints",
            json!({
                "source": { "path": path.to_string_lossy(), "name": path.file_name().map(|n| n.to_string_lossy().into_owned()) },
                "breakpoints": breakpoints,
                "lines": breakpoints.iter().map(|b| b.line).collect::<Vec<_>>(),
            }),
        );
        async move { decode_field(request.await?, "breakpoints") }
    }

    pub fn set_function_breakpoints(
        &self,
        breakpoints: &[FunctionBreakpoint],
    ) -> impl std::future::Future<Output = Result<Vec<Breakpoint>>> + Send + 'static {
        let request = self.request("setFunctionBreakpoints", json!({ "breakpoints": breakpoints }));
        async move { decode_field(request.await?, "breakpoints") }
    }

    pub fn set_exception_breakpoints(&self, filters: &[String]) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("setExceptionBreakpoints", json!({ "filters": filters }))
    }

    pub fn configuration_done(&self) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("configurationDone", json!({}))
    }

    pub fn threads(&self) -> impl std::future::Future<Output = Result<Vec<Thread>>> + Send + 'static {
        let request = self.request("threads", json!({}));
        async move { decode_field(request.await?, "threads") }
    }

    pub fn stack_trace(&self, thread_id: i64, levels: u32) -> impl std::future::Future<Output = Result<Vec<StackFrame>>> + Send + 'static {
        let request = self.request("stackTrace", json!({ "threadId": thread_id, "startFrame": 0, "levels": levels }));
        async move { decode_field(request.await?, "stackFrames") }
    }

    pub fn scopes(&self, frame_id: i64) -> impl std::future::Future<Output = Result<Vec<Scope>>> + Send + 'static {
        let request = self.request("scopes", json!({ "frameId": frame_id }));
        async move { decode_field(request.await?, "scopes") }
    }

    pub fn variables(&self, reference: i64) -> impl std::future::Future<Output = Result<Vec<Variable>>> + Send + 'static {
        let request = self.request("variables", json!({ "variablesReference": reference }));
        async move { decode_field(request.await?, "variables") }
    }

    /// `context` is `"watch"`, `"hover"`, `"repl"` or `"variables"`.
    pub fn evaluate(
        &self,
        expression: &str,
        frame_id: Option<i64>,
        context: &str,
    ) -> impl std::future::Future<Output = Result<EvaluateResponse>> + Send + 'static {
        let mut args = json!({ "expression": expression, "context": context });
        if let Some(frame_id) = frame_id {
            args["frameId"] = json!(frame_id);
        }
        self.request_as("evaluate", args)
    }

    pub fn set_variable(
        &self,
        reference: i64,
        name: &str,
        value: &str,
    ) -> impl std::future::Future<Output = Result<SetVariableResponse>> + Send + 'static {
        self.request_as(
            "setVariable",
            json!({ "variablesReference": reference, "name": name, "value": value }),
        )
    }

    pub fn continue_(&self, thread_id: i64) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("continue", json!({ "threadId": thread_id }))
    }

    pub fn next(&self, thread_id: i64) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("next", json!({ "threadId": thread_id }))
    }

    pub fn step_in(&self, thread_id: i64) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("stepIn", json!({ "threadId": thread_id }))
    }

    pub fn step_out(&self, thread_id: i64) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("stepOut", json!({ "threadId": thread_id }))
    }

    pub fn pause(&self, thread_id: i64) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("pause", json!({ "threadId": thread_id }))
    }

    pub fn disconnect(&self, terminate_debuggee: bool) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request("disconnect", json!({ "terminateDebuggee": terminate_debuggee, "restart": false }))
    }

    /// Kills the adapter process (and with it, a debuggee it launched).
    pub fn kill(&self) {
        if let Some(mut child) = self.child.lock().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        self.kill();
    }
}

fn decode_field<T: DeserializeOwned + Default>(body: Value, field: &str) -> Result<T> {
    match body.get(field) {
        Some(value) => serde_json::from_value(value.clone()).with_context(|| format!("decoding `{field}`")),
        None => Ok(T::default()),
    }
}

fn dispatch(
    message: Value,
    pending: &Pending,
    events: &async_channel::Sender<Event>,
    client: &std::sync::Weak<DapClient>,
) {
    match message.get("type").and_then(Value::as_str) {
        Some("response") => {
            let Some(seq) = message.get("request_seq").and_then(Value::as_i64) else { return };
            let waiter = pending.lock().as_mut().and_then(|map| map.remove(&seq));
            let Some(waiter) = waiter else { return };
            let success = message.get("success").and_then(Value::as_bool).unwrap_or(false);
            let result = if success {
                Ok(message.get("body").cloned().unwrap_or(Value::Null))
            } else {
                let command = message.get("command").and_then(Value::as_str).unwrap_or_default().to_string();
                let message = message
                    .pointer("/body/error/format")
                    .and_then(Value::as_str)
                    .or_else(|| message.get("message").and_then(Value::as_str))
                    .unwrap_or("request failed")
                    .to_string();
                Err(RequestError { command, message })
            };
            let _ = waiter.send(result);
        }
        Some("event") => {
            let name = message.get("event").and_then(Value::as_str).unwrap_or_default();
            let body = message.get("body").cloned().unwrap_or(Value::Null);
            let _ = events.send_blocking(Event::parse(name, body));
        }
        Some("request") => {
            // A reverse request we did not advertise support for.
            let Some(client) = client.upgrade() else { return };
            let reply = json!({
                "seq": client.next_seq(),
                "type": "response",
                "request_seq": message.get("seq").cloned().unwrap_or(Value::Null),
                "command": message.get("command").cloned().unwrap_or(Value::Null),
                "success": false,
                "message": "not supported by Hangar",
            });
            let _ = client.send(&reply);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn framed(values: &[Value]) -> Vec<u8> {
        let mut out = Vec::new();
        for v in values {
            codec::write_message(&mut out, v).unwrap();
        }
        out
    }

    #[test]
    fn events_arrive_and_exit_is_reported() {
        let input = framed(&[
            json!({"seq": 1, "type": "event", "event": "initialized"}),
            json!({"seq": 2, "type": "event", "event": "output", "body": {"category": "stdout", "output": "hi\n"}}),
        ]);
        let (_client, events) = DapClient::connect("test", Cursor::new(input), std::io::sink());
        assert!(matches!(events.recv_blocking().unwrap(), Event::Initialized));
        match events.recv_blocking().unwrap() {
            Event::Output(o) => assert_eq!(o.output, "hi\n"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(events.recv_blocking().unwrap(), Event::AdapterExited));
    }

    #[test]
    fn requests_fail_once_the_adapter_is_gone() {
        let (client, events) = DapClient::connect("test", Cursor::new(Vec::new()), std::io::sink());
        assert!(matches!(events.recv_blocking().unwrap(), Event::AdapterExited));
        let result = futures::executor::block_on(client.threads());
        assert!(result.is_err());
    }
}
