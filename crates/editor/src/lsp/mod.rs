//! A Language Server Protocol client.
//!
//! One [`LspClient`] talks to one server process over stdio. Requests return
//! futures (usable from GPUI's executor); diagnostics and other server
//! notifications arrive on a channel. Requests the server makes of the client
//! (`workspace/configuration`, progress tokens, capability registration) are
//! answered automatically, since many servers stall waiting for them.

pub mod codec;
mod registry;

use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, OnceLock,
    },
};

use anyhow::{anyhow, Context as _, Result};
use futures::channel::oneshot;
use lsp_types::{
    notification::{self, Notification},
    request::{self, Request},
    ClientCapabilities, InitializeParams, InitializeResult, ServerCapabilities, Uri,
};
use parking_lot::Mutex;
use serde_json::{json, Value};

pub use registry::{server_for_language, server_key, ServerConfig};

/// Something the server told us without being asked.
#[derive(Clone, Debug)]
pub enum ServerEvent {
    Diagnostics(lsp_types::PublishDiagnosticsParams),
    Message {
        kind: lsp_types::MessageType,
        text: String,
    },
    /// The server process went away.
    Exited,
}

#[derive(Debug, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, ResponseError>>>>>;

pub struct LspClient {
    name: String,
    writer: Mutex<Box<dyn Write + Send>>,
    pending: Pending,
    next_id: AtomicI64,
    capabilities: OnceLock<ServerCapabilities>,
    child: Mutex<Option<Child>>,
}

