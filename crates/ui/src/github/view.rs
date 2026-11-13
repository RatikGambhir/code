//! The GitHub view: signing in, then the selected repository's code, pull
//! requests, issues and Actions, laid out the way github.com lays them out.

use std::collections::HashSet;

use cockpit_github::{
    api::StateFilter, auth, write::RunAction, Comment, Entry, EntryKind, FileContent, Issue, Job,
    PullRequest, PullStatus, Repo, Run, Workflow,
};
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight,
    ScrollHandle, SharedString, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    text::TextView,
    v_flex, Icon, IconName, Sizable as _,
};

use super::{
    edit::{Edit, Target},
    model::{AuthState, Detail, GitHubModel, IssueDetail, Load, PullDetail, RepoState, RepoTab},
    widgets,
};
use crate::{
    assets::icon,
    chat::{diff::shown_lines, render_diff_line},
    chrome,
    editor_view::EditorView,
    theme::{tokens, Tokens, RADIUS, RADIUS_LG, TEXT_BODY, TEXT_SMALL, TEXT_TITLE},
};

/// Diff lines drawn per file before "open on GitHub".
const MAX_DIFF_LINES: usize = 400;
/// Pull request files shown expanded until the user says otherwise.
const EXPANDED_FILES: usize = 12;
/// Widest a list or conversation gets, as on github.com.
const READING_WIDTH: f32 = 1012.;

