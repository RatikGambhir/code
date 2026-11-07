//! The Tasks view's state, shared by its sidebar and its main list: the
//! workspace's local board, the Linear pull, and what is selected.
//!
//! All disk and network work runs in the background; the UI only reads
//! what has arrived. `revision` moves on every change so views can cache.

use std::path::PathBuf;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use cockpit_persistence::now_ts;
use cockpit_tasks::{
    key::{self, Credential},
    linear::{self, Viewer},
    oauth,
    local::{self, LocalBoard},
    Board, Issue, Priority, Project, Source, Status, StatusKind,
};
use gpui::{AppContext as _, Context, Task};

/// The list the main view shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// Issues assigned to the signed-in Linear user.
    MyIssues,
    Issues,
    Projects,
    Project(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub source: Source,
    pub view: View,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            source: Source::Local,
            view: View::Issues,
        }
    }
}

/// Linear's side: the key's state and the last pull.
#[derive(Default)]
pub struct LinearState {
    /// A key is stored (it may still turn out to be invalid).
    pub has_key: bool,
    /// Checked the Keychain once.
    pub key_checked: bool,
    pub connecting: bool,
    /// The browser sign-in is waiting for the user to approve in Linear.
    pub waiting_for_browser: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub viewer: Option<Viewer>,
    pub board: Board,
    /// When the last pull landed (seconds since the epoch).
    pub fetched_at: Option<i64>,
}

pub struct TasksModel {
    root: PathBuf,
    local_path: Option<PathBuf>,
    pub local: LocalBoard,
    pub local_loaded: bool,
    pub local_error: Option<String>,
    pub linear: LinearState,
    pub selection: Selection,
    /// The issue open in the detail pane, as (source, id).
    pub open_issue: Option<(Source, String)>,
    revision: u64,
    _load: Option<Task<()>>,
    _fetch: Option<Task<()>>,
    /// Stops a browser sign-in that is waiting.
    cancel_sign_in: Arc<AtomicBool>,
    /// A write is in flight.
    saving: bool,
    /// Edits not yet written.
    dirty: bool,
}