impl LspClient {
    /// Starts the server described by `config` with its working directory at
    /// `root`, and performs the `initialize` handshake.
    pub async fn start(
        config: &ServerConfig,
        root: &Path,
    ) -> Result<(Arc<Self>, async_channel::Receiver<ServerEvent>)> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", config.command))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let (client, events) = Self::connect(config.name.clone(), stdout, stdin);
        *client.child.lock() = Some(child);
        client.initialize(root).await?;
        Ok((client, events))
    }

    /// Wires a client to an already-running server's streams.
    pub fn connect(
        name: impl Into<String>,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> (Arc<Self>, async_channel::Receiver<ServerEvent>) {
        let (events_tx, events_rx) = async_channel::unbounded();
        let client = Arc::new(Self {
            name: name.into(),
            writer: Mutex::new(Box::new(writer)),
            pending: Arc::default(),
            next_id: AtomicI64::new(1),
            capabilities: OnceLock::new(),
            child: Mutex::new(None),
        });
        let weak = Arc::downgrade(&client);
        let pending = client.pending.clone();
        std::thread::Builder::new()
            .name(format!("lsp-{}", client.name))
            .spawn(move || {
                let mut reader = BufReader::new(reader);
                while let Ok(Some(message)) = codec::read_message(&mut reader) {
                    dispatch(message, &pending, &events_tx, &weak);
                }
                // Fail whatever was still waiting rather than hanging it.
                for (_, waiter) in pending.lock().drain() {
                    let _ = waiter.send(Err(ResponseError {
                        code: -32099,
                        message: "language server exited".into(),
                    }));
                }
                let _ = events_tx.send_blocking(ServerEvent::Exited);
            })
            .expect("spawning the LSP reader thread");
        (client, events_rx)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.get()
    }

    fn send(&self, message: &Value) -> Result<()> {
        codec::write_message(&mut **self.writer.lock(), message)
            .with_context(|| format!("writing to {}", self.name))
    }

    /// Sends a typed request and resolves to its typed result.
    pub fn request<R: Request>(
        &self,
        params: R::Params,
    ) -> impl std::future::Future<Output = Result<R::Result>> + Send + 'static
    where
        R::Result: Send + 'static,
    {
        let rx = self.request_raw(R::METHOD, serde_json::to_value(params));
        async move {
            let value = rx?.await.map_err(|_| anyhow!("request cancelled"))??;
            serde_json::from_value(value).context("decoding the server's response")
        }
    }

    fn request_raw(
        &self,
        method: &str,
        params: serde_json::Result<Value>,
    ) -> Result<oneshot::Receiver<Result<Value, ResponseError>>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        let sent = self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params?,
        }));
        if let Err(err) = sent {
            self.pending.lock().remove(&id);
            return Err(err);
        }
        Ok(rx)
    }

    pub fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": N::METHOD,
            "params": serde_json::to_value(params)?,
        }))
    }

    async fn initialize(&self, root: &Path) -> Result<InitializeResult> {
        let root_uri = path_to_uri(root)?;
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root_uri.clone()),
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: root_uri,
                name: root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            }]),
            capabilities: client_capabilities(),
            client_info: Some(lsp_types::ClientInfo {
                name: "Cockpit".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            ..Default::default()
        };
        let result = self.request::<request::Initialize>(params).await?;
        let _ = self.capabilities.set(result.capabilities.clone());
        self.notify::<notification::Initialized>(lsp_types::InitializedParams {})?;
        Ok(result)
    }

    pub fn did_open(&self, path: &Path, language_id: &str, version: i32, text: &str) -> Result<()> {
        self.notify::<notification::DidOpenTextDocument>(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: path_to_uri(path)?,
                language_id: language_id.into(),
                version,
                text: text.into(),
            },
        })
    }

    /// Sends the whole new text. Every server supports full sync, and for
    /// files a person edits the cost is negligible.
    pub fn did_change(&self, path: &Path, version: i32, text: &str) -> Result<()> {
        self.notify::<notification::DidChangeTextDocument>(lsp_types::DidChangeTextDocumentParams {
            text_document: lsp_types::VersionedTextDocumentIdentifier {
                uri: path_to_uri(path)?,
                version,
            },
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.into(),
            }],
        })
    }

    pub fn did_save(&self, path: &Path, text: &str) -> Result<()> {
        self.notify::<notification::DidSaveTextDocument>(lsp_types::DidSaveTextDocumentParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: path_to_uri(path)?,
            },
            text: Some(text.into()),
        })
    }

    pub fn did_close(&self, path: &Path) -> Result<()> {
        self.notify::<notification::DidCloseTextDocument>(lsp_types::DidCloseTextDocumentParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: path_to_uri(path)?,
            },
        })
    }

    pub fn completion(
        &self,
        path: &Path,
        position: lsp_types::Position,
        context: Option<lsp_types::CompletionContext>,
    ) -> impl std::future::Future<Output = Result<Option<lsp_types::CompletionResponse>>> + Send + 'static
    {
        let uri = path_to_uri(path);
        let request = uri.map(|uri| {
            self.request::<request::Completion>(lsp_types::CompletionParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context,
            })
        });
        async move { request?.await }
    }

    pub fn hover(
        &self,
        path: &Path,
        position: lsp_types::Position,
    ) -> impl std::future::Future<Output = Result<Option<lsp_types::Hover>>> + Send + 'static {
        let request = path_to_uri(path).map(|uri| {
            self.request::<request::HoverRequest>(lsp_types::HoverParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: Default::default(),
            })
        });
        async move { request?.await }
    }

    pub fn definition(
        &self,
        path: &Path,
        position: lsp_types::Position,
    ) -> impl std::future::Future<Output = Result<Option<lsp_types::GotoDefinitionResponse>>> + Send + 'static
    {
        let request = path_to_uri(path).map(|uri| {
            self.request::<request::GotoDefinition>(lsp_types::GotoDefinitionParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
        });
        async move { request?.await }
    }

    /// Asks the server to shut down and exit.
    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.request::<request::Shutdown>(()).await;
        let _ = self.notify::<notification::Exit>(());
        if let Some(mut child) = self.child.lock().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn dispatch(
    message: Value,
    pending: &Pending,
    events: &async_channel::Sender<ServerEvent>,
    client: &std::sync::Weak<LspClient>,
) {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).map(str::to_string);

    match (id, method) {
        // A response to one of our requests.
        (Some(id), None) => {
            let Some(id) = id.as_i64() else { return };
            let Some(waiter) = pending.lock().remove(&id) else {
                return;
            };
            let result = match message.get("error") {
                Some(error) => Err(ResponseError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                }),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = waiter.send(result);
        }
        // A request from the server.
        (Some(id), Some(method)) => {
            let Some(client) = client.upgrade() else { return };
            let response = match method.as_str() {
                "workspace/configuration" => {
                    let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                    json!({"jsonrpc": "2.0", "id": id, "result": vec![Value::Null; items]})
                }
                "window/workDoneProgress/create"
                | "client/registerCapability"
                | "client/unregisterCapability"
                | "window/showMessageRequest" => {
                    json!({"jsonrpc": "2.0", "id": id, "result": Value::Null})
                }
                "workspace/applyEdit" => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"applied": false}})
                }
                _ => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("{method} is not supported")},
                }),
            };
            let _ = client.send(&response);
        }
        // A notification.
        (None, Some(method)) => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let event = match method.as_str() {
                notification::PublishDiagnostics::METHOD => {
                    serde_json::from_value(params).ok().map(ServerEvent::Diagnostics)
                }
                notification::ShowMessage::METHOD | notification::LogMessage::METHOD => {
                    serde_json::from_value::<lsp_types::ShowMessageParams>(params)
                        .ok()
                        .map(|p| ServerEvent::Message {
                            kind: p.typ,
                            text: p.message,
                        })
                }
                _ => None,
            };
            if let Some(event) = event {
                let _ = events.send_blocking(event);
            }
        }
        (None, None) => {}
    }
}

fn client_capabilities() -> ClientCapabilities {
    serde_json::from_value(json!({
        "textDocument": {
            "synchronization": { "didSave": true, "dynamicRegistration": false },
            "completion": {
                "completionItem": {
                    "snippetSupport": false,
                    "documentationFormat": ["markdown", "plaintext"],
                    "insertReplaceSupport": true
                },
                "contextSupport": true
            },
            "hover": { "contentFormat": ["markdown", "plaintext"] },
            "definition": { "linkSupport": true },
            "publishDiagnostics": { "relatedInformation": false, "versionSupport": true }
        },
        "window": { "workDoneProgress": true },
        "workspace": { "configuration": true, "workspaceFolders": true },
        "general": { "positionEncodings": ["utf-16"] }
    }))
    .expect("static capabilities are valid")
}