pub struct GitHubView {
    pub(super) model: Entity<GitHubModel>,
    token_input: Entity<InputState>,
    /// The open file, in a read-only editor: (repo@ref:path, editor).
    file_view: Option<(String, Entity<EditorView>)>,
    /// Pull request files whose expanded state the user flipped.
    toggled_files: HashSet<String>,
    scroll: ScrollHandle,
    side_scroll: ScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl GitHubView {
    pub fn new(model: Entity<GitHubModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("ghp_… or github_pat_…")
        });
        let subscriptions = vec![
            cx.observe_in(&model, window, |this, _, window, cx| {
                this.sync_file_view(window, cx);
                cx.notify();
            }),
            cx.subscribe(&token_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.sign_in_with_token(cx);
                }
            }),
        ];
        Self {
            model,
            token_input,
            file_view: None,
            toggled_files: HashSet::new(),
            scroll: ScrollHandle::new(),
            side_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn sign_in_with_token(&mut self, cx: &mut Context<Self>) {
        let token = self.token_input.read(cx).value().to_string();
        self.model.update(cx, |m, cx| m.sign_in_with_token(token, cx));
    }

    /// Keeps the read-only editor in step with the open file.
    fn sync_file_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let open = model.repo.as_ref().and_then(|repo| {
            let file = repo.file.as_ref()?;
            let FileContent::Text(text) = file.content.ready()? else {
                return None;
            };
            Some((format!("{}@{}:{}", repo.full_name(), repo.git_ref, file.path), file.path.clone(), text.clone()))
        });
        match open {
            None => self.file_view = None,
            Some((key, _, _)) if self.file_view.as_ref().is_some_and(|(k, _)| *k == key) => {}
            Some((key, path, text)) => {
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                let (title, extension) = match name.rsplit_once('.') {
                    Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), ext.to_string()),
                    _ => (name.clone(), "txt".to_string()),
                };
                let editor = cx.new(|cx| EditorView::read_only(&title, &extension, text, window, cx));
                self.file_view = Some((key, editor));
            }
        }
    }

    fn markdown(id: impl Into<SharedString>, text: &str, window: &mut Window, cx: &mut App) -> AnyElement {
        let id: SharedString = id.into();
        div()
            .text_size(TEXT_BODY)
            .child(TextView::markdown(id, text.to_string(), window, cx).selectable(true))
            .into_any_element()
    }

    // --- signing in ----------------------------------------------------------

    fn render_sign_in(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let model = self.model.read(cx);
        let (auth, error, has_gh, has_client_id) =
            (model.auth.clone(), model.error.clone(), model.has_gh_cli, model.has_client_id);
        if auth == AuthState::Checking {
            return centered(Spinner::new().into_any_element());
        }
        let verifying = auth == AuthState::Verifying;

        let card = v_flex()
            .w(px(440.))
            .p(px(24.))
            .gap(px(14.))
            .rounded(RADIUS_LG)
            .border_1()
            .border_color(t.border)
            .bg(t.surface)
            .child(
                h_flex()
                    .gap(px(10.))
                    .child(Icon::new(IconName::GitHub).size(px(22.)).text_color(t.fg))
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Sign in to GitHub")),
            );

        if let AuthState::Waiting(code) = &auth {
            let uri = code.verification_uri.clone();
            let user_code = code.user_code.clone();
            return centered(
                card.child(div().text_size(TEXT_BODY).text_color(t.fg_muted).child(
                    "Enter this code on GitHub to finish signing in. It is copied to your clipboard and the page is open in your browser.",
                ))
                .child(
                    h_flex()
                        .justify_center()
                        .py(px(14.))
                        .rounded(RADIUS)
                        .bg(t.raised)
                        .border_1()
                        .border_color(t.border)
                        .font_family(t.mono.clone())
                        .text_size(px(26.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.fg)
                        .child(user_code.clone()),
                )
                .child(
                    h_flex()
                        .gap(px(8.))
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_muted)
                        .child(Spinner::new().xsmall())
                        .child("Waiting for you to approve on GitHub…"),
                )
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            Button::new("gh-open-device")
                                .label("Open GitHub")
                                .icon(icon("external-link"))
                                .primary()
                                .small()
                                .on_click(move |_, _, cx| cx.open_url(&uri)),
                        )
                        .child(
                            Button::new("gh-copy-code")
                                .label("Copy code")
                                .icon(IconName::Copy)
                                .ghost()
                                .small()
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(user_code.clone()))
                                }),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("gh-cancel-sign-in")
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.model.update(cx, |m, cx| m.cancel_sign_in(cx))
                                })),
                        ),
                )
                .into_any_element(),
            );
        }

        let card = card
            .child(div().text_size(TEXT_BODY).text_color(t.fg_muted).child(
                "Browse your repositories, code, pull requests, issues and Actions runs. \
                 The token is kept in your macOS Keychain and only sent to api.github.com.",
            ))
            .when(has_client_id, |el| {
                el.child(
                    Button::new("gh-sign-in-browser")
                        .label("Sign in with GitHub")
                        .icon(IconName::GitHub)
                        .primary()
                        .loading(verifying)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |m, cx| m.sign_in_with_browser(cx))
                        })),
                )
            })
            .when(has_gh, |el| {
                let button = Button::new("gh-sign-in-cli")
                    .label("Use my GitHub CLI login")
                    .icon(icon("terminal"))
                    .loading(verifying)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model.update(cx, |m, cx| m.sign_in_with_gh_cli(cx))
                    }));
                el.child(if has_client_id { button.ghost() } else { button.primary() })
            })
            .child(
                h_flex()
                    .gap(px(8.))
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_subtle)
                    .child(div().flex_1().h(px(1.)).bg(t.border))
                    .child(if has_client_id || has_gh {
                        "or use a personal access token"
                    } else {
                        "Sign in with a personal access token"
                    })
                    .child(div().flex_1().h(px(1.)).bg(t.border)),
            )
            .child(Input::new(&self.token_input).small())
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("gh-sign-in-token")
                            .label("Sign in")
                            .small()
                            .loading(verifying)
                            .when(!has_client_id && !has_gh, |b| b.primary())
                            .on_click(cx.listener(|this, _, _, cx| this.sign_in_with_token(cx))),
                    )
                    .child(
                        Button::new("gh-new-token")
                            .label("Create a token on GitHub")
                            .icon(icon("external-link"))
                            .ghost()
                            .small()
                            .on_click(|_, _, cx| cx.open_url(auth::NEW_TOKEN_URL)),
                    ),
            )
            .when_some(error, |el, error| {
                el.child(div().text_size(TEXT_SMALL).text_color(t.danger).child(error))
            });
        centered(card.into_any_element())
    }

    // --- home ----------------------------------------------------------------

    fn render_home(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let model = self.model.read(cx);
        let login = model.viewer().map(|u| u.login.clone()).unwrap_or_default();
        let repos: Option<Vec<Repo>> = model.repos.ready().map(|r| r.iter().take(12).cloned().collect());
        let failed = match &model.repos {
            Load::Failed(err) => Some(err.clone()),
            _ => None,
        };
        let Some(repos) = repos else {
            return centered(match failed {
                Some(err) => div().text_size(TEXT_BODY).text_color(t.danger).child(err).into_any_element(),
                None => loading_line("Loading repositories…", &t),
            });
        };
        let cards = repos.into_iter().map(|repo| {
            let name = repo.full_name.clone();
            div()
                .id(SharedString::from(format!("gh-home-{}", repo.full_name)))
                .w(px(300.))
                .p(px(14.))
                .rounded(RADIUS_LG)
                .border_1()
                .border_color(t.border)
                .bg(t.raised)
                .cursor_pointer()
                .hover(|s| s.border_color(t.border_strong))
                .child(repo_card_body(&repo, &t))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let name = name.clone();
                    this.model.update(cx, |m, cx| m.select_repo(&name, cx));
                }))
        });
        v_flex()
            .id("gh-home")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p(px(28.))
            .gap(px(16.))
            .child(
                h_flex()
                    .gap(px(10.))
                    .child(Icon::new(IconName::GitHub).size(px(20.)))
                    .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child(format!("Welcome, {login}"))),
            )
            .child(div().text_size(TEXT_BODY).text_color(t.fg_muted).child(
                "Recently pushed repositories. Pick one here or from the list on the left.",
            ))
            .child(h_flex().flex_wrap().gap(px(12.)).children(cards))
            .into_any_element()
    }

    // --- repository ----------------------------------------------------------

    fn render_repo_header(&self, repo: &RepoState, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let r = &repo.repo;
        let url = r.html_url.clone();
        let loading = match repo.tab {
            RepoTab::Code => repo.listing.is_loading() || repo.file.as_ref().is_some_and(|f| f.content.is_loading()),
            RepoTab::Pulls => repo.pulls.is_loading(),
            RepoTab::Issues => repo.issues.is_loading(),
            RepoTab::Actions => repo.runs.is_loading(),
        } || matches!(
            &repo.detail,
            Detail::Pull(_, Load::Loading) | Detail::Issue(_, Load::Loading) | Detail::Run(_, Load::Loading)
        );
        h_flex()
            .h(px(48.))
            .flex_none()
            .px(px(16.))
            .gap(px(8.))
            .child(icon(if r.private { "lock" } else { "book-marked" }).size(px(15.)).text_color(t.fg_muted))
            .child(
                h_flex()
                    .min_w_0()
                    .gap(px(4.))
                    .text_size(TEXT_TITLE)
                    .child(div().text_color(t.fg_muted).child(r.owner.clone()))
                    .child(div().text_color(t.fg_subtle).child("/"))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(t.fg).truncate().child(r.name.clone())),
            )
            .child(widgets::chip(if r.private { "Private" } else { "Public" }, &t))
            .when(r.archived, |el| el.child(widgets::chip("Archived", &t)))
            .when(r.fork, |el| el.child(widgets::chip("Fork", &t)))
            .child(div().flex_1())
            .when_some(r.language.clone(), |el, lang| {
                el.child(div().text_size(TEXT_SMALL).text_color(t.fg_muted).child(lang))
            })
            .child(stat(Icon::new(IconName::Star), widgets::compact(r.stargazers_count), &t))
            .child(stat(icon("git-fork"), widgets::compact(r.forks_count), &t))
            .child(
                Button::new("gh-refresh-tab")
                    .icon(icon("refresh-cw"))
                    .ghost()
                    .small()
                    .loading(loading)
                    .tooltip("Reload")
                    .on_click(cx.listener(|this, _, _, cx| this.model.update(cx, |m, cx| m.refresh_tab(cx)))),
            )
            .child(
                Button::new("gh-open-repo")
                    .icon(icon("external-link"))
                    .ghost()
                    .small()
                    .tooltip("Open on GitHub")
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
            .into_any_element()
    }

    fn render_tabs(&self, repo: &RepoState, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let counts = [
            None,
            repo.pulls.ready().map(Vec::len),
            repo.issues.ready().map(Vec::len),
            None,
        ];
        let tab_icon = |tab: RepoTab| match tab {
            RepoTab::Code => icon("code-xml"),
            RepoTab::Pulls => icon("git-pull-request"),
            RepoTab::Issues => icon("circle-dot"),
            RepoTab::Actions => icon("play"),
        };
        h_flex()
            .h(px(36.))
            .flex_none()
            .px(px(8.))
            .border_b_1()
            .border_color(t.border)
            .children(RepoTab::ALL.into_iter().zip(counts).map(|(tab, count)| {
                let active = repo.tab == tab;
                // Counts are only meaningful for open items.
                let show_count = match tab {
                    RepoTab::Pulls => repo.pull_filter == StateFilter::Open,
                    RepoTab::Issues => repo.issue_filter == StateFilter::Open,
                    RepoTab::Code | RepoTab::Actions => false,
                };
                chrome::underline_tab(("gh-tab", tab as usize), active, cx)
                    .text_size(TEXT_BODY)
                    .child(tab_icon(tab).size(px(14.)))
                    .child(tab.label())
                    .when_some(count.filter(|_| show_count), |el, n| el.child(widgets::count(n, &t)))
                    .on_click(cx.listener(move |this, _, _, cx| this.model.update(cx, |m, cx| m.set_tab(tab, cx))))
            }))
            .into_any_element()
    }

    // --- code ----------------------------------------------------------------

    fn render_code(&self, repo: &RepoState, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let branches = repo.branches.ready().cloned().unwrap_or_default();
        let current = repo.git_ref.clone();
        let model = self.model.clone();
        let branch_button = Button::new("gh-branch")
            .icon(icon("git-branch"))
            .label(current.clone())
            .small()
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.scrollable(true).max_h(px(360.));
                if branches.is_empty() {
                    menu = menu.item(PopupMenuItem::new("Loading branches…").disabled(true));
                }
                for branch in &branches {
                    let (model, branch) = (model.clone(), branch.clone());
                    menu = menu.item(
                        PopupMenuItem::new(branch.clone())
                            .checked(branch == current)
                            .on_click(move |_, _, cx| {
                                let branch = branch.clone();
                                model.update(cx, |m, cx| m.set_ref(branch, cx));
                            }),
                    );
                }
                menu
            });

        // Breadcrumbs: the repository, then each folder, then the open file.
        let shown_path = repo.file.as_ref().map_or(repo.path.clone(), |f| f.path.clone());
        let mut crumbs = vec![crumb("gh-crumb-root".into(), repo.repo.name.clone(), String::new(), true, &t, cx)];
        let segments: Vec<&str> = shown_path.split('/').filter(|s| !s.is_empty()).collect();
        for (ix, segment) in segments.iter().enumerate() {
            let last = ix + 1 == segments.len();
            let target = segments[..=ix].join("/");
            crumbs.push(div().text_color(t.fg_subtle).child("/").into_any_element());
            let is_file = last && repo.file.is_some();
            crumbs.push(crumb(
                SharedString::from(format!("gh-crumb-{ix}")),
                segment.to_string(),
                target,
                !is_file,
                &t,
                cx,
            ));
        }
        let toolbar = h_flex()
            .flex_none()
            .h(px(44.))
            .px(px(16.))
            .gap(px(10.))
            .child(branch_button)
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .flex_wrap()
                    .gap(px(4.))
                    .text_size(TEXT_BODY)
                    .children(crumbs),
            )
            .when(!repo.repo.archived && !repo.git_ref.is_empty(), |el| {
                let mut actions = vec![(
                    "New branch",
                    Edit::Branch {
                        from: repo.git_ref.clone(),
                    },
                )];
                if let Some(file) = &repo.file {
                    if file.content.ready().is_some() {
                        actions.push((
                            "Delete file",
                            Edit::DeleteFile {
                                branch: repo.git_ref.clone(),
                                path: file.path.clone(),
                            },
                        ));
                    }
                } else {
                    let path = if repo.path.is_empty() {
                        String::new()
                    } else {
                        format!("{}/", repo.path)
                    };
                    actions.push((
                        "New file",
                        Edit::File {
                            branch: repo.git_ref.clone(),
                            path,
                            new: true,
                        },
                    ));
                }
                el.child(self.edit_menu("gh-code-actions", "Code actions", actions, cx))
            });

        let body = match &repo.file {
            Some(file) => self.render_file(repo, &file.path, &file.content, cx),
            None => self.render_listing(repo, window, cx),
        };
        v_flex()
            .size_full()
            .child(toolbar)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_listing(&self, repo: &RepoState, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let entries = match &repo.listing {
            Load::Ready(entries) => entries.clone(),
            Load::Failed(err) => return centered(error_text(err, &t)),
            _ => return centered(loading_line("Loading files…", &t)),
        };
        let parent = (!repo.path.is_empty()).then(|| {
            repo.path
                .rsplit_once('/')
                .map_or(String::new(), |(parent, _)| parent.to_string())
        });
        let mut rows: Vec<AnyElement> = Vec::with_capacity(entries.len() + 1);
        if let Some(parent) = parent {
            rows.push(
                file_row("gh-entry-parent".into(), &t)
                    .child(Icon::new(IconName::Folder).size(px(15.)).text_color(t.accent_text))
                    .child(div().text_size(TEXT_BODY).text_color(t.fg_muted).child(".."))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let parent = parent.clone();
                        this.model.update(cx, |m, cx| m.open_dir(parent, cx));
                    }))
                    .into_any_element(),
            );
        }
        for entry in entries {
            rows.push(self.render_entry(entry, &t, cx));
        }
        let readme = repo.readme.clone();
        let readme_card = readme.and_then(|(path, text)| match text {
            Load::Ready(text) if !text.trim().is_empty() => Some(
                v_flex()
                    .mt(px(16.))
                    .rounded(RADIUS_LG)
                    .border_1()
                    .border_color(t.border)
                    .child(
                        h_flex()
                            .h(px(36.))
                            .px(px(14.))
                            .gap(px(6.))
                            .border_b_1()
                            .border_color(t.border)
                            .text_size(TEXT_SMALL)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(Icon::new(IconName::BookOpen).size(px(13.)))
                            .child(path.rsplit('/').next().unwrap_or(&path).to_string()),
                    )
                    .child(div().p(px(20.)).child(Self::markdown(
                        format!("gh-readme-{}-{}", repo.full_name(), path),
                        &text,
                        window,
                        cx,
                    )))
                    .into_any_element(),
            ),
            Load::Loading => Some(div().mt(px(16.)).child(loading_line("Loading README…", &t)).into_any_element()),
            _ => None,
        });
        scroll_area(
            "gh-listing",
            &self.scroll,
            v_flex()
                .w_full()
                .px(px(16.))
                .pb(px(24.))
                .child(
                    v_flex()
                        .rounded(RADIUS_LG)
                        .border_1()
                        .border_color(t.border)
                        .overflow_hidden()
                        .children(rows),
                )
                .children(readme_card)
                .into_any_element(),
        )
    }

    fn render_entry(&self, entry: Entry, t: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let (mark, color) = match entry.kind {
            EntryKind::Dir => (Icon::new(IconName::Folder), t.accent_text),
            EntryKind::Submodule => (icon("git-commit-horizontal"), t.fg_muted),
            EntryKind::File | EntryKind::Symlink => (Icon::new(IconName::File), t.fg_subtle),
        };
        let size = (entry.kind == EntryKind::File).then(|| widgets::size(entry.size));
        file_row(SharedString::from(format!("gh-entry-{}", entry.path)), t)
            .child(mark.size(px(15.)).text_color(color))
            .child(div().flex_1().min_w_0().truncate().text_size(TEXT_BODY).text_color(t.fg).child(entry.name.clone()))
            .when_some(size, |el, size| {
                el.child(div().flex_none().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(size))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                let entry = entry.clone();
                this.model.update(cx, |m, cx| m.open_entry(&entry, cx));
            }))
            .into_any_element()
    }

    fn render_file(&self, repo: &RepoState, path: &str, content: &Load<FileContent>, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let url = format!("{}/blob/{}/{}", repo.repo.html_url, repo.git_ref, path);
        let (detail, body): (Option<String>, AnyElement) = match content {
            Load::Ready(FileContent::Text(text)) => (
                Some(format!("{} lines · {}", text.lines().count(), widgets::size(text.len() as u64))),
                match &self.file_view {
                    Some((_, editor)) => editor.clone().into_any_element(),
                    None => loading_line("Opening…", &t),
                },
            ),
            Load::Ready(FileContent::Binary { size }) => (
                Some(widgets::size(*size)),
                centered(muted_text("This is a binary file. Open it on GitHub to view it.", &t)),
            ),
            Load::Ready(FileContent::TooLarge { size }) => (
                Some(widgets::size(*size)),
                centered(muted_text("This file is too large to show here. Open it on GitHub to view it.", &t)),
            ),
            Load::Failed(err) => (None, centered(error_text(err, &t))),
            _ => (None, centered(loading_line("Loading file…", &t))),
        };
        v_flex()
            .size_full()
            .px(px(16.))
            .pb(px(12.))
            .child(
                v_flex()
                    .size_full()
                    .rounded(RADIUS_LG)
                    .border_1()
                    .border_color(t.border)
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .h(px(38.))
                            .flex_none()
                            .px(px(12.))
                            .gap(px(8.))
                            .border_b_1()
                            .border_color(t.border)
                            .bg(t.raised)
                            .child(
                                Icon::new(IconName::File)
                                    .size(px(14.))
                                    .text_color(t.fg_subtle),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(TEXT_BODY)
                                    .child(path.to_string()),
                            )
                            .when_some(detail, |el, d| {
                                el.child(
                                    div().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(d),
                                )
                            })
                            .when(
                                matches!(content, Load::Ready(FileContent::Text(_)))
                                    && !repo.repo.archived
                                    && !repo.git_ref.is_empty(),
                                |el| {
                                    el.child(self.edit_button(
                                        "gh-file-edit",
                                        "Edit file",
                                        Edit::File {
                                            branch: repo.git_ref.clone(),
                                            path: path.to_string(),
                                            new: false,
                                        },
                                        false,
                                        cx,
                                    ))
                                },
                            )
                            .child(
                                Button::new("gh-file-open")
                                    .icon(icon("external-link"))
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Open on GitHub")
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                            .child(
                                Button::new("gh-file-close")
                                    .icon(icon("x"))
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Back to the folder")
                                    .on_click(cx.listener(|this, _, _, cx| this.model.update(cx, |m, cx| m.close_file(cx)))),
                            ),
                    )
                    .child(div().flex_1().min_h_0().bg(t.surface).child(body)),
            )
            .into_any_element()
    }

    // --- pull requests and issues ---------------------------------------------

    fn render_filter_bar(
        &self,
        repo: &RepoState,
        current: StateFilter,
        pulls: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = tokens(cx).clone();
        h_flex()
            .flex_none()
            .h(px(44.))
            .gap(px(4.))
            .children(StateFilter::ALL.map(|filter| {
                let active = current == filter;
                div()
                    .id(SharedString::from(format!("gh-filter-{}-{}", pulls, filter.as_query())))
                    .h(px(26.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(RADIUS)
                    .border_1()
                    .text_size(TEXT_SMALL)
                    .cursor_pointer()
                    .when(active, |el| el.border_color(t.border_strong).bg(t.raised).text_color(t.fg))
                    .when(!active, |el| {
                        el.border_color(gpui::transparent_black())
                            .text_color(t.fg_muted)
                            .hover(|s| s.bg(t.hover))
                    })
                    .child(filter.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model.update(cx, |m, cx| {
                            if pulls {
                                m.set_pull_filter(filter, cx)
                            } else {
                                m.set_issue_filter(filter, cx)
                            }
                        })
                    }))
            }))
            .child(div().flex_1())
            .when(!repo.repo.archived, |el| {
                let (label, action) = if pulls {
                    (
                        "New pull request",
                        Edit::NewPull {
                            head: if repo.git_ref == repo.repo.default_branch {
                                String::new()
                            } else {
                                repo.git_ref.clone()
                            },
                            base: repo.repo.default_branch.clone(),
                        },
                    )
                } else {
                    ("New issue", Edit::NewIssue)
                };
                el.child(self.edit_button("gh-new-item", label, action, true, cx))
            })
            .into_any_element()
    }

    fn render_pulls(&self, repo: &RepoState, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let list = match &repo.pulls {
            Load::Ready(pulls) if pulls.is_empty() => empty_state(icon("git-pull-request"), "No pull requests", "Nothing matches this filter.", &t),
            Load::Ready(pulls) => list_box(pulls.iter().map(|pull| self.render_pull_row(pull, &t, cx)), &t),
            Load::Failed(err) => error_text(err, &t),
            _ => loading_line("Loading pull requests…", &t),
        };
        scroll_area(
            "gh-pulls",
            &self.scroll,
            reading_column()
                .child(self.render_filter_bar(repo, repo.pull_filter, true, cx))
                .child(list)
                .into_any_element(),
        )
    }

    fn render_pull_row(&self, pull: &PullRequest, t: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let number = pull.number;
        let status = pull.status();
        list_row(SharedString::from(format!("gh-pull-{number}")), t)
            .child(div().pt(px(2.)).child(widgets::pull_icon(status, px(15.), t)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .child(div().text_size(TEXT_BODY).font_weight(FontWeight::MEDIUM).text_color(t.fg).child(pull.title.clone()))
                            .children(pull.labels.iter().map(|l| widgets::label(&l.name, l.color.as_deref(), t))),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_subtle)
                            .child(format!(
                                "#{number} · {} by {} · updated {}",
                                status.label().to_lowercase(),
                                pull.user,
                                widgets::ago(pull.updated_at)
                            ))
                            .child(widgets::branch(&pull.head.name, t)),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggled_files.clear();
                this.model.update(cx, |m, cx| m.open_pull(number, cx));
            }))
            .into_any_element()
    }

    fn render_pull_detail(&self, repo: &RepoState, number: u64, detail: &Load<PullDetail>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let back = back_button("Pull requests", cx);
        let detail = match detail {
            Load::Ready(detail) => detail,
            Load::Failed(err) => return detail_shell(back, error_text(err, &t), &self.scroll),
            _ => return detail_shell(back, loading_line(&format!("Loading #{number}…"), &t), &self.scroll),
        };
        let pull = &detail.pull;
        let status = pull.status();
        let url = pull.html_url.clone();
        let header = v_flex()
            .gap(px(8.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(div().flex_1().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(t.fg).child(format!("{} #{}", pull.title, pull.number)))
                    .child(open_on_github("gh-pull-open", url)),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .text_size(TEXT_BODY)
                    .text_color(t.fg_muted)
                    .child(widgets::pill(status.label(), widgets::pull_color(status, &t), &t).child(widgets::pull_icon(status, px(12.), &t)))
                    .child(format!(
                        "{} wants to merge {} into",
                        pull.user,
                        pull.commits.map_or_else(|| "commits".to_string(), |n| format!("{n} commit{}", if n == 1 { "" } else { "s" }))
                    ))
                    .child(widgets::branch(&pull.base.name, &t))
                    .child("from")
                    .child(widgets::branch(&pull.head.name, &t)),
            )
            .child(
                h_flex()
                    .gap(px(12.))
                    .text_size(TEXT_SMALL)
                    .child(div().text_color(t.fg_muted).child(format!("{} files changed", detail.files.len())))
                    .child(div().text_color(t.success).child(format!("+{}", pull.additions.unwrap_or(0))))
                    .child(div().text_color(t.danger).child(format!("−{}", pull.deletions.unwrap_or(0))))
                    .when_some(pull.mergeable_state.clone().filter(|_| status == cockpit_github::PullStatus::Open), |el, state| {
                        el.child(div().text_color(t.fg_subtle).child(format!("merge state: {state}")))
                    }),
            );
        let body = conversation_item(
            &pull.user,
            pull.created_at,
            pull.body.as_deref(),
            format!("gh-pr-body-{}-{}", repo.full_name(), number),
            window,
            cx,
        );
        let target = Target::Pull(Some(number));
        let comments = self.render_comments(&detail.comments, repo, number, target, window, cx);
        let files = self.render_pull_files(repo, number, detail, cx);
        let actions = (!repo.repo.archived).then(|| self.pull_actions(pull, target, cx));
        let reviews = match &detail.reviews {
            Load::Ready(reviews) => reviews
                .iter()
                .map(|review| {
                    v_flex()
                        .gap(px(6.))
                        .child(muted_text(
                            &format!(
                                "{} · {}",
                                review.user,
                                match review.state.as_str() {
                                    "APPROVED" => "Approved",
                                    "CHANGES_REQUESTED" => "Changes requested",
                                    "DISMISSED" => "Dismissed",
                                    "PENDING" => "Pending",
                                    _ => "Reviewed",
                                }
                            ),
                            &t,
                        ))
                        .when_some(
                            review
                                .body
                                .as_deref()
                                .filter(|body| !body.trim().is_empty()),
                            |el, body| {
                                el.child(Self::markdown(
                                    format!("gh-review-{}-{}", repo.full_name(), review.id),
                                    body,
                                    window,
                                    cx,
                                ))
                            },
                        )
                        .into_any_element()
                })
                .collect::<Vec<_>>(),
            Load::Failed(err) => vec![error_text(err, &t)],
            _ => vec![],
        };
        detail_shell(
            back,
            v_flex()
                .gap(px(18.))
                .child(header)
                .children(actions)
                .child(div().h(px(1.)).bg(t.border))
                .child(body)
                .children(comments)
                .when(!repo.repo.archived, |el| {
                    el.child(self.edit_button(
                        "gh-pull-comment",
                        "Add comment",
                        Edit::Comment {
                            target,
                            number,
                            comment: None,
                        },
                        false,
                        cx,
                    ))
                })
                .when(!reviews.is_empty(), |el| {
                    el.child(section_title("Reviews", &t)).children(reviews)
                })
                .child(section_title(
                    &format!("Files changed · {}", detail.files.len()),
                    &t,
                ))
                .child(files)
                .into_any_element(),
            &self.scroll,
        )
    }

    fn pull_actions(
        &self,
        pull: &PullRequest,
        target: Target,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let number = pull.number;
        let status = pull.status();
        let mut actions = vec![
            ("Edit title and description", Edit::Pull(pull.clone())),
            (
                "Manage assignees, labels and milestone",
                Edit::Metadata {
                    target,
                    number,
                    assignees: pull.assignees.clone(),
                    labels: pull.labels.iter().map(|l| l.name.clone()).collect(),
                    milestone: pull.milestone.clone(),
                },
            ),
        ];
        if matches!(status, PullStatus::Open | PullStatus::Draft) {
            actions.extend([
                (
                    "Close pull request",
                    Edit::PullState {
                        number,
                        close: true,
                    },
                ),
                (
                    "Request review",
                    Edit::Reviewers {
                        number,
                        remove: false,
                    },
                ),
            ]);
            if !pull.requested_reviewers.is_empty() || !pull.requested_teams.is_empty() {
                actions.push((
                    "Remove review requests",
                    Edit::Reviewers {
                        number,
                        remove: true,
                    },
                ));
            }
            if !pull.node_id.is_empty() {
                actions.push((
                    if pull.draft {
                        "Mark ready for review"
                    } else {
                        "Convert to draft"
                    },
                    Edit::PullDraft {
                        number,
                        node: pull.node_id.clone(),
                        draft: !pull.draft,
                    },
                ));
            }
        }
        if status == PullStatus::Closed {
            actions.push((
                "Reopen pull request",
                Edit::PullState {
                    number,
                    close: false,
                },
            ));
        }
        let author = self
            .model
            .read(cx)
            .viewer()
            .is_some_and(|viewer| viewer.login.eq_ignore_ascii_case(&pull.user));
        h_flex()
            .flex_wrap()
            .gap(px(8.))
            .child(self.edit_menu("gh-pr-actions", "Pull request actions", actions, cx))
            .when(
                matches!(status, PullStatus::Open | PullStatus::Draft),
                |el| {
                    el.child(self.edit_button(
                        "gh-pr-review",
                        "Review",
                        Edit::Review {
                            number,
                            sha: pull.head.sha.clone(),
                            author,
                        },
                        false,
                        cx,
                    ))
                },
            )
            .when(status == PullStatus::Open, |el| {
                el.child(self.edit_button(
                    "gh-pr-merge",
                    "Merge",
                    Edit::Merge {
                        number,
                        sha: pull.head.sha.clone(),
                        base: pull.base.name.clone(),
                    },
                    true,
                    cx,
                ))
            })
            .into_any_element()
    }

    fn render_pull_files(&self, repo: &RepoState, number: u64, detail: &PullDetail, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let base_url = format!("{}/pull/{number}/files", repo.repo.html_url);
        let files = detail.files.iter().enumerate().map(|(ix, file)| {
            let flipped = self.toggled_files.contains(&file.filename);
            let expanded = (ix < EXPANDED_FILES) != flipped;
            let name = file.filename.clone();
            let header = h_flex()
                .id(SharedString::from(format!("gh-pr-file-{ix}")))
                .h(px(34.))
                .px(px(10.))
                .gap(px(8.))
                .bg(t.raised)
                .cursor_pointer()
                .child(Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).size(px(13.)).text_color(t.fg_subtle))
                .child(div().flex_1().min_w_0().truncate().font_family(t.mono.clone()).text_size(TEXT_SMALL).child(file.filename.clone()))
                .child(widgets::chip(file.status.clone(), &t))
                .child(div().text_size(TEXT_SMALL).text_color(t.success).child(format!("+{}", file.additions)))
                .child(div().text_size(TEXT_SMALL).text_color(t.danger).child(format!("−{}", file.deletions)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.toggled_files.remove(&name) {
                        this.toggled_files.insert(name.clone());
                    }
                    cx.notify();
                }));
            let body = expanded.then(|| match file.patch.as_deref() {
                Some(patch) => {
                    let (lines, hidden) = shown_lines(patch, MAX_DIFF_LINES);
                    v_flex()
                        .border_t_1()
                        .border_color(t.border)
                        .font_family(t.mono.clone())
                        .text_size(TEXT_SMALL)
                        .children(lines.iter().map(|line| render_diff_line(line, &t)))
                        .when(hidden > 0, |el| {
                            el.child(div().px(px(10.)).py(px(6.)).text_color(t.fg_subtle).child(format!("{hidden} more lines — open on GitHub to see them")))
                        })
                        .into_any_element()
                }
                None => div()
                    .border_t_1()
                    .border_color(t.border)
                    .px(px(10.))
                    .py(px(8.))
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_subtle)
                    .child("No diff to show (binary or very large file).")
                    .into_any_element(),
            });
            v_flex()
                .rounded(RADIUS)
                .border_1()
                .border_color(t.border)
                .overflow_hidden()
                .child(header)
                .children(body)
        });
        v_flex()
            .gap(px(10.))
            .children(files)
            .when(detail.files.len() >= 100, |el| {
                el.child(
                    h_flex()
                        .gap(px(6.))
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_subtle)
                        .child("Only the first 100 files are listed.")
                        .child(open_on_github("gh-pr-files-open", base_url)),
                )
            })
            .into_any_element()
    }

    fn render_issues(&self, repo: &RepoState, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let list = match &repo.issues {
            Load::Ready(issues) if issues.is_empty() => empty_state(icon("circle-dot"), "No issues", "Nothing matches this filter.", &t),
            Load::Ready(issues) => list_box(issues.iter().map(|issue| self.render_issue_row(issue, &t, cx)), &t),
            Load::Failed(err) => error_text(err, &t),
            _ => loading_line("Loading issues…", &t),
        };
        scroll_area(
            "gh-issues",
            &self.scroll,
            reading_column()
                .child(self.render_filter_bar(repo, repo.issue_filter, false, cx))
                .child(list)
                .into_any_element(),
        )
    }

    fn render_issue_row(&self, issue: &Issue, t: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let number = issue.number;
        let open = issue.state == "open";
        list_row(SharedString::from(format!("gh-issue-{number}")), t)
            .child(div().pt(px(2.)).child(widgets::issue_icon(open, px(15.), t)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .child(div().text_size(TEXT_BODY).font_weight(FontWeight::MEDIUM).text_color(t.fg).child(issue.title.clone()))
                            .children(issue.labels.iter().map(|l| widgets::label(&l.name, l.color.as_deref(), t))),
                    )
                    .child(div().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(format!(
                        "#{number} · opened by {} · updated {}",
                        issue.user,
                        widgets::ago(issue.updated_at)
                    ))),
            )
            .when(issue.comments > 0, |el| {
                el.child(
                    h_flex()
                        .flex_none()
                        .gap(px(4.))
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_subtle)
                        .child(icon("message-square").size(px(12.)))
                        .child(issue.comments.to_string()),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| this.model.update(cx, |m, cx| m.open_issue(number, cx))))
            .into_any_element()
    }

    fn render_issue_detail(&self, repo: &RepoState, number: u64, detail: &Load<IssueDetail>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let back = back_button("Issues", cx);
        let detail = match detail {
            Load::Ready(detail) => detail,
            Load::Failed(err) => return detail_shell(back, error_text(err, &t), &self.scroll),
            _ => return detail_shell(back, loading_line(&format!("Loading #{number}…"), &t), &self.scroll),
        };
        let issue = &detail.issue;
        let open = issue.state == "open";
        let header = v_flex()
            .gap(px(8.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(div().flex_1().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(t.fg).child(format!("{} #{}", issue.title, issue.number)))
                    .child(open_on_github("gh-issue-open", issue.html_url.clone())),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .text_size(TEXT_BODY)
                    .text_color(t.fg_muted)
                    .child(widgets::pill(if open { "Open" } else { "Closed" }, widgets::issue_color(open, &t), &t).child(widgets::issue_icon(open, px(12.), &t)))
                    .child(format!(
                        "{} opened this {} · {} comment{}",
                        issue.user,
                        widgets::ago(issue.created_at),
                        issue.comments,
                        if issue.comments == 1 { "" } else { "s" }
                    ))
                    .children(issue.labels.iter().map(|l| widgets::label(&l.name, l.color.as_deref(), &t))),
            )
            .when(!issue.assignees.is_empty(), |el| {
                el.child(div().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(format!("Assigned to {}", issue.assignees.join(", "))))
            });
        let body = conversation_item(&issue.user, issue.created_at, issue.body.as_deref(), format!("gh-issue-body-{}-{number}", repo.full_name()), window, cx);
        let target = Target::Issue(Some(number));
        let comments = self.render_comments(&detail.comments, repo, number, target, window, cx);
        let actions = (!repo.repo.archived).then(|| {
            h_flex()
                .flex_wrap()
                .gap(px(8.))
                .child(self.edit_button(
                    "gh-issue-edit",
                    "Edit issue",
                    Edit::Issue(issue.clone()),
                    false,
                    cx,
                ))
                .child(self.edit_menu(
                    "gh-issue-actions",
                    "Issue actions",
                    vec![
                        (
                            "Manage assignees, labels and milestone",
                            Edit::Metadata {
                                target,
                                number,
                                assignees: issue.assignees.clone(),
                                labels: issue.labels.iter().map(|l| l.name.clone()).collect(),
                                milestone: issue.milestone.clone(),
                            },
                        ),
                        (
                            if open { "Close issue" } else { "Reopen issue" },
                            Edit::IssueState {
                                number,
                                close: open,
                            },
                        ),
                    ],
                    cx,
                ))
        });
        detail_shell(
            back,
            v_flex()
                .gap(px(18.))
                .child(header)
                .children(actions)
                .child(div().h(px(1.)).bg(t.border))
                .child(body)
                .children(comments)
                .when(!repo.repo.archived, |el| {
                    el.child(self.edit_button(
                        "gh-issue-comment",
                        "Add comment",
                        Edit::Comment {
                            target,
                            number,
                            comment: None,
                        },
                        false,
                        cx,
                    ))
                })
                .into_any_element(),
            &self.scroll,
        )
    }

    fn render_comments(
        &self,
        comments: &[Comment],
        repo: &RepoState,
        number: u64,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let login = self.model.read(cx).viewer().map(|v| v.login.clone());
        comments
            .iter()
            .map(|comment| {
                let own = !repo.repo.archived
                    && login
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(&comment.user));
                v_flex()
                    .gap(px(4.))
                    .child(conversation_item(
                        &comment.user,
                        comment.created_at,
                        comment.body.as_deref(),
                        format!("gh-comment-{}-{}", repo.full_name(), comment.id),
                        window,
                        cx,
                    ))
                    .when(own, |el| {
                        el.child(h_flex().justify_end().child(self.edit_menu(
                            ("gh-comment-actions", comment.id),
                            "Comment actions",
                            vec![
                                (
                                    "Edit comment",
                                    Edit::Comment {
                                        target,
                                        number,
                                        comment: Some(comment.clone()),
                                    },
                                ),
                                (
                                    "Delete comment",
                                    Edit::DeleteComment {
                                        target,
                                        number,
                                        id: comment.id,
                                    },
                                ),
                            ],
                            cx,
                        )))
                    })
                    .into_any_element()
            })
            .collect()
    }

    // --- Actions ---------------------------------------------------------------

    fn render_actions(&self, repo: &RepoState, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let workflows: Vec<Workflow> = repo.workflows.ready().cloned().unwrap_or_default();
        let selected = repo.workflow;
        let mut side = vec![self.workflow_row(None, "All workflows".into(), selected.is_none(), &t, cx)];
        side.push(div().h(px(8.)).into_any_element());
        side.extend(workflows.iter().map(|w| {
            let label = if w.state == "active" { w.name.clone() } else { format!("{} (disabled)", w.name) };
            self.workflow_row(Some(w.id), label, selected == Some(w.id), &t, cx)
        }));
        if let Load::Failed(err) = &repo.workflows {
            side.push(div().px(px(8.)).child(error_text(err, &t)).into_any_element());
        }
        let names: std::collections::HashMap<u64, String> = workflows.iter().map(|w| (w.id, w.name.clone())).collect();
        let runs = match &repo.runs {
            Load::Ready(runs) if runs.is_empty() => empty_state(icon("workflow"), "No workflow runs", "Runs show up here once a workflow runs.", &t),
            Load::Ready(runs) => list_box(runs.iter().map(|run| self.render_run_row(run, names.get(&run.workflow_id).map(String::as_str), &t, cx)), &t),
            Load::Failed(err) => error_text(err, &t),
            _ => loading_line("Loading runs…", &t),
        };
        h_flex()
            .size_full()
            .items_start()
            .child(
                div()
                    .w(px(240.))
                    .flex_none()
                    .h_full()
                    .border_r_1()
                    .border_color(t.border)
                    .child(
                        v_flex()
                            .id("gh-workflows")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.side_scroll)
                            .p(px(8.))
                            .gap(px(1.))
                            .child(div().px(px(8.)).py(px(6.)).text_size(TEXT_SMALL).font_weight(FontWeight::SEMIBOLD).text_color(t.fg_subtle).child("Workflows"))
                            .children(side),
                    ),
            )
            .child(
                div().flex_1().min_w_0().h_full().child(scroll_area(
                    "gh-runs",
                    &self.scroll,
                    v_flex()
                        .px(px(16.))
                        .pb(px(24.))
                        .child(
                            h_flex()
                                .h(px(44.))
                                .gap(px(8.))
                                .text_size(TEXT_BODY)
                                .font_weight(FontWeight::MEDIUM)
                                .child(
                                    selected
                                        .and_then(|id| names.get(&id).cloned())
                                        .unwrap_or_else(|| "All workflow runs".to_string()),
                                )
                                .child(div().flex_1())
                                .when_some(
                                    workflows.iter().find(|w| {
                                        selected == Some(w.id)
                                            && w.state == "active"
                                            && !repo.repo.archived
                                    }),
                                    |el, workflow| {
                                        el.child(self.edit_button(
                                            "gh-dispatch-workflow",
                                            "Run workflow",
                                            Edit::Workflow {
                                                id: workflow.id,
                                                name: workflow.name.clone(),
                                                git_ref: repo.git_ref.clone(),
                                            },
                                            true,
                                            cx,
                                        ))
                                    },
                                ),
                        )
                        .child(runs)
                        .into_any_element(),
                )),
            )
            .into_any_element()
    }

    fn workflow_row(&self, id: Option<u64>, label: String, selected: bool, t: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        chrome::list_row(SharedString::from(format!("gh-workflow-{}", id.unwrap_or(0))), selected, cx)
            .h(px(28.))
            .child(icon("workflow").size(px(13.)).text_color(t.fg_subtle))
            .child(div().flex_1().min_w_0().truncate().text_size(TEXT_BODY).text_color(if selected { t.fg } else { t.fg_muted }).child(label))
            .on_click(cx.listener(move |this, _, _, cx| this.model.update(cx, |m, cx| m.select_workflow(id, cx))))
            .into_any_element()
    }

    fn render_run_row(&self, run: &Run, workflow: Option<&str>, t: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let state = run.state();
        let workflow = workflow.map(str::to_string).or_else(|| run.name.clone()).unwrap_or_default();
        let open = run.clone();
        list_row(SharedString::from(format!("gh-run-{}", run.id)), t)
            .child(div().pt(px(2.)).child(widgets::run_icon(state, px(15.), t)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    .child(div().text_size(TEXT_BODY).font_weight(FontWeight::MEDIUM).text_color(t.fg).truncate().child(run.title().to_string()))
                    .child(div().text_size(TEXT_SMALL).text_color(t.fg_subtle).truncate().child(format!(
                        "{workflow} #{} · {} · {}{}",
                        run.run_number,
                        run.event,
                        run.actor.as_deref().map(|a| format!("{a} · ")).unwrap_or_default(),
                        widgets::ago(run.created_at)
                    ))),
            )
            .when_some(run.head_branch.clone(), |el, branch| el.child(widgets::branch(&branch, t)))
            .child(div().w(px(80.)).flex_none().text_right().text_size(TEXT_SMALL).text_color(widgets::run_color(state, t)).child(state.label()))
            .on_click(cx.listener(move |this, _, _, cx| {
                let run = open.clone();
                this.model.update(cx, |m, cx| m.open_run(run, cx));
            }))
            .into_any_element()
    }

    fn render_run_detail(
        &self,
        repo: &RepoState,
        run: &Run,
        jobs: &Load<Vec<Job>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = tokens(cx).clone();
        let back = back_button("Runs", cx);
        let state = run.state();
        let header = v_flex()
            .gap(px(8.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(widgets::run_icon(state, px(20.), &t))
                    .child(div().flex_1().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(t.fg).child(run.title().to_string()))
                    .child(open_on_github("gh-run-open", run.html_url.clone())),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .text_size(TEXT_BODY)
                    .text_color(t.fg_muted)
                    .child(widgets::pill(state.label(), widgets::run_color(state, &t), &t))
                    .child(format!(
                        "{} #{} · {} · {}",
                        run.name.clone().unwrap_or_default(),
                        run.run_number,
                        run.event,
                        widgets::ago(run.created_at)
                    ))
                    .when_some(run.head_branch.clone(), |el, b| el.child(widgets::branch(&b, &t)))
                    .child(div().font_family(t.mono.clone()).text_size(TEXT_SMALL).child(run.head_sha.chars().take(7).collect::<String>())),
            );
        let jobs_view = match jobs {
            Load::Ready(jobs) if jobs.is_empty() => muted_text("No jobs.", &t),
            Load::Ready(jobs) => v_flex().gap(px(10.)).children(jobs.iter().map(|job| render_job(job, &t))).into_any_element(),
            Load::Failed(err) => error_text(err, &t),
            _ => loading_line("Loading jobs…", &t),
        };
        detail_shell(
            back,
            v_flex()
                .gap(px(18.))
                .child(header)
                .when(!repo.repo.archived, |el| {
                    let actions = if run.status == "completed" {
                        let mut actions = vec![(
                            "Rerun workflow",
                            Edit::Run {
                                run: run.clone(),
                                action: RunAction::Rerun,
                            },
                        )];
                        if run.conclusion.as_deref() == Some("failure") {
                            actions.push((
                                "Rerun failed jobs",
                                Edit::Run {
                                    run: run.clone(),
                                    action: RunAction::RerunFailedJobs,
                                },
                            ));
                        }
                        actions
                    } else {
                        vec![(
                            "Cancel run",
                            Edit::Run {
                                run: run.clone(),
                                action: RunAction::Cancel,
                            },
                        )]
                    };
                    el.child(self.edit_menu("gh-run-actions", "Run actions", actions, cx))
                })
                .child(div().h(px(1.)).bg(t.border))
                .child(section_title("Jobs", &t))
                .child(jobs_view)
                .child(muted_text("Logs open on GitHub: use the arrow on a job or run.", &t))
                .into_any_element(),
            &self.scroll,
        )
    }
}

impl Focusable for GitHubView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitHubView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let model = self.model.read(cx);
        let signed_in = model.viewer().is_some();
        let repo = model.repo.clone();

        let content = if !signed_in {
            self.render_sign_in(cx)
        } else if let Some(repo) = repo {
            let body = match (&repo.tab, &repo.detail) {
                (RepoTab::Pulls, Detail::Pull(number, detail)) => {
                    self.render_pull_detail(&repo, *number, detail, window, cx)
                }
                (RepoTab::Issues, Detail::Issue(number, detail)) => {
                    self.render_issue_detail(&repo, *number, detail, window, cx)
                }
                (RepoTab::Actions, Detail::Run(run, jobs)) => {
                    self.render_run_detail(&repo, run, jobs, cx)
                }
                (RepoTab::Code, _) => self.render_code(&repo, window, cx),
                (RepoTab::Pulls, _) => self.render_pulls(&repo, cx),
                (RepoTab::Issues, _) => self.render_issues(&repo, cx),
                (RepoTab::Actions, _) => self.render_actions(&repo, cx),
            };
            let description = repo.repo.description.clone().filter(|d| !d.trim().is_empty());
            v_flex()
                .size_full()
                .child(self.render_repo_header(&repo, cx))
                .when_some(description, |el, d| {
                    el.child(div().px(px(16.)).pb(px(8.)).mt(px(-6.)).text_size(TEXT_SMALL).text_color(t.fg_muted).truncate().child(d))
                })
                .child(self.render_tabs(&repo, cx))
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element()
        } else {
            self.render_home(cx)
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(t.surface)
            .text_color(t.fg)
            .child(content)
    }
}

