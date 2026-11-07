//! The GitHub view's state, shared by its sidebar and its main view: the
//! sign-in, the repository list, and what is open in the selected repository.
//!
//! All network work runs in the background; the UI only reads what has
//! arrived. Each kind of request has one slot, so a newer request replaces
//! an older one and a late answer never lands on the wrong page.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Result;
use cockpit_github::{
    api::{check_repo_name, StateFilter},
    auth::{self, DeviceCode},
    token, Client, Comment, Entry, EntryKind, FileContent, Issue, Job, PullFile, PullRequest,
    Repo, Run, User, Workflow,
};
use gpui::{AppContext as _, Context, Task};

/// Something fetched from GitHub.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Load<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

impl<T> From<Result<T>> for Load<T> {
    fn from(result: Result<T>) -> Self {
        match result {
            Ok(value) => Self::Ready(value),
            Err(err) => Self::Failed(format!("{err:#}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// Reading the Keychain and checking a saved token.
    Checking,
    SignedOut,
    /// The device flow: waiting for the user to enter the code on GitHub.
    Waiting(DeviceCode),
    /// A token is being checked.
    Verifying,
    SignedIn(User),
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum RepoTab {
    #[default]
    Code,
    Pulls,
    Issues,
    Actions,
}

impl RepoTab {
    pub const ALL: [RepoTab; 4] = [Self::Code, Self::Pulls, Self::Issues, Self::Actions];

    pub fn label(self) -> &'static str {
        match self {
            Self::Code => "Code",
            Self::Pulls => "Pull requests",
            Self::Issues => "Issues",
            Self::Actions => "Actions",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullDetail {
    pub pull: PullRequest,
    pub files: Vec<PullFile>,
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueDetail {
    pub issue: Issue,
    pub comments: Vec<Comment>,
}

/// What fills the tab instead of its list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Detail {
    #[default]
    None,
    Pull(u64, Load<PullDetail>),
    Issue(u64, Load<IssueDetail>),
    /// A run (kept from the list for its header) and its jobs.
    Run(Box<Run>, Load<Vec<Job>>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenFile {
    pub path: String,
    pub content: Load<FileContent>,
}

/// The selected repository and everything loaded for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoState {
    pub repo: Repo,
    pub tab: RepoTab,
    /// The branch the code tab shows.
    pub git_ref: String,
    pub branches: Load<Vec<String>>,
    /// The directory the code tab lists.
    pub path: String,
    pub listing: Load<Vec<Entry>>,
    /// The listed directory's README: (path, text).
    pub readme: Option<(String, Load<String>)>,
    pub file: Option<OpenFile>,
    pub pull_filter: StateFilter,
    pub pulls: Load<Vec<PullRequest>>,
    pub issue_filter: StateFilter,
    pub issues: Load<Vec<Issue>>,
    pub workflows: Load<Vec<Workflow>>,
    /// The workflow whose runs are listed; `None` lists every run.
    pub workflow: Option<u64>,
    pub runs: Load<Vec<Run>>,
    pub detail: Detail,
}

impl RepoState {
    fn new(repo: Repo) -> Self {
        Self {
            git_ref: repo.default_branch.clone(),
            repo,
            tab: RepoTab::Code,
            branches: Load::Idle,
            path: String::new(),
            listing: Load::Idle,
            readme: None,
            file: None,
            pull_filter: StateFilter::Open,
            pulls: Load::Idle,
            issue_filter: StateFilter::Open,
            issues: Load::Idle,
            workflows: Load::Idle,
            workflow: None,
            runs: Load::Idle,
            detail: Detail::None,
        }
    }

    pub fn full_name(&self) -> &str {
        &self.repo.full_name
    }
}

/// One in-flight request per slot.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Slot {
    Auth,
    Repos,
    Branches,
    Listing,
    Readme,
    File,
    Pulls,
    Issues,
    Workflows,
    Runs,
    Detail,
}

impl Slot {
    /// Requests about the selected repository, dropped when it changes.
    const REPO: [Slot; 9] = [
        Self::Branches,
        Self::Listing,
        Self::Readme,
        Self::File,
        Self::Pulls,
        Self::Issues,
        Self::Workflows,
        Self::Runs,
        Self::Detail,
    ];
}

pub struct GitHubModel {
    client: Option<Client>,
    pub auth: AuthState,
    /// The last sign-in or repository-list error.
    pub error: Option<String>,
    /// The GitHub CLI is installed, so its login can be used.
    pub has_gh_cli: bool,
    /// Browser sign-in is set up (an OAuth App client ID is known).
    pub has_client_id: bool,
    pub repos: Load<Vec<Repo>>,
    /// The repository list is being fetched again.
    pub refreshing_repos: bool,
    pub repo: Option<RepoState>,
    revision: u64,
    tasks: HashMap<Slot, Task<()>>,
    /// Stops a device-flow poll.
    cancel_sign_in: Arc<AtomicBool>,
}

impl GitHubModel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut model = Self {
            client: None,
            auth: AuthState::Checking,
            error: None,
            has_gh_cli: false,
            has_client_id: auth::client_id().is_some(),
            repos: Load::Idle,
            refreshing_repos: false,
            repo: None,
            revision: 0,
            tasks: HashMap::new(),
            cancel_sign_in: Arc::new(AtomicBool::new(false)),
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

    pub fn viewer(&self) -> Option<&User> {
        match &self.auth {
            AuthState::SignedIn(user) => Some(user),
            _ => None,
        }
    }

    /// Runs `job` with the client in the background and hands its result to
    /// `apply`, unless a newer request took the slot first.
    fn fetch<T: Send + 'static>(
        &mut self,
        slot: Slot,
        job: impl FnOnce(&Client) -> Result<T> + Send + 'static,
        apply: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let task = cx.background_spawn(async move { job(&client) });
        self.tasks.insert(
            slot,
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    apply(this, result, cx);
                    this.changed(cx);
                })
                .ok();
            }),
        );
    }

    /// Like [`Self::fetch`], for a request about the selected repository:
    /// `apply` gets that repository's state.
    fn fetch_repo<T: Send + 'static>(
        &mut self,
        slot: Slot,
        job: impl FnOnce(&Client, &str) -> Result<T> + Send + 'static,
        apply: impl FnOnce(&mut RepoState, Result<T>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.repo.as_ref().map(|r| r.repo.full_name.clone()) else {
            return;
        };
        let expected = name.clone();
        self.fetch(
            slot,
            move |client| job(client, &name),
            move |this, result, _| {
                if let Some(repo) = this.repo.as_mut().filter(|r| r.repo.full_name == expected) {
                    apply(repo, result);
                }
            },
            cx,
        );
    }

    /// Like [`Self::fetch_repo`], with the whole model in `apply`.
    fn fetch_repo_then<T: Send + 'static>(
        &mut self,
        slot: Slot,
        job: impl FnOnce(&Client, &str) -> Result<T> + Send + 'static,
        apply: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.repo.as_ref().map(|r| r.repo.full_name.clone()) else {
            return;
        };
        self.fetch(slot, move |client| job(client, &name), apply, cx);
    }

    // --- signing in --------------------------------------------------------

    /// Reads the saved token and checks it with GitHub.
    pub fn load(&mut self, cx: &mut Context<Self>) {
        self.auth = AuthState::Checking;
        self.error = None;
        self.changed(cx);
        let task = cx.background_spawn(async move {
            let has_gh_cli = auth::gh_cli().is_some();
            let signed_in = match token::read() {
                Ok(Some(token)) => {
                    let client = Client::new(token);
                    Some(client.viewer().map(|user| (client, user)))
                }
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            };
            (has_gh_cli, signed_in)
        });
        self.tasks.insert(
            Slot::Auth,
            cx.spawn(async move |this, cx| {
                let (has_gh_cli, signed_in) = task.await;
                this.update(cx, |this, cx| {
                    this.has_gh_cli = has_gh_cli;
                    match signed_in {
                        Some(Ok((client, user))) => this.signed_in(client, user, cx),
                        Some(Err(err)) => {
                            this.auth = AuthState::SignedOut;
                            this.error = Some(format!("{err:#}"));
                        }
                        None => this.auth = AuthState::SignedOut,
                    }
                    this.changed(cx);
                })
                .ok();
            }),
        );
    }

    fn signed_in(&mut self, client: Client, user: User, cx: &mut Context<Self>) {
        self.client = Some(client);
        self.auth = AuthState::SignedIn(user);
        self.error = None;
        self.refresh_repos(cx);
    }

    /// Checks a token, keeps it, and signs in with it.
    fn sign_in_with(
        &mut self,
        get_token: impl FnOnce() -> Result<String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        // Dropping the task below doesn't stop a job already running, so a
        // cancel or sign-out also has to keep it from saving the token.
        self.cancel_sign_in.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_sign_in = cancel.clone();
        self.auth = AuthState::Verifying;
        self.error = None;
        self.changed(cx);
        let task = cx.background_spawn(async move {
            let token = get_token()?;
            let client = Client::new(token.clone());
            let user = client.viewer()?;
            if !token::write_unless_cancelled(&token, &cancel)? {
                anyhow::bail!("Sign-in canceled");
            }
            Ok::<_, anyhow::Error>((client, user))
        });
        self.tasks.insert(
            Slot::Auth,
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok((client, user)) => this.signed_in(client, user, cx),
                        Err(err) => {
                            this.auth = AuthState::SignedOut;
                            this.error = Some(format!("{err:#}"));
                        }
                    }
                    this.changed(cx);
                })
                .ok();
            }),
        );
    }

    pub fn sign_in_with_token(&mut self, token: String, cx: &mut Context<Self>) {
        let token = token.trim().to_string();
        if token.is_empty() || self.auth == AuthState::Verifying {
            return;
        }
        self.sign_in_with(move || Ok(token), cx);
    }

    pub fn sign_in_with_gh_cli(&mut self, cx: &mut Context<Self>) {
        self.sign_in_with(auth::gh_cli_token, cx);
    }

    /// Starts the browser sign-in: gets a one-time code, copies it, opens
    /// GitHub's device page, and waits for the user to approve.
    pub fn sign_in_with_browser(&mut self, cx: &mut Context<Self>) {
        let Some(client_id) = auth::client_id() else {
            self.error = Some("Browser sign-in is not set up in this build.".into());
            self.changed(cx);
            return;
        };
        self.cancel_sign_in.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_sign_in = cancel.clone();
        self.auth = AuthState::Verifying;
        self.error = None;
        self.changed(cx);

        let start = {
            let client_id = client_id.clone();
            cx.background_spawn(async move { auth::start_device_flow(&client_id) })
        };
        self.tasks.insert(
            Slot::Auth,
            cx.spawn(async move |this, cx| {
                let code = match start.await {
                    Ok(code) => code,
                    Err(err) => {
                        this.update(cx, |this, cx| {
                            this.auth = AuthState::SignedOut;
                            this.error = Some(format!("{err:#}"));
                            this.changed(cx);
                        })
                        .ok();
                        return;
                    }
                };
                let shown = this.update(cx, |this, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.user_code.clone()));
                    cx.open_url(&code.verification_uri);
                    this.auth = AuthState::Waiting(code.clone());
                    this.changed(cx);
                });
                if shown.is_err() {
                    return;
                }
                // The poll can take minutes; it gets its own thread rather
                // than holding a background executor thread.
                let (tx, rx) = futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let result = auth::finish_device_flow(&client_id, &code, &cancel)
                        .and_then(|token| {
                            let client = Client::new(token.clone());
                            let user = client.viewer()?;
                            if !token::write_unless_cancelled(&token, &cancel)? {
                                anyhow::bail!("Sign-in canceled");
                            }
                            Ok((client, user))
                        });
                    tx.send(result).ok();
                });
                let Ok(result) = rx.await else {
                    return;
                };
                this.update(cx, |this, cx| {
                    // A cancel already put the view back.
                    if !matches!(this.auth, AuthState::Waiting(_)) {
                        return;
                    }
                    match result {
                        Ok((client, user)) => this.signed_in(client, user, cx),
                        Err(err) => {
                            this.auth = AuthState::SignedOut;
                            this.error = Some(format!("{err:#}"));
                        }
                    }
                    this.changed(cx);
                })
                .ok();
            }),
        );
    }

    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in.store(true, Ordering::Relaxed);
        self.tasks.remove(&Slot::Auth);
        self.auth = AuthState::SignedOut;
        self.changed(cx);
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in.store(true, Ordering::Relaxed);
        self.tasks.clear();
        self.client = None;
        self.auth = AuthState::SignedOut;
        self.error = None;
        self.repos = Load::Idle;
        self.refreshing_repos = false;
        self.repo = None;
        cx.background_spawn(async move {
            if let Err(err) = token::delete() {
                tracing::warn!("removing the GitHub token: {err:#}");
            }
        })
        .detach();
        self.changed(cx);
    }

    // --- repositories ------------------------------------------------------

    pub fn refresh_repos(&mut self, cx: &mut Context<Self>) {
        // Keep showing the old list while the new one loads.
        if self.repos.ready().is_none() {
            self.repos = Load::Loading;
        }
        self.refreshing_repos = true;
        self.fetch(
            Slot::Repos,
            Client::repos,
            |this, result, _| {
                this.refreshing_repos = false;
                if let Err(err) = &result {
                    this.error = Some(format!("{err:#}"));
                } else {
                    this.error = None;
                }
                if result.is_ok() || this.repos.ready().is_none() {
                    this.repos = result.into();
                }
            },
            cx,
        );
        self.changed(cx);
    }

    pub fn select_repo(&mut self, full_name: &str, cx: &mut Context<Self>) {
        if self.repo.as_ref().is_some_and(|r| r.repo.full_name == full_name) {
            return;
        }
        let Some(repo) = self
            .repos
            .ready()
            .and_then(|repos| repos.iter().find(|r| r.full_name == full_name))
            .cloned()
        else {
            return;
        };
        if let Err(err) = check_repo_name(&repo.full_name) {
            self.error = Some(format!("{err:#}"));
            self.changed(cx);
            return;
        }
        for slot in Slot::REPO {
            self.tasks.remove(&slot);
        }
        self.repo = Some(RepoState::new(repo));
        self.open_dir(String::new(), cx);
        self.load_branches(cx);
    }

    pub fn close_repo(&mut self, cx: &mut Context<Self>) {
        for slot in Slot::REPO {
            self.tasks.remove(&slot);
        }
        self.repo = None;
        self.changed(cx);
    }

    pub fn set_tab(&mut self, tab: RepoTab, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        if repo.tab != tab {
            repo.tab = tab;
            repo.detail = Detail::None;
            self.tasks.remove(&Slot::Detail);
        }
        let repo = self.repo.as_ref().expect("checked above");
        match tab {
            RepoTab::Code if repo.listing.is_idle() => self.open_dir(repo.path.clone(), cx),
            RepoTab::Pulls if repo.pulls.is_idle() => self.load_pulls(cx),
            RepoTab::Issues if repo.issues.is_idle() => self.load_issues(cx),
            RepoTab::Actions if repo.runs.is_idle() => self.load_actions(cx),
            _ => {}
        }
        self.changed(cx);
    }

    /// Reloads what the current tab shows.
    pub fn refresh_tab(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            self.refresh_repos(cx);
            return;
        };
        match (&repo.tab, &repo.detail) {
            (_, Detail::Pull(number, _)) => self.open_pull(*number, cx),
            (_, Detail::Issue(number, _)) => self.open_issue(*number, cx),
            (_, Detail::Run(run, _)) => {
                let run = (**run).clone();
                self.open_run(run, cx);
            }
            (RepoTab::Code, _) => {
                let (path, file) = (repo.path.clone(), repo.file.clone());
                self.open_dir(path, cx);
                if let Some(file) = file {
                    self.open_file_at(file.path, 0, cx);
                }
            }
            (RepoTab::Pulls, _) => self.load_pulls(cx),
            (RepoTab::Issues, _) => self.load_issues(cx),
            (RepoTab::Actions, _) => self.load_actions(cx),
        }
    }

    // --- code --------------------------------------------------------------

    fn load_branches(&mut self, cx: &mut Context<Self>) {
        if let Some(repo) = self.repo.as_mut() {
            repo.branches = Load::Loading;
        }
        self.fetch_repo(
            Slot::Branches,
            |client, repo| client.branches(repo),
            |repo, result| repo.branches = result.into(),
            cx,
        );
    }

    pub fn set_ref(&mut self, git_ref: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        if repo.git_ref == git_ref {
            return;
        }
        repo.git_ref = git_ref;
        let path = repo.path.clone();
        self.open_dir(path, cx);
    }

    /// Lists a directory (closing any open file) and loads its README.
    pub fn open_dir(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        repo.path = path.clone();
        repo.listing = Load::Loading;
        repo.readme = None;
        repo.file = None;
        let git_ref = repo.git_ref.clone();
        self.tasks.remove(&Slot::File);
        self.tasks.remove(&Slot::Readme);
        let expected = (repo.repo.full_name.clone(), path.clone());
        self.fetch_repo_then(
            Slot::Listing,
            move |client, repo| client.list_dir(repo, &path, &git_ref),
            move |this, result, cx| {
                let Some(repo) = this
                    .repo
                    .as_mut()
                    .filter(|r| (&r.repo.full_name, &r.path) == (&expected.0, &expected.1))
                else {
                    return;
                };
                repo.listing = result.into();
                this.load_readme(cx);
            },
            cx,
        );
        self.changed(cx);
    }

    /// Loads the listed directory's README, if it has one.
    fn load_readme(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        if repo.readme.is_some() {
            return;
        }
        let Some(entry) = repo.listing.ready().and_then(|entries| find_readme(entries)).cloned()
        else {
            return;
        };
        repo.readme = Some((entry.path.clone(), Load::Loading));
        let git_ref = repo.git_ref.clone();
        let path = entry.path.clone();
        self.fetch_repo(
            Slot::Readme,
            move |client, repo| client.file(repo, &entry.path, &git_ref, entry.size),
            move |repo, result| {
                let text = result.map(|content| match content {
                    FileContent::Text(text) => text,
                    _ => String::new(),
                });
                repo.readme = Some((path, text.into()));
            },
            cx,
        );
    }

    pub fn open_entry(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        match entry.kind {
            EntryKind::Dir => self.open_dir(entry.path.clone(), cx),
            EntryKind::File | EntryKind::Symlink => {
                self.open_file_at(entry.path.clone(), entry.size, cx)
            }
            EntryKind::Submodule => {}
        }
    }

    fn open_file_at(&mut self, path: String, size: u64, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        repo.file = Some(OpenFile {
            path: path.clone(),
            content: Load::Loading,
        });
        let git_ref = repo.git_ref.clone();
        let expected = path.clone();
        self.fetch_repo(
            Slot::File,
            move |client, repo| client.file(repo, &path, &git_ref, size),
            move |repo, result| {
                if let Some(file) = repo.file.as_mut().filter(|f| f.path == expected) {
                    file.content = result.into();
                }
            },
            cx,
        );
        self.changed(cx);
    }

    pub fn close_file(&mut self, cx: &mut Context<Self>) {
        if let Some(repo) = self.repo.as_mut() {
            repo.file = None;
            self.tasks.remove(&Slot::File);
            self.changed(cx);
        }
    }

    // --- pull requests and issues -------------------------------------------

    pub fn set_pull_filter(&mut self, filter: StateFilter, cx: &mut Context<Self>) {
        if let Some(repo) = self.repo.as_mut() {
            repo.pull_filter = filter;
            self.load_pulls(cx);
        }
    }

    fn load_pulls(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        repo.pulls = Load::Loading;
        let filter = repo.pull_filter;
        self.fetch_repo(
            Slot::Pulls,
            move |client, repo| client.pulls(repo, filter),
            |repo, result| repo.pulls = result.into(),
            cx,
        );
        self.changed(cx);
    }

    pub fn open_pull(&mut self, number: u64, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        repo.detail = Detail::Pull(number, Load::Loading);
        self.fetch_repo(
            Slot::Detail,
            move |client, repo| {
                Ok(PullDetail {
                    pull: client.pull(repo, number)?,
                    files: client.pull_files(repo, number)?,
                    comments: client.comments(repo, number)?,
                })
            },
            move |repo, result| {
                if matches!(repo.detail, Detail::Pull(n, _) if n == number) {
                    repo.detail = Detail::Pull(number, result.into());
                }
            },
            cx,
        );
        self.changed(cx);
    }

    pub fn set_issue_filter(&mut self, filter: StateFilter, cx: &mut Context<Self>) {
        if let Some(repo) = self.repo.as_mut() {
            repo.issue_filter = filter;
            self.load_issues(cx);
        }
    }

    fn load_issues(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        repo.issues = Load::Loading;
        let filter = repo.issue_filter;
        self.fetch_repo(
            Slot::Issues,
            move |client, repo| client.issues(repo, filter),
            |repo, result| repo.issues = result.into(),
            cx,
        );
        self.changed(cx);
    }

    pub fn open_issue(&mut self, number: u64, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        repo.detail = Detail::Issue(number, Load::Loading);
        self.fetch_repo(
            Slot::Detail,
            move |client, repo| {
                Ok(IssueDetail {
                    issue: client.issue(repo, number)?,
                    comments: client.comments(repo, number)?,
                })
            },
            move |repo, result| {
                if matches!(repo.detail, Detail::Issue(n, _) if n == number) {
                    repo.detail = Detail::Issue(number, result.into());
                }
            },
            cx,
        );
        self.changed(cx);
    }

    pub fn close_detail(&mut self, cx: &mut Context<Self>) {
        if let Some(repo) = self.repo.as_mut() {
            repo.detail = Detail::None;
            self.tasks.remove(&Slot::Detail);
            self.changed(cx);
        }
    }

    // --- Actions -------------------------------------------------------------

    fn load_actions(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        repo.workflows = Load::Loading;
        self.fetch_repo(
            Slot::Workflows,
            |client, repo| client.workflows(repo),
            |repo, result| repo.workflows = result.into(),
            cx,
        );
        self.load_runs(cx);
    }

    pub fn select_workflow(&mut self, workflow: Option<u64>, cx: &mut Context<Self>) {
        if let Some(repo) = self.repo.as_mut() {
            repo.workflow = workflow;
            repo.detail = Detail::None;
            self.load_runs(cx);
        }
    }

    fn load_runs(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        repo.runs = Load::Loading;
        let workflow = repo.workflow;
        self.fetch_repo(
            Slot::Runs,
            move |client, repo| client.runs(repo, workflow),
            |repo, result| repo.runs = result.into(),
            cx,
        );
        self.changed(cx);
    }

    pub fn open_run(&mut self, run: Run, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        let id = run.id;
        repo.detail = Detail::Run(Box::new(run), Load::Loading);
        self.fetch_repo(
            Slot::Detail,
            move |client, repo| client.jobs(repo, id),
            move |repo, result| {
                if let Detail::Run(run, jobs) = &mut repo.detail {
                    if run.id == id {
                        *jobs = result.into();
                    }
                }
            },
            cx,
        );
        self.changed(cx);
    }
}

/// The README GitHub would show for a directory.
fn find_readme(entries: &[Entry]) -> Option<&Entry> {
    const NAMES: [&str; 4] = ["readme.md", "readme.markdown", "readme", "readme.txt"];
    NAMES.iter().find_map(|name| {
        entries
            .iter()
            .find(|e| e.kind == EntryKind::File && e.name.eq_ignore_ascii_case(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Entry {
        Entry {
            name: name.into(),
            path: name.into(),
            kind: EntryKind::File,
            size: 1,
        }
    }

    #[test]
    fn readme_prefers_markdown() {
        let entries = [file("README"), file("Readme.md"), file("main.rs")];
        assert_eq!(find_readme(&entries).map(|e| e.name.as_str()), Some("Readme.md"));
        assert!(find_readme(&[file("main.rs")]).is_none());
    }
}