impl TasksModel {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let prefix = local::prefix_for(&root);
        let mut model = Self {
            root,
            local_path: None,
            local: LocalBoard::new(prefix),
            local_loaded: false,
            local_error: None,
            linear: LinearState::default(),
            selection: Selection::default(),
            open_issue: None,
            revision: 0,
            _load: None,
            _fetch: None,
            cancel_sign_in: Arc::new(AtomicBool::new(false)),
            saving: false,
            dirty: false,
        };
        model.load(cx);
        model
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.revision = self.revision.wrapping_add(1);
        cx.notify();
    }

    pub fn workspace_name(&self) -> String {
        self.root
            .file_name()
            .map_or_else(|| "Workspace".into(), |n| n.to_string_lossy().into_owned())
    }

    /// Reads the local board and the Keychain, then pulls Linear if a key is there.
    fn load(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let task = cx.background_spawn(async move {
            let local = cockpit_tasks::data_dir()
                .map(|dir| local::path_for(&dir, &root))
                .and_then(|path| {
                    let board = local::load(&path, &root)?;
                    Ok((path, board))
                });
            let has_key = key::read().map(|k| k.is_some());
            (local, has_key)
        });
        self._load = Some(cx.spawn(async move |this, cx| {
            let (local, has_key) = task.await;
            this.update(cx, |this, cx| {
                match local {
                    Ok((path, board)) => {
                        this.local_path = Some(path);
                        this.local = board;
                    }
                    Err(err) => this.local_error = Some(format!("{err:#}")),
                }
                this.local_loaded = true;
                this.linear.key_checked = true;
                match has_key {
                    Ok(has_key) => this.linear.has_key = has_key,
                    Err(err) => this.linear.error = Some(format!("{err:#}")),
                }
                if this.linear.has_key {
                    this.refresh_linear(cx);
                }
                this.changed(cx);
            })
            .ok();
        }));
    }

    // --- selection ---------------------------------------------------------

    pub fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if self.selection != selection {
            self.selection = selection;
            self.changed(cx);
        }
    }

    pub fn open_issue(&mut self, issue: Option<(Source, String)>, cx: &mut Context<Self>) {
        if self.open_issue != issue {
            self.open_issue = issue;
            self.changed(cx);
        }
    }

    pub fn board(&self, source: Source) -> &Board {
        match source {
            Source::Local => &self.local.board,
            Source::Linear => &self.linear.board,
        }
    }

    pub fn project(&self, source: Source, id: &str) -> Option<&Project> {
        self.board(source).projects.iter().find(|p| p.id == id)
    }

    pub fn issue(&self, source: Source, id: &str) -> Option<&Issue> {
        self.board(source).issues.iter().find(|i| i.id == id)
    }

    // --- Linear ------------------------------------------------------------

    /// Checks the key with Linear before keeping it in the Keychain.
    pub fn connect_linear(&mut self, api_key: String, cx: &mut Context<Self>) {
        let api_key = api_key.trim().to_string();
        if api_key.is_empty() || self.linear.connecting {
            return;
        }
        self.linear.connecting = true;
        self.linear.error = None;
        self.changed(cx);
        let task = cx.background_spawn(async move {
            let credential = Credential::ApiKey(api_key);
            linear::verify(&credential.header())?;
            key::write_credential(&credential)?;
            linear::fetch(&credential.header())
        });
        self.finish_connect(task, cx);
    }

    /// Browser sign-in is set up (an OAuth app client ID is known).
    pub fn has_linear_browser_sign_in(&self) -> bool {
        oauth::client_id().is_some()
    }

    /// Signs in to Linear in the browser: opens Linear's consent page and
    /// waits for it to call back to localhost.
    pub fn connect_linear_in_browser(&mut self, cx: &mut Context<Self>) {
        let Some(client_id) = oauth::client_id() else {
            return;
        };
        if self.linear.connecting {
            return;
        }
        self.cancel_sign_in.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_sign_in = cancel.clone();
        let pending = match oauth::begin_sign_in(&client_id) {
            Ok(pending) => pending,
            Err(err) => {
                self.linear.error = Some(format!("{err:#}"));
                self.changed(cx);
                return;
            }
        };
        cx.open_url(pending.auth_url());
        self.linear.connecting = true;
        self.linear.waiting_for_browser = true;
        self.linear.error = None;
        self.changed(cx);
        // The wait can take minutes; it gets its own thread rather than
        // holding a background executor thread.
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let result = pending.finish(&cancel).and_then(|tokens| {
                let credential = Credential::OAuth(tokens);
                linear::verify(&credential.header())?;
                key::write_credential(&credential)?;
                linear::fetch(&credential.header())
            });
            tx.send(result).ok();
        });
        let task = cx.background_spawn(async move {
            rx.await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Sign-in canceled")))
        });
        self.finish_connect(task, cx);
    }

    pub fn cancel_linear_sign_in(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in.store(true, Ordering::Relaxed);
        self._fetch = None;
        self.linear.connecting = false;
        self.linear.waiting_for_browser = false;
        self.changed(cx);
    }

    /// Lands a connect attempt: the first pull, or the error.
    fn finish_connect(&mut self, task: Task<anyhow::Result<linear::LinearBoard>>, cx: &mut Context<Self>) {
        self._fetch = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.linear.connecting = false;
                this.linear.waiting_for_browser = false;
                match result {
                    Ok(pulled) => {
                        this.linear.has_key = true;
                        this.apply_pull(pulled);
                        this.selection = Selection {
                            source: Source::Linear,
                            view: View::MyIssues,
                        };
                    }
                    Err(err) => this.linear.error = Some(format!("{err:#}")),
                }
                this.changed(cx);
            })
            .ok();
        }));
    }

    pub fn refresh_linear(&mut self, cx: &mut Context<Self>) {
        if !self.linear.has_key || self.linear.loading {
            return;
        }
        self.linear.loading = true;
        self.changed(cx);
        let task = cx.background_spawn(async move {
            let credential = key::usable_credential()?
                .ok_or_else(|| anyhow::anyhow!("Linear is not connected"))?;
            linear::fetch(&credential.header())
        });
        self._fetch = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.linear.loading = false;
                match result {
                    Ok(pulled) => {
                        this.linear.error = None;
                        this.apply_pull(pulled);
                    }
                    Err(err) => this.linear.error = Some(format!("{err:#}")),
                }
                this.changed(cx);
            })
            .ok();
        }));
    }

    fn apply_pull(&mut self, pulled: linear::LinearBoard) {
        self.linear.viewer = Some(pulled.viewer);
        self.linear.board = pulled.board;
        self.linear.fetched_at = Some(now_ts());
        // Drop a selection that no longer exists.
        if let View::Project(id) = &self.selection.view {
            if self.selection.source == Source::Linear
                && !self.linear.board.projects.iter().any(|p| &p.id == id)
            {
                self.selection.view = View::Projects;
            }
        }
    }

    pub fn disconnect_linear(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in.store(true, Ordering::Relaxed);
        self._fetch = None;
        self.linear = LinearState {
            key_checked: true,
            ..LinearState::default()
        };
        if self.selection.source == Source::Linear {
            self.selection = Selection::default();
        }
        if matches!(&self.open_issue, Some((Source::Linear, _))) {
            self.open_issue = None;
        }
        cx.background_spawn(async move {
            if let Ok(Some(Credential::OAuth(tokens))) = key::read_credential() {
                oauth::revoke(&tokens);
            }
            if let Err(err) = key::delete() {
                tracing::warn!("removing the Linear key: {err:#}");
            }
        })
        .detach();
        self.changed(cx);
    }

    // --- local edits -------------------------------------------------------

    pub fn add_issue(
        &mut self,
        title: &str,
        status: StatusKind,
        project_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if title.trim().is_empty() {
            return None;
        }
        let id = self
            .local
            .add_issue(title, status, project_id, now_ts())
            .id
            .clone();
        self.local.refresh_progress();
        self.save(cx);
        Some(id)
    }

    pub fn add_project(&mut self, name: &str, cx: &mut Context<Self>) -> Option<String> {
        if name.trim().is_empty() {
            return None;
        }
        const COLORS: [&str; 6] = [
            "#5e6ad2", "#26b5ce", "#4cb782", "#f2c94c", "#f2994a", "#eb5757",
        ];
        let color = COLORS[self.local.board.projects.len() % COLORS.len()].to_string();
        let id = self.local.add_project(name, Some(color)).id.clone();
        self.save(cx);
        Some(id)
    }

    pub fn set_status(&mut self, id: &str, kind: StatusKind, cx: &mut Context<Self>) {
        if self
            .local
            .edit_issue(id, now_ts(), |i| i.status = Status::local(kind))
        {
            self.local.refresh_progress();
            self.save(cx);
        }
    }

    pub fn set_priority(&mut self, id: &str, priority: Priority, cx: &mut Context<Self>) {
        if self
            .local
            .edit_issue(id, now_ts(), |i| i.priority = priority)
        {
            self.save(cx);
        }
    }

    pub fn set_project(&mut self, id: &str, project_id: Option<String>, cx: &mut Context<Self>) {
        if self
            .local
            .edit_issue(id, now_ts(), |i| i.project_id = project_id)
        {
            self.local.refresh_progress();
            self.save(cx);
        }
    }

    pub fn delete_issue(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.local.remove_issue(id) {
            if self
                .open_issue
                .as_ref()
                .is_some_and(|(s, open)| *s == Source::Local && open == id)
            {
                self.open_issue = None;
            }
            self.local.refresh_progress();
            self.save(cx);
        }
    }

    /// Writes the board in the background, one write at a time: edits made
    /// while a write is in flight are saved right after it.
    fn save(&mut self, cx: &mut Context<Self>) {
        self.changed(cx);
        self.dirty = true;
        if !self.saving {
            self.flush(cx);
        }
    }

    fn flush(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.local_path.clone() else {
            return;
        };
        self.dirty = false;
        self.saving = true;
        let board = self.local.clone();
        let task = cx.background_spawn(async move { local::save(&path, &board) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.saving = false;
                if let Err(err) = result {
                    this.local_error = Some(format!("{err:#}"));
                    this.changed(cx);
                }
                if this.dirty {
                    this.flush(cx);
                }
            })
            .ok();
        })
        .detach();
    }
}