// --- pieces ---------------------------------------------------------------------

fn centered(child: AnyElement) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(24.))
        .child(child)
        .into_any_element()
}

fn loading_line(text: &str, t: &Tokens) -> AnyElement {
    h_flex()
        .py(px(12.))
        .gap(px(8.))
        .text_size(TEXT_BODY)
        .text_color(t.fg_muted)
        .child(Spinner::new().small())
        .child(text.to_string())
        .into_any_element()
}

fn error_text(err: &str, t: &Tokens) -> AnyElement {
    div().py(px(8.)).text_size(TEXT_BODY).text_color(t.danger).child(err.to_string()).into_any_element()
}

fn muted_text(text: &str, t: &Tokens) -> AnyElement {
    div().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(text.to_string()).into_any_element()
}

fn section_title(text: &str, t: &Tokens) -> AnyElement {
    div().text_size(TEXT_TITLE).font_weight(FontWeight::SEMIBOLD).text_color(t.fg).child(text.to_string()).into_any_element()
}

fn empty_state(mark: Icon, title: &str, detail: &str, t: &Tokens) -> AnyElement {
    v_flex()
        .items_center()
        .py(px(48.))
        .gap(px(6.))
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(t.border)
        .child(mark.size(px(22.)).text_color(t.fg_subtle))
        .child(div().text_size(TEXT_BODY).font_weight(FontWeight::MEDIUM).child(title.to_string()))
        .child(div().text_size(TEXT_SMALL).text_color(t.fg_muted).child(detail.to_string()))
        .into_any_element()
}