/// `file://` URI for a filesystem path, percent-encoded as servers expect.
pub fn path_to_uri(path: &Path) -> Result<Uri> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let url = url::Url::from_file_path(&absolute)
        .map_err(|_| anyhow!("{} cannot be expressed as a file URI", absolute.display()))?;
    Uri::from_str(url.as_str()).map_err(|err| anyhow!("invalid URI {url}: {err}"))
}

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    url::Url::parse(uri.as_str()).ok()?.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake server on the other end of two pipes: answers `initialize`,
    /// asks the client for configuration, publishes diagnostics on didOpen.
    fn fake_server() -> (Arc<LspClient>, async_channel::Receiver<ServerEvent>, std::thread::JoinHandle<Vec<Value>>) {
        let (client_reader, mut server_writer) = std::io::pipe().unwrap();
        let (server_reader, client_writer) = std::io::pipe().unwrap();
        let handle = std::thread::spawn(move || {
            let mut reader = BufReader::new(server_reader);
            let mut seen = Vec::new();
            while let Ok(Some(message)) = codec::read_message(&mut reader) {
                seen.push(message.clone());
                match message.get("method").and_then(Value::as_str) {
                    Some("initialize") => {
                        // Ask the client something before answering, like real servers do.
                        codec::write_message(&mut server_writer, &json!({
                            "jsonrpc": "2.0", "id": "cfg-1", "method": "workspace/configuration",
                            "params": {"items": [{"section": "a"}, {"section": "b"}]}
                        })).unwrap();
                        codec::write_message(&mut server_writer, &json!({
                            "jsonrpc": "2.0", "id": message["id"],
                            "result": {"capabilities": {"hoverProvider": true}}
                        })).unwrap();
                    }
                    Some("textDocument/didOpen") => {
                        let uri = message["params"]["textDocument"]["uri"].clone();
                        codec::write_message(&mut server_writer, &json!({
                            "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                            "params": {"uri": uri, "diagnostics": [{
                                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}},
                                "severity": 1, "message": "broken"
                            }]}
                        })).unwrap();
                    }
                    Some("textDocument/hover") => {
                        codec::write_message(&mut server_writer, &json!({
                            "jsonrpc": "2.0", "id": message["id"],
                            "error": {"code": -32603, "message": "no hover today"}
                        })).unwrap();
                    }
                    Some("shutdown") => {
                        codec::write_message(&mut server_writer, &json!({
                            "jsonrpc": "2.0", "id": message["id"], "result": null
                        })).unwrap();
                    }
                    Some("exit") => break,
                    _ => {}
                }
            }
            seen
        });
        let (client, events) = LspClient::connect("fake", client_reader, client_writer);
        (client, events, handle)
    }

    #[test]
    fn handshake_diagnostics_errors_and_shutdown() {
        let (client, events, server) = fake_server();
        let dir = std::env::temp_dir();
        futures::executor::block_on(client.initialize(&dir)).unwrap();
        assert!(client.capabilities().unwrap().hover_provider.is_some());

        let file = dir.join("main.rs");
        client.did_open(&file, "rust", 1, "fn").unwrap();
        let event = events.recv_blocking().unwrap();
        let ServerEvent::Diagnostics(diagnostics) = event else {
            panic!("expected diagnostics, got {event:?}");
        };
        assert_eq!(uri_to_path(&diagnostics.uri).unwrap(), file);
        assert_eq!(diagnostics.diagnostics[0].message, "broken");

        let hover = futures::executor::block_on(client.hover(&file, lsp_types::Position::new(0, 0)));
        assert!(hover.unwrap_err().to_string().contains("no hover today"));

        futures::executor::block_on(client.shutdown()).unwrap();
        let seen = server.join().unwrap();

        // The configuration request was answered with one entry per item.
        let methods: Vec<_> = seen.iter().filter_map(|m| m["method"].as_str()).collect();
        assert_eq!(&methods[..2], &["initialize", "initialized"]);
        let config_reply = seen.iter().find(|m| m["id"] == "cfg-1").expect("config reply");
        assert_eq!(config_reply["result"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn pending_requests_fail_when_the_server_dies() {
        let (client_reader, server_writer) = std::io::pipe().unwrap();
        let (_server_reader, client_writer) = std::io::pipe().unwrap();
        let (client, _events) = LspClient::connect("dying", client_reader, client_writer);
        let request = client.hover(Path::new("/tmp/x.rs"), lsp_types::Position::new(0, 0));
        drop(server_writer);
        let result = futures::executor::block_on(request);
        assert!(result.unwrap_err().to_string().contains("exited"));
    }

    #[test]
    fn uris_are_percent_encoded() {
        let uri = path_to_uri(Path::new("/tmp/my project/a b.rs")).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/my%20project/a%20b.rs");
        assert_eq!(uri_to_path(&uri).unwrap(), PathBuf::from("/tmp/my project/a b.rs"));
    }
}
