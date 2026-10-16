//! Language servers for a workspace.
//!
//! One server per language family, started on first use. Editors attach
//! when they open a file; diagnostics are routed back to the editor showing
//! the file.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use anyhow::Result;
use cockpit_editor::{
    lsp::{server_for_language, server_key, uri_to_path, LspClient, ServerEvent},
    lsp_language_id,
};
use gpui::{App, AppContext as _, Context, EventEmitter, Task, WeakEntity, Window};
use gpui_component::input::{CompletionProvider, HoverProvider, InputState, RopeExt as _};
use gpui_component::Rope;
use lsp_types::{CompletionContext, CompletionResponse};

use crate::editor_view::EditorView;

pub enum LspEvent {
    /// New diagnostics for a file.
    Diagnostics(PathBuf),
    /// A server started or failed; the status bar shows it.
    StatusChanged,
}

enum Slot {
    Starting(Vec<WeakEntity<EditorView>>),
    Ready(Arc<LspClient>),
    Failed(String),
}

pub struct LspManager {
    root: PathBuf,
    servers: HashMap<String, Slot>,
    diagnostics: HashMap<PathBuf, Vec<lsp_types::Diagnostic>>,
}

impl EventEmitter<LspEvent> for LspManager {}

impl LspManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            servers: HashMap::new(),
            diagnostics: HashMap::new(),
        }
    }

    pub fn diagnostics(&self, path: &Path) -> &[lsp_types::Diagnostic] {
        self.diagnostics.get(path).map(Vec::as_slice).unwrap_or_default()
    }

    /// Counts of (errors, warnings) across the workspace.
    pub fn diagnostic_counts(&self) -> (usize, usize) {
        let mut errors = 0;
        let mut warnings = 0;
        for diagnostic in self.diagnostics.values().flatten() {
            match diagnostic.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => errors += 1,
                Some(lsp_types::DiagnosticSeverity::WARNING) => warnings += 1,
                _ => {}
            }
        }
        (errors, warnings)
    }

    /// One line for the status bar.
    pub fn status(&self) -> Option<String> {
        let mut parts: Vec<String> = self
            .servers
            .iter()
            .map(|(key, slot)| match slot {
                Slot::Starting(_) => format!("{key}: starting"),
                Slot::Ready(client) => client.name().to_string(),
                Slot::Failed(err) => format!("{key}: failed ({})", cockpit_common::truncate_chars(err, 60)),
            })
            .collect();
        parts.sort();
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    /// Attaches `editor` to the server for `language`, starting it if needed.
    /// The editor is called back with the client once it is ready.
    pub fn attach(&mut self, language: &str, editor: WeakEntity<EditorView>, cx: &mut Context<Self>) {
        let key = server_key(language).to_string();
        match self.servers.get_mut(&key) {
            Some(Slot::Ready(client)) => {
                let client = client.clone();
                cx.defer(move |cx| {
                    editor.update(cx, |editor, cx| editor.attach_lsp(client, cx)).ok();
                });
                return;
            }
            Some(Slot::Starting(waiting)) => {
                waiting.push(editor);
                return;
            }
            Some(Slot::Failed(_)) => return,
            None => {}
        }

        let search_dirs = cockpit_common::shell_env::common_tool_dirs();
        let Some(config) = server_for_language(language, &search_dirs) else {
            return;
        };
        self.servers.insert(key.clone(), Slot::Starting(vec![editor]));
        cx.emit(LspEvent::StatusChanged);

        let root = self.root.clone();
        let start = cx.background_spawn(async move { LspClient::start(&config, &root).await });
        cx.spawn(async move |this, cx| {
            let started = start.await;
            this.update(cx, |this, cx| {
                let waiting = match this.servers.remove(&key) {
                    Some(Slot::Starting(waiting)) => waiting,
                    _ => Vec::new(),
                };
                match started {
                    Ok((client, events)) => {
                        this.servers.insert(key.clone(), Slot::Ready(client.clone()));
                        for editor in waiting {
                            let client = client.clone();
                            editor.update(cx, |editor, cx| editor.attach_lsp(client, cx)).ok();
                        }
                        this.listen(events, cx);
                    }
                    Err(err) => {
                        tracing::warn!("language server for {key} failed to start: {err:#}");
                        this.servers.insert(key.clone(), Slot::Failed(format!("{err:#}")));
                    }
                }
                cx.emit(LspEvent::StatusChanged);
            })
        })
        .detach();
    }

    fn listen(&mut self, events: async_channel::Receiver<ServerEvent>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let keep_going = this.update(cx, |this, cx| match event {
                    ServerEvent::Diagnostics(params) => {
                        if let Some(path) = uri_to_path(&params.uri) {
                            this.diagnostics.insert(path.clone(), params.diagnostics);
                            cx.emit(LspEvent::Diagnostics(path));
                        }
                    }
                    ServerEvent::Message { kind, text } => {
                        if kind == lsp_types::MessageType::ERROR {
                            tracing::warn!("language server: {text}");
                        }
                    }
                    ServerEvent::Exited => {
                        this.servers.retain(|_, slot| !matches!(slot, Slot::Ready(client) if Arc::strong_count(client) == 1));
                        cx.emit(LspEvent::StatusChanged);
                    }
                });
                if keep_going.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn shutdown(&mut self, cx: &mut App) {
        for slot in self.servers.drain().map(|(_, slot)| slot) {
            if let Slot::Ready(client) = slot {
                cx.background_spawn(async move {
                    let _ = client.shutdown().await;
                })
                .detach();
            }
        }
    }
}

/// Completion through the language server.
pub struct LspCompletion {
    pub client: Arc<LspClient>,
    pub path: PathBuf,
    pub triggers: Vec<String>,
}

impl CompletionProvider for LspCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>> {
        let position = text.offset_to_position(offset);
        let request = self.client.completion(&self.path, position, Some(trigger));
        cx.background_spawn(async move {
            Ok(request.await?.unwrap_or(CompletionResponse::Array(Vec::new())))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut Context<InputState>) -> bool {
        let Some(last) = new_text.chars().last() else {
            return false;
        };
        last.is_alphanumeric() || last == '_' || self.triggers.iter().any(|t| t.ends_with(last))
    }
}

/// Hover documentation through the language server.
pub struct LspHover {
    pub client: Arc<LspClient>,
    pub path: PathBuf,
}

impl HoverProvider for LspHover {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>> {
        let position = text.offset_to_position(offset);
        let request = self.client.hover(&self.path, position);
        cx.background_spawn(request)
    }
}

/// Providers for an editor of `language`.
pub fn providers(
    client: &Arc<LspClient>,
    path: &Path,
) -> (Rc<dyn CompletionProvider>, Option<Rc<dyn HoverProvider>>) {
    let capabilities = client.capabilities();
    let triggers = capabilities
        .and_then(|c| c.completion_provider.as_ref())
        .and_then(|c| c.trigger_characters.clone())
        .unwrap_or_default();
    let hover = capabilities
        .and_then(|c| c.hover_provider.as_ref())
        .is_some()
        .then(|| {
            Rc::new(LspHover {
                client: client.clone(),
                path: path.to_path_buf(),
            }) as Rc<dyn HoverProvider>
        });
    (
        Rc::new(LspCompletion {
            client: client.clone(),
            path: path.to_path_buf(),
            triggers,
        }),
        hover,
    )
}

pub fn language_id(language: &str) -> &str {
    lsp_language_id(language)
}