/// A vertical scroll container with a scrollbar.
fn scroll_area(id: &'static str, handle: &ScrollHandle, child: AnyElement) -> AnyElement {
    div()
        .relative()
        .size_full()
        .child(div().id(id).size_full().overflow_y_scroll().track_scroll(handle).child(child))
        .vertical_scrollbar(handle)
        .into_any_element()
}

fn reading_column() -> gpui::Div {
    v_flex().w_full().max_w(px(READING_WIDTH)).mx_auto().px(px(16.)).pb(px(24.))
}

/// Rows in one bordered box, divided by lines.
fn list_box(rows: impl Iterator<Item = AnyElement>, t: &Tokens) -> AnyElement {
    v_flex()
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(t.border)
        .overflow_hidden()
        .children(rows)
        .into_any_element()
}

fn list_row(id: SharedString, t: &Tokens) -> gpui::Stateful<gpui::Div> {
    let hover = t.hover;
    h_flex()
        .id(id)
        .items_start()
        .px(px(14.))
        .py(px(10.))
        .gap(px(10.))
        .border_b_1()
        .border_color(t.border)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
}

fn file_row(id: SharedString, t: &Tokens) -> gpui::Stateful<gpui::Div> {
    let hover = t.hover;
    h_flex()
        .id(id)
        .h(px(36.))
        .px(px(14.))
        .gap(px(10.))
        .border_b_1()
        .border_color(t.border)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
}

fn crumb(id: SharedString, label: String, target: String, clickable: bool, t: &Tokens, cx: &mut Context<GitHubView>) -> AnyElement {
    let el = div().id(id).child(label);
    if !clickable {
        return el.font_weight(FontWeight::SEMIBOLD).text_color(t.fg).into_any_element();
    }
    el.text_color(t.accent_text)
        .cursor_pointer()
        .hover(|s| s.underline())
        .on_click(cx.listener(move |this, _, _, cx| {
            let target = target.clone();
            this.model.update(cx, |m, cx| m.open_dir(target, cx));
        }))
        .into_any_element()
}

fn back_button(label: &'static str, cx: &mut Context<GitHubView>) -> AnyElement {
    Button::new("gh-back")
        .icon(IconName::ArrowLeft)
        .label(label)
        .ghost()
        .small()
        .on_click(cx.listener(|this, _, _, cx| this.model.update(cx, |m, cx| m.close_detail(cx))))
        .into_any_element()
}

fn open_on_github(id: &'static str, url: String) -> AnyElement {
    Button::new(id)
        .label("Open on GitHub")
        .icon(icon("external-link"))
        .ghost()
        .small()
        .on_click(move |_, _, cx| cx.open_url(&url))
        .into_any_element()
}

/// A detail page: a back button over a scrolling reading column.
fn detail_shell(back: AnyElement, body: AnyElement, scroll: &ScrollHandle) -> AnyElement {
    scroll_area(
        "gh-detail",
        scroll,
        reading_column()
            .pt(px(8.))
            .gap(px(12.))
            .child(h_flex().child(back))
            .child(body)
            .into_any_element(),
    )
}

/// The opening post or a comment: author, time and Markdown body.
fn conversation_item(user: &str, at: i64, body: Option<&str>, id: String, window: &mut Window, cx: &mut Context<GitHubView>) -> AnyElement {
    let t = tokens(cx).clone();
    let text = body.filter(|b| !b.trim().is_empty());
    let body = match text {
        Some(text) => GitHubView::markdown(id, text, window, cx),
        None => muted_text("No description provided.", &t),
    };
    v_flex()
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(t.border)
        .overflow_hidden()
        .child(
            h_flex()
                .h(px(36.))
                .px(px(12.))
                .gap(px(8.))
                .border_b_1()
                .border_color(t.border)
                .bg(t.raised)
                .text_size(TEXT_SMALL)
                .child(crate::tasks::avatar(Some(user), 18., &t))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(t.fg).child(user.to_string()))
                .child(div().text_color(t.fg_subtle).child(widgets::ago(at))),
        )
        .child(div().p(px(14.)).child(body))
        .into_any_element()
}

fn stat(mark: Icon, value: String, t: &Tokens) -> AnyElement {
    h_flex()
        .gap(px(4.))
        .text_size(TEXT_SMALL)
        .text_color(t.fg_muted)
        .child(mark.size(px(13.)))
        .child(value)
        .into_any_element()
}

fn repo_card_body(repo: &Repo, t: &Tokens) -> AnyElement {
    v_flex()
        .gap(px(6.))
        .child(
            h_flex()
                .gap(px(6.))
                .child(icon(if repo.private { "lock" } else { "book-marked" }).size(px(13.)).text_color(t.fg_subtle))
                .child(div().min_w_0().truncate().text_size(TEXT_BODY).font_weight(FontWeight::SEMIBOLD).text_color(t.accent_text).child(repo.full_name.clone())),
        )
        .child(
            div()
                .h(px(34.))
                .text_size(TEXT_SMALL)
                .text_color(t.fg_muted)
                .overflow_hidden()
                .child(repo.description.clone().unwrap_or_else(|| "No description".into())),
        )
        .child(
            h_flex()
                .gap(px(12.))
                .text_size(TEXT_SMALL)
                .text_color(t.fg_subtle)
                .when_some(repo.language.clone(), |el, l| el.child(l))
                .child(stat(Icon::new(IconName::Star), widgets::compact(repo.stargazers_count), t))
                .child(div().child(format!("pushed {}", widgets::ago(repo.pushed_at)))),
        )
        .into_any_element()
}

fn render_job(job: &Job, t: &Tokens) -> AnyElement {
    let state = job.state();
    let duration = (job.completed_at > job.started_at && job.started_at > 0).then(|| {
        let secs = job.completed_at - job.started_at;
        if secs >= 60 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        }
    });
    let url = job.html_url.clone();
    v_flex()
        .rounded(RADIUS)
        .border_1()
        .border_color(t.border)
        .overflow_hidden()
        .child(
            h_flex()
                .h(px(38.))
                .px(px(12.))
                .gap(px(8.))
                .bg(t.raised)
                .child(widgets::run_icon(state, px(15.), t))
                .child(div().flex_1().min_w_0().truncate().text_size(TEXT_BODY).font_weight(FontWeight::MEDIUM).child(job.name.clone()))
                .when_some(duration, |el, d| el.child(div().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(d)))
                .when_some(url, |el, url| {
                    el.child(
                        Button::new(SharedString::from(format!("gh-job-{}", job.id)))
                            .icon(icon("external-link"))
                            .ghost()
                            .xsmall()
                            .tooltip("Open logs on GitHub")
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                }),
        )
        .children(job.steps.iter().map(|step| {
            let state = step.state();
            h_flex()
                .h(px(28.))
                .px(px(12.))
                .pl(px(34.))
                .gap(px(8.))
                .border_t_1()
                .border_color(t.border)
                .child(widgets::run_icon(state, px(13.), t))
                .child(div().flex_1().min_w_0().truncate().text_size(TEXT_SMALL).text_color(t.fg_muted).child(step.name.clone()))
        }))
        .into_any_element()
}
