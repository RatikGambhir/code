//! The Tasks view: issues grouped by status, or projects with their
//! progress, with the open issue's details beside the list, laid out the
//! way Linear lays them out. Lists are virtualized and rebuilt only when the
//! model, the selection or a filter changes.

use std::{collections::HashSet, ops::Range};

use cockpit_tasks::{Issue, Priority, Project, Source, StatusKind};
use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, App, Context, Entity, FocusHandle, Focusable,
    FontWeight, ScrollHandle, SharedString, Subscription, UniformListScrollHandle, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Escape, Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    v_flex, Icon, IconName, Sizable as _,
};

use super::{
    edit::{Edit, IssueFields},
    model::{Selection, TasksModel, View},
    widgets,
};
use crate::{
    assets::icon,
    theme::{tokens, Tokens, RADIUS, TEXT_BODY, TEXT_CAPTION, TEXT_SMALL, TEXT_TITLE},
};

const ISSUE_ROW: f32 = 38.;
const PROJECT_ROW: f32 = 46.;
const DETAIL_WIDTH: f32 = 380.;
const LINEAR_KEYS_URL: &str = "https://linear.app/settings/account/security";

/// The tabs above an issue list.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
enum IssueTab {
    #[default]
    All,
    Active,
    Backlog,
}

impl IssueTab {
    const ALL: [IssueTab; 3] = [IssueTab::All, IssueTab::Active, IssueTab::Backlog];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All issues",
            Self::Active => "Active",
            Self::Backlog => "Backlog",
        }
    }

    fn admits(self, kind: StatusKind) -> bool {
        match self {
            Self::All => true,
            Self::Active => kind.is_active(),
            Self::Backlog => kind == StatusKind::Backlog,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Row {
    Group {
        kind: StatusKind,
        count: usize,
    },
    /// Index into the source board's issues.
    Issue(usize),
    /// Index into the source board's projects.
    Project(usize),
}

/// A visible row's data, copied out of the model for drawing.
enum Visible {
    Group(StatusKind, usize),
    Issue(Box<Issue>, Option<Project>, bool),
    Project(Project),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ComposerTarget {
    Issue(StatusKind),
    Project,
}

/// The inline "new issue" / "new project" field.
struct Composer {
    input: Entity<InputState>,
    target: ComposerTarget,
    _subscription: Subscription,
}

pub struct TasksView {
    pub(super) model: Entity<TasksModel>,
    filter: Entity<InputState>,
    key_input: Entity<InputState>,
    composer: Option<Composer>,
    reconnecting_linear: bool,
    tab: IssueTab,
    collapsed: HashSet<StatusKind>,
    rows: Vec<Row>,
    /// The model revision and selection the rows were built for.
    rows_for: Option<(u64, Selection)>,
    rows_dirty: bool,
    scroll: UniformListScrollHandle,
    detail_scroll: ScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl TasksView {
    pub fn new(model: Entity<TasksModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by title or ID"));
        let key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("lin_api_…")
        });
        let subscriptions = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.rows_dirty = true;
                    cx.notify();
                }
            }),
            cx.subscribe(&key_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.connect(cx);
                }
            }),
        ];
        Self {
            model,
            filter,
            key_input,
            composer: None,
            reconnecting_linear: false,
            tab: IssueTab::default(),
            collapsed: HashSet::new(),
            rows: Vec::new(),
            rows_for: None,
            rows_dirty: true,
            scroll: UniformListScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let key = self.key_input.read(cx).value().to_string();
        self.model.update(cx, |m, cx| m.connect_linear(key, cx));
    }

    fn set_tab(&mut self, tab: IssueTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.rows_dirty = true;
            cx.notify();
        }
    }

    fn toggle_group(&mut self, kind: StatusKind, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&kind) {
            self.collapsed.insert(kind);
        }
        self.rows_dirty = true;
        cx.notify();
    }

    fn start_composer(
        &mut self,
        target: ComposerTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = match target {
            ComposerTarget::Issue(_) => "Issue title — Enter to add, Esc to close",
            ComposerTarget::Project => "Project name — Enter to add, Esc to close",
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit_composer(window, cx),
                InputEvent::Blur
                    if this
                        .composer
                        .as_ref()
                        .is_some_and(|c| c.input.read(cx).value().trim().is_empty()) =>
                {
                    this.composer = None;
                    cx.notify();
                }
                _ => {}
            },
        );
        input.update(cx, |state, cx| state.focus(window, cx));
        self.composer = Some(Composer {
            input,
            target,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = &self.composer else {
            return;
        };
        let text = composer.input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        match composer.target.clone() {
            ComposerTarget::Issue(kind) => {
                let project = match &self.model.read(cx).selection.view {
                    View::Project(id) => Some(id.clone()),
                    _ => None,
                };
                self.model
                    .update(cx, |m, cx| m.add_issue(&text, kind, project, cx));
                // Stay open for the next one, as Linear's quick add does.
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            ComposerTarget::Project => {
                if let Some(id) = self.model.update(cx, |m, cx| m.add_project(&text, cx)) {
                    self.composer = None;
                    self.model.update(cx, |m, cx| {
                        m.select(
                            Selection {
                                source: Source::Local,
                                view: View::Project(id),
                            },
                            cx,
                        )
                    });
                }
            }
        }
        cx.notify();
    }

    /// Rebuilds the rows when what they show changed.
    fn ensure_rows(&mut self, cx: &App) {
        let model = self.model.read(cx);
        let current = (model.revision(), model.selection.clone());
        if !self.rows_dirty && self.rows_for.as_ref() == Some(&current) {
            return;
        }
        let filter = self.filter.read(cx).value().trim().to_lowercase();
        let board = model.board(current.1.source);
        let mut rows = std::mem::take(&mut self.rows);
        rows.clear();
        match &current.1.view {
            View::Projects => {
                let mut projects: Vec<usize> = board
                    .projects
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| filter.is_empty() || p.name.to_lowercase().contains(&filter))
                    .map(|(ix, _)| ix)
                    .collect();
                projects.sort_by(|&a, &b| {
                    let (a, b) = (&board.projects[a], &board.projects[b]);
                    a.state
                        .cmp(&b.state)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                rows.extend(projects.into_iter().map(Row::Project));
            }
            view => {
                let mut issues: Vec<usize> = board
                    .issues
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| match view {
                        View::MyIssues => i.mine,
                        View::Project(id) => i.project_id.as_deref() == Some(id.as_str()),
                        _ => true,
                    })
                    .filter(|(_, i)| self.tab.admits(i.status.kind))
                    .filter(|(_, i)| {
                        filter.is_empty()
                            || i.title.to_lowercase().contains(&filter)
                            || i.identifier.to_lowercase().contains(&filter)
                    })
                    .map(|(ix, _)| ix)
                    .collect();
                // Status group, then priority (urgent first, none last), then newest.
                issues.sort_by(|&a, &b| {
                    let (a, b) = (&board.issues[a], &board.issues[b]);
                    a.status
                        .kind
                        .cmp(&b.status.kind)
                        .then(a.priority.cmp(&b.priority))
                        .then(b.updated_at.cmp(&a.updated_at))
                });
                for group in issues
                    .chunk_by(|&a, &b| board.issues[a].status.kind == board.issues[b].status.kind)
                {
                    let kind = board.issues[group[0]].status.kind;
                    rows.push(Row::Group {
                        kind,
                        count: group.len(),
                    });
                    if !self.collapsed.contains(&kind) {
                        rows.extend(group.iter().map(|&ix| Row::Issue(ix)));
                    }
                }
            }
        }
        self.rows = rows;
        self.rows_for = Some(current);
        self.rows_dirty = false;
    }

    // --- rendering ---------------------------------------------------------

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = tokens(cx).clone();
        // Copy out only the visible rows' data, so drawing can take `cx`.
        let (source, visible) = {
            let model = self.model.read(cx);
            let source = model.selection.source;
            let in_project = matches!(model.selection.view, View::Project(_));
            let open = model.open_issue.as_ref();
            let board = model.board(source);
            let visible: Vec<Visible> = range
                .filter_map(|ix| self.rows.get(ix).copied())
                .map(|row| match row {
                    Row::Group { kind, count } => Visible::Group(kind, count),
                    Row::Issue(i) => {
                        let issue = board.issues[i].clone();
                        let project = (!in_project)
                            .then(|| {
                                issue
                                    .project_id
                                    .as_deref()
                                    .and_then(|id| board.projects.iter().find(|p| p.id == id))
                            })
                            .flatten()
                            .cloned();
                        let selected = open.is_some_and(|(s, id)| *s == source && *id == issue.id);
                        Visible::Issue(Box::new(issue), project, selected)
                    }
                    Row::Project(p) => Visible::Project(board.projects[p].clone()),
                })
                .collect();
            (source, visible)
        };
        visible
            .into_iter()
            .map(|row| match row {
                Visible::Group(kind, count) => self.render_group(kind, count, source, &t, cx),
                Visible::Issue(issue, project, selected) => {
                    render_issue_row(&issue, project.as_ref(), selected, &t, cx)
                }
                Visible::Project(project) => render_project_row(&project, source, &t, cx),
            })
            .collect()
    }

    fn render_group(
        &self,
        kind: StatusKind,
        count: usize,
        source: Source,
        t: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = self.collapsed.contains(&kind);
        h_flex()
            .id(("tasks-group", kind as usize))
            .h(px(ISSUE_ROW))
            .px(px(16.))
            .gap(px(8.))
            .bg(t.raised)
            .border_b_1()
            .border_color(t.border)
            .cursor_pointer()
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(12.))
                .text_color(t.fg_subtle),
            )
            .child(
                widgets::kind_icon(kind)
                    .size(px(14.))
                    .text_color(widgets::kind_color(kind, t)),
            )
            .child(
                div()
                    .text_size(TEXT_BODY)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.fg)
                    .child(kind.label()),
            )
            .child(
                div()
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_subtle)
                    .child(count.to_string()),
            )
            .child(div().flex_1())
            .when(source == Source::Local, |el| {
                el.child(
                    Button::new(("tasks-group-add", kind as usize))
                        .icon(IconName::Plus)
                        .ghost()
                        .xsmall()
                        .tooltip(format!("New issue in {}", kind.label()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.start_composer(ComposerTarget::Issue(kind), window, cx);
                        })),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(kind, cx)))
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let model = self.model.read(cx);
        let selection = model.selection.clone();
        let source = selection.source;
        let source_label = match source {
            Source::Local => model.workspace_name(),
            Source::Linear => "Linear".to_string(),
        };
        let (title, project_url) = match &selection.view {
            View::MyIssues => ("My issues".to_string(), None),
            View::Issues => (
                if source == Source::Linear {
                    "All issues"
                } else {
                    "Issues"
                }
                .to_string(),
                None,
            ),
            View::Projects => ("Projects".to_string(), None),
            View::Project(id) => model
                .project(source, id)
                .map_or(("Project".to_string(), None), |p| {
                    (p.name.clone(), p.url.clone())
                }),
        };
        let connected = source == Source::Local || model.linear.has_key;
        let loading = model.linear.loading;
        let issues_view = !matches!(selection.view, View::Projects);
        let linear_editable = source == Source::Linear
            && model.linear.write_authorized
            && model.linear.viewer.is_some()
            && !model.linear.connecting;

        h_flex()
            .h(px(44.))
            .flex_none()
            .px(px(16.))
            .gap(px(10.))
            .border_b_1()
            .border_color(t.border)
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .text_size(TEXT_BODY)
                    .child(div().text_color(t.fg_subtle).child(source_label))
                    .child(div().text_color(t.fg_subtle).child("›"))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.fg)
                            .child(title),
                    ),
            )
            .when(issues_view && connected, |el| {
                el.child(
                    h_flex()
                        .ml(px(8.))
                        .gap(px(4.))
                        .children(IssueTab::ALL.map(|tab| {
                            let active = self.tab == tab;
                            div()
                                .id(("tasks-tab", tab as usize))
                                .h(px(24.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .rounded(RADIUS)
                                .border_1()
                                .text_size(TEXT_SMALL)
                                .cursor_pointer()
                                .when(active, |el| {
                                    el.border_color(t.border_strong)
                                        .bg(t.raised)
                                        .text_color(t.fg)
                                })
                                .when(!active, |el| {
                                    el.border_color(gpui::transparent_black())
                                        .text_color(t.fg_muted)
                                        .hover(|s| s.bg(t.hover))
                                })
                                .child(tab.label())
                                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
                        })),
                )
            })
            .child(div().flex_1())
            .when(connected, |el| {
                el.child(
                    div().w(px(200.)).child(
                        Input::new(&self.filter)
                            .small()
                            .prefix(Icon::new(IconName::Search).size(px(12.))),
                    ),
                )
            })
            .when(source == Source::Linear && connected, |el| {
                el.child(
                    Button::new("tasks-refresh")
                        .icon(icon("refresh-cw"))
                        .ghost()
                        .small()
                        .loading(loading)
                        .tooltip("Pull from Linear")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |m, cx| m.refresh_linear(cx))
                        })),
                )
            })
            .when_some(project_url, |el, url| {
                el.child(
                    Button::new("tasks-open-project")
                        .icon(icon("external-link"))
                        .ghost()
                        .small()
                        .tooltip("Open in Linear")
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
            .when(linear_editable && issues_view, |el| {
                el.child(
                    Button::new("tasks-linear-new")
                        .icon(IconName::Plus)
                        .label("New issue")
                        .primary()
                        .small()
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_linear_create(window, cx)),
                        ),
                )
            })
            .when(source == Source::Local, |el| {
                let projects = matches!(selection.view, View::Projects);
                let status = if self.tab == IssueTab::Backlog {
                    StatusKind::Backlog
                } else {
                    StatusKind::Unstarted
                };
                el.child(
                    Button::new("tasks-new")
                        .icon(IconName::Plus)
                        .label(if projects { "New project" } else { "New issue" })
                        .primary()
                        .small()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let target = if projects {
                                ComposerTarget::Project
                            } else {
                                ComposerTarget::Issue(status)
                            };
                            this.start_composer(target, window, cx);
                        })),
                )
            })
            .into_any_element()
    }

    /// The strip under the header in a project: state, lead, target, progress.
    fn render_project_summary(&self, project: &Project, cx: &App) -> AnyElement {
        let t = tokens(cx);
        let color = widgets::project_color(project.color.as_deref(), t);
        h_flex()
            .flex_none()
            .px(px(16.))
            .py(px(10.))
            .gap(px(18.))
            .border_b_1()
            .border_color(t.border)
            .text_size(TEXT_SMALL)
            .text_color(t.fg_muted)
            .child(widgets::project_icon(project.color.as_deref(), px(16.), t))
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .bg(widgets::project_state_color(project.state, t)),
                    )
                    .child(project.state.label()),
            )
            .when_some(project.lead.clone(), |el, lead| {
                el.child(
                    h_flex()
                        .gap(px(6.))
                        .child(widgets::avatar(Some(&lead), 16., t))
                        .child(lead),
                )
            })
            .when_some(
                project.target_date.as_deref().map(widgets::target_date),
                |el, date| {
                    el.child(
                        h_flex()
                            .gap(px(6.))
                            .child(Icon::new(IconName::Calendar).size(px(12.)))
                            .child(date),
                    )
                },
            )
            .child(widgets::progress(project.progress, color, t))
            .when(!project.description.is_empty(), |el| {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(t.fg_subtle)
                        .child(project.description.clone()),
                )
            })
            .into_any_element()
    }

    fn render_composer(&self, composer: &Composer, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let mark = match composer.target {
            ComposerTarget::Issue(kind) => widgets::kind_icon(kind)
                .size(px(14.))
                .text_color(widgets::kind_color(kind, &t)),
            ComposerTarget::Project => icon("project").size(px(14.)).text_color(t.fg_muted),
        };
        h_flex()
            .flex_none()
            .h(px(ISSUE_ROW + 4.))
            .px(px(16.))
            .gap(px(10.))
            .border_b_1()
            .border_color(t.border)
            .bg(t.accent_soft.alpha(0.35))
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                cx.stop_propagation();
                this.composer = None;
                cx.notify();
            }))
            .child(mark)
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&composer.input).appearance(false).small()),
            )
            .into_any_element()
    }

    fn render_connect(&self, upgrading: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let linear = &self.model.read(cx).linear;
        if !linear.key_checked {
            return centered(Spinner::new().into_any_element());
        }
        let connecting = linear.connecting;
        let waiting = linear.waiting_for_browser;
        let error = linear.error.clone();
        let browser = self.model.read(cx).has_linear_browser_sign_in();
        let card = v_flex()
            .w(px(420.))
            .p(px(20.))
            .gap(px(12.))
            .rounded(crate::theme::RADIUS_LG)
            .border_1()
            .border_color(t.border)
            .bg(t.surface)
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(icon("list-todo").size(px(16.)).text_color(t.accent_text))
                    .child(
                        div()
                            .text_size(TEXT_TITLE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if upgrading {
                                "Enable Linear editing"
                            } else {
                                "Connect Linear"
                            }),
                    ),
            );
        if waiting {
            return centered(
                card.child(
                    h_flex()
                        .gap(px(8.))
                        .text_size(TEXT_BODY)
                        .text_color(t.fg_muted)
                        .child(Spinner::new().small())
                        .child("Finish signing in to Linear in your browser…"),
                )
                .child(
                    h_flex().child(
                        Button::new("tasks-cancel-sign-in")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |m, cx| m.cancel_linear_sign_in(cx))
                            })),
                    ),
                )
                .into_any_element(),
            );
        }
        centered(
            card.child(div().text_size(TEXT_BODY).text_color(t.fg_muted).child(
                    "Sign in to view and edit your Linear issues. If you use a personal API key, give it write access for the teams you want to edit.",
                ))
                .when(browser, |el| {
                    el.child(
                        Button::new("tasks-sign-in-browser")
                            .label("Sign in with Linear")
                            .icon(icon("external-link"))
                            .primary()
                            .loading(connecting)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |m, cx| m.connect_linear_in_browser(cx))
                            })),
                    )
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_subtle)
                            .child(div().flex_1().h(px(1.)).bg(t.border))
                            .child("or paste a personal API key")
                            .child(div().flex_1().h(px(1.)).bg(t.border)),
                    )
                })
                .child(Input::new(&self.key_input).small())
                .when_some(error, |el, error| el.child(div().text_size(TEXT_SMALL).text_color(t.danger).child(error)))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            Button::new("tasks-connect")
                                .label("Connect")
                                .when(!browser, |b| b.primary())
                                .small()
                                .loading(connecting)
                                .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                        )
                        .child(
                            Button::new("tasks-get-key")
                                .label("Create a key in Linear")
                                .icon(icon("external-link"))
                                .ghost()
                                .small()
                                .on_click(|_, _, cx| cx.open_url(LINEAR_KEYS_URL)),
                        ),
                )
                .when(upgrading, |el| {
                    el.child(Button::new("tasks-return-to-linear")
                        .label("Back to issues")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.reconnecting_linear = false;
                            cx.notify();
                        })))
                })
                .into_any_element(),
        )
    }

    fn render_empty(&self, cx: &App) -> AnyElement {
        let t = tokens(cx);
        let model = self.model.read(cx);
        let (title, detail) = match (&model.selection.view, model.selection.source) {
            (_, Source::Linear) if model.linear.loading => {
                return centered(
                    h_flex()
                        .gap(px(8.))
                        .text_size(TEXT_BODY)
                        .text_color(t.fg_muted)
                        .child(Spinner::new().small())
                        .child("Pulling from Linear…")
                        .into_any_element(),
                );
            }
            (View::Projects, Source::Local) => (
                "No projects yet",
                "Group issues into a project with New project.",
            ),
            (View::Projects, Source::Linear) => (
                "No projects",
                "Projects you can see in Linear show up here.",
            ),
            (View::MyIssues, _) => (
                "Nothing assigned to you",
                "Issues assigned to you in Linear show up here.",
            ),
            (_, Source::Local) => (
                "No issues",
                "Add one with New issue, or press + on a status group.",
            ),
            (_, Source::Linear) => ("No issues", "Nothing matches this view."),
        };
        let filtered = !self.filter.read(cx).value().trim().is_empty() || self.tab != IssueTab::All;
        centered(
            v_flex()
                .items_center()
                .gap(px(6.))
                .child(icon("list-todo").size(px(20.)).text_color(t.fg_subtle))
                .child(
                    div()
                        .text_size(TEXT_BODY)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.fg)
                        .child(if filtered { "No matches" } else { title }),
                )
                .child(
                    div()
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_muted)
                        .child(if filtered {
                            "Try another filter or tab."
                        } else {
                            detail
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_detail(&self, issue: &Issue, source: Source, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let board = self.model.read(cx).board(source);
        let project = issue
            .project_id
            .as_deref()
            .and_then(|id| board.projects.iter().find(|p| p.id == id))
            .cloned();
        let project = project.as_ref();
        let projects: Vec<(String, String)> = board
            .projects
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        let editable = source == Source::Local;
        let linear = &self.model.read(cx).linear;
        let linear_editable =
            source == Source::Linear && linear.write_authorized && !linear.connecting;
        let id = issue.id.clone();
        let handle = self.model.clone();

        let prop = |label: &'static str, value: AnyElement| {
            h_flex()
                .min_h(px(30.))
                .gap(px(8.))
                .child(
                    div()
                        .w(px(76.))
                        .flex_none()
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_subtle)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(value))
                .into_any_element()
        };
        let text = |text: String| {
            div()
                .text_size(TEXT_BODY)
                .text_color(t.fg)
                .truncate()
                .child(text)
                .into_any_element()
        };

        let status = if linear_editable {
            let title = format!("Change status · {}", issue.identifier);
            let edit = Edit::Issue {
                id: id.clone(),
                fields: IssueFields::Status,
            };
            Button::new("detail-linear-status")
                .icon(widgets::status_icon(&issue.status, px(14.), &t))
                .label(issue.status.name.clone())
                .ghost()
                .xsmall()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_linear_edit(title.clone(), edit.clone(), window, cx)
                }))
                .into_any_element()
        } else if editable {
            let (handle, id) = (handle.clone(), id.clone());
            Button::new("detail-status")
                .icon(widgets::status_icon(&issue.status, px(14.), &t))
                .label(issue.status.name.clone())
                .ghost()
                .xsmall()
                .dropdown_menu(move |mut menu, _, _| {
                    for kind in StatusKind::LOCAL {
                        let (handle, id) = (handle.clone(), id.clone());
                        menu = menu.item(
                            PopupMenuItem::new(kind.label())
                                .icon(widgets::kind_icon(kind))
                                .on_click(move |_, _, cx| {
                                    handle.update(cx, |m, cx| m.set_status(&id, kind, cx))
                                }),
                        );
                    }
                    menu
                })
                .into_any_element()
        } else {
            h_flex()
                .gap(px(6.))
                .child(widgets::status_icon(&issue.status, px(14.), &t))
                .child(text(issue.status.name.clone()))
                .into_any_element()
        };
        let priority = if editable {
            let (handle, id) = (handle.clone(), id.clone());
            Button::new("detail-priority")
                .icon(widgets::priority_icon(issue.priority, px(14.), &t))
                .label(issue.priority.label())
                .ghost()
                .xsmall()
                .dropdown_menu(move |mut menu, _, _| {
                    for priority in Priority::ALL {
                        let (handle, id) = (handle.clone(), id.clone());
                        menu = menu.item(PopupMenuItem::new(priority.label()).on_click(
                            move |_, _, cx| {
                                handle.update(cx, |m, cx| m.set_priority(&id, priority, cx))
                            },
                        ));
                    }
                    menu
                })
                .into_any_element()
        } else {
            h_flex()
                .gap(px(6.))
                .child(widgets::priority_icon(issue.priority, px(14.), &t))
                .child(text(issue.priority.label().to_string()))
                .into_any_element()
        };
        let project_value = if editable {
            let current = issue.project_id.clone();
            let (handle, id) = (handle.clone(), id.clone());
            Button::new("detail-project")
                .icon(widgets::project_icon(
                    project.and_then(|p| p.color.as_deref()),
                    px(13.),
                    &t,
                ))
                .label(project.map_or_else(|| "No project".to_string(), |p| p.name.clone()))
                .ghost()
                .xsmall()
                .dropdown_menu(move |mut menu, _, _| {
                    let none = (handle.clone(), id.clone());
                    menu = menu.item(
                        PopupMenuItem::new("No project")
                            .checked(current.is_none())
                            .on_click(move |_, _, cx| {
                                none.0.update(cx, |m, cx| m.set_project(&none.1, None, cx))
                            }),
                    );
                    for (project_id, name) in &projects {
                        let (handle, id, project_id) =
                            (handle.clone(), id.clone(), project_id.clone());
                        menu = menu.item(
                            PopupMenuItem::new(name.clone())
                                .checked(current.as_deref() == Some(project_id.as_str()))
                                .on_click(move |_, _, cx| {
                                    handle.update(cx, |m, cx| {
                                        m.set_project(&id, Some(project_id.clone()), cx)
                                    })
                                }),
                        );
                    }
                    menu
                })
                .into_any_element()
        } else {
            match project {
                Some(p) => h_flex()
                    .gap(px(6.))
                    .child(widgets::project_icon(p.color.as_deref(), px(13.), &t))
                    .child(text(p.name.clone()))
                    .into_any_element(),
                None => text("No project".into()),
            }
        };
        let assignee_name = issue
            .assignee
            .clone()
            .unwrap_or_else(|| "Unassigned".into());
        let assignee = if linear_editable {
            let title = format!("Change assignee · {}", issue.identifier);
            let edit = Edit::Issue {
                id: id.clone(),
                fields: IssueFields::Assignee,
            };
            Button::new("detail-linear-assignee")
                .label(assignee_name)
                .ghost()
                .xsmall()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_linear_edit(title.clone(), edit.clone(), window, cx)
                }))
                .into_any_element()
        } else {
            h_flex()
                .gap(px(6.))
                .child(widgets::avatar(issue.assignee.as_deref(), 18., &t))
                .child(text(assignee_name))
                .into_any_element()
        };
        let labels = if issue.labels.is_empty() {
            div()
                .text_size(TEXT_BODY)
                .text_color(t.fg_subtle)
                .child("None")
                .into_any_element()
        } else {
            h_flex()
                .flex_wrap()
                .gap(px(4.))
                .children(
                    issue
                        .labels
                        .iter()
                        .map(|l| label_pill(&l.name, l.color.as_deref(), &t)),
                )
                .into_any_element()
        };
        let url = issue.url.clone();

        v_flex()
            .w(px(DETAIL_WIDTH))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(t.border)
            .bg(t.surface)
            .child(
                h_flex()
                    .h(px(44.))
                    .flex_none()
                    .px(px(16.))
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(t.border)
                    .child(
                        div()
                            .flex_1()
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_subtle)
                            .child(issue.identifier.clone()),
                    )
                    .when(linear_editable, |el| {
                        el.child(self.linear_edit_button(
                            "detail-linear-edit",
                            "Edit issue",
                            issue,
                            Edit::Issue {
                                id: id.clone(),
                                fields: IssueFields::All,
                            },
                            cx,
                        ))
                    })
                    .when_some(url, |el, url| {
                        el.child(
                            Button::new("detail-open")
                                .icon(icon("external-link"))
                                .ghost()
                                .xsmall()
                                .tooltip("Open in Linear")
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    })
                    .child(
                        Button::new("detail-close")
                            .icon(icon("x"))
                            .ghost()
                            .xsmall()
                            .tooltip("Close")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |m, cx| m.open_issue(None, cx))
                            })),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("tasks-detail")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.detail_scroll)
                            .p(px(20.))
                            .gap(px(16.))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(t.fg)
                                    .child(issue.title.clone()),
                            )
                            .child(
                                v_flex()
                                    .gap(px(2.))
                                    .child(prop("Status", status))
                                    .child(prop("Priority", priority))
                                    .child(prop("Assignee", assignee))
                                    .child(prop("Project", project_value))
                                    .child(prop("Labels", labels))
                                    .when(issue.created_at > 0, |el| {
                                        el.child(prop(
                                            "Created",
                                            text(widgets::short_date(issue.created_at)),
                                        ))
                                    })
                                    .when(issue.updated_at > 0, |el| {
                                        el.child(prop(
                                            "Updated",
                                            text(widgets::short_date(issue.updated_at)),
                                        ))
                                    }),
                            )
                            .child(div().h(px(1.)).bg(t.border))
                            .child(if issue.description.trim().is_empty() {
                                div()
                                    .text_size(TEXT_BODY)
                                    .text_color(t.fg_subtle)
                                    .child("No description")
                                    .into_any_element()
                            } else {
                                div()
                                    .text_size(TEXT_BODY)
                                    .text_color(t.fg_muted)
                                    .child(issue.description.clone())
                                    .into_any_element()
                            })
                            .when(linear_editable, |el| {
                                el.child(
                                    h_flex()
                                        .flex_wrap()
                                        .gap(px(6.))
                                        .child(self.linear_edit_button(
                                            "detail-linear-labels",
                                            "Manage labels",
                                            issue,
                                            Edit::Labels { id: id.clone() },
                                            cx,
                                        ))
                                        .child(self.linear_edit_button(
                                            "detail-linear-archive",
                                            "Archive issue",
                                            issue,
                                            Edit::Archive { id: id.clone() },
                                            cx,
                                        )),
                                )
                            })
                            .when(source == Source::Linear, |el| {
                                el.child(self.render_linear_comments(issue, cx))
                            })
                            .when(editable, |el| {
                                el.child(
                                    Button::new("detail-delete")
                                        .label("Delete issue")
                                        .ghost()
                                        .xsmall()
                                        .text_color(t.danger)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let id = id.clone();
                                            this.model.update(cx, |m, cx| m.delete_issue(&id, cx));
                                        })),
                                )
                            }),
                    )
                    .vertical_scrollbar(&self.detail_scroll),
            )
            .into_any_element()
    }
}

fn centered(child: AnyElement) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p(px(24.))
        .child(child)
        .into_any_element()
}

fn label_pill(name: &str, color: Option<&str>, t: &Tokens) -> AnyElement {
    h_flex()
        .flex_none()
        .h(px(20.))
        .px(px(7.))
        .gap(px(5.))
        .rounded_full()
        .border_1()
        .border_color(t.border)
        .text_size(TEXT_CAPTION)
        .text_color(t.fg_muted)
        .child(
            div()
                .size(px(6.))
                .rounded_full()
                .bg(widgets::project_color(color, t)),
        )
        .child(name.to_string())
        .into_any_element()
}

fn render_issue_row(
    issue: &Issue,
    project: Option<&Project>,
    selected: bool,
    t: &Tokens,
    cx: &mut Context<TasksView>,
) -> AnyElement {
    let open = (issue.source, issue.id.clone());
    let hover = t.hover;
    h_flex()
        .id(SharedString::from(format!("tasks-issue-{}", issue.id)))
        .h(px(ISSUE_ROW))
        .px(px(16.))
        .gap(px(10.))
        .border_b_1()
        .border_color(t.border.alpha(0.6))
        .cursor_pointer()
        .when(selected, |el| el.bg(t.accent_soft))
        .when(!selected, |el| el.hover(move |s| s.bg(hover)))
        .child(widgets::priority_icon(issue.priority, px(14.), t))
        .child(
            div()
                .w(px(72.))
                .flex_none()
                .truncate()
                .text_size(TEXT_SMALL)
                .text_color(t.fg_subtle)
                .child(issue.identifier.clone()),
        )
        .child(widgets::status_icon(&issue.status, px(14.), t))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(TEXT_BODY)
                .text_color(t.fg)
                .child(issue.title.clone()),
        )
        .children(
            issue
                .labels
                .iter()
                .take(2)
                .map(|l| label_pill(&l.name, l.color.as_deref(), t)),
        )
        .when_some(project, |el, p| {
            el.child(
                h_flex()
                    .flex_none()
                    .max_w(px(150.))
                    .h(px(20.))
                    .px(px(7.))
                    .gap(px(5.))
                    .rounded_full()
                    .border_1()
                    .border_color(t.border)
                    .child(widgets::project_icon(p.color.as_deref(), px(11.), t))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(TEXT_CAPTION)
                            .text_color(t.fg_muted)
                            .child(p.name.clone()),
                    ),
            )
        })
        .child(
            div()
                .w(px(54.))
                .flex_none()
                .text_right()
                .text_size(TEXT_SMALL)
                .text_color(t.fg_subtle)
                .child(widgets::short_date(issue.updated_at)),
        )
        .child(widgets::avatar(issue.assignee.as_deref(), 18., t))
        .on_click(cx.listener(move |this, _, _, cx| {
            let open = open.clone();
            this.model.update(cx, |m, cx| m.open_issue(Some(open), cx));
        }))
        .into_any_element()
}

fn render_project_row(
    project: &Project,
    source: Source,
    t: &Tokens,
    cx: &mut Context<TasksView>,
) -> AnyElement {
    let selection = Selection {
        source,
        view: View::Project(project.id.clone()),
    };
    let hover = t.hover;
    let color = widgets::project_color(project.color.as_deref(), t);
    h_flex()
        .id(SharedString::from(format!("tasks-project-{}", project.id)))
        .h(px(PROJECT_ROW))
        .px(px(16.))
        .gap(px(12.))
        .border_b_1()
        .border_color(t.border.alpha(0.6))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(widgets::project_icon(project.color.as_deref(), px(16.), t))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(TEXT_BODY)
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.fg)
                .child(project.name.clone()),
        )
        .child(
            h_flex()
                .w(px(110.))
                .flex_none()
                .gap(px(6.))
                .text_size(TEXT_SMALL)
                .text_color(t.fg_muted)
                .child(
                    div()
                        .size(px(7.))
                        .rounded_full()
                        .bg(widgets::project_state_color(project.state, t)),
                )
                .child(project.state.label()),
        )
        .child(
            h_flex()
                .w(px(130.))
                .flex_none()
                .gap(px(6.))
                .child(widgets::avatar(project.lead.as_deref(), 18., t))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_muted)
                        .children(project.lead.clone()),
                ),
        )
        .child(
            div()
                .w(px(80.))
                .flex_none()
                .text_size(TEXT_SMALL)
                .text_color(t.fg_muted)
                .children(project.target_date.as_deref().map(widgets::target_date)),
        )
        .child(
            div()
                .w(px(90.))
                .flex_none()
                .child(widgets::progress(project.progress, color, t)),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            let selection = selection.clone();
            this.model.update(cx, |m, cx| m.select(selection, cx));
        }))
        .into_any_element()
}

fn project_columns(t: &Tokens) -> AnyElement {
    let col = |w: f32, label: &'static str| div().w(px(w)).flex_none().child(label);
    h_flex()
        .flex_none()
        .h(px(30.))
        .px(px(16.))
        .gap(px(12.))
        .border_b_1()
        .border_color(t.border)
        .text_size(TEXT_CAPTION)
        .text_color(t.fg_subtle)
        .child(div().w(px(16.)))
        .child(div().flex_1().child("Name"))
        .child(col(110., "Status"))
        .child(col(130., "Lead"))
        .child(col(80., "Target"))
        .child(col(90., "Progress"))
        .into_any_element()
}

impl Focusable for TasksView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TasksView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_rows(cx);
        let t = tokens(cx).clone();
        let header = self.render_header(cx);
        let model = self.model.read(cx);
        let selection = model.selection.clone();
        let needs_key = selection.source == Source::Linear && !model.linear.has_key;
        let needs_upgrade = selection.source == Source::Linear
            && model.linear.has_key
            && !model.linear.write_authorized;
        if model.linear.write_authorized {
            self.reconnecting_linear = false;
        }
        let upgrading = needs_upgrade && self.reconnecting_linear;
        let projects_view = selection.view == View::Projects;
        let summary = match &selection.view {
            View::Project(id) => model.project(selection.source, id).cloned(),
            _ => None,
        };
        let detail = model
            .open_issue
            .as_ref()
            .filter(|(source, _)| *source == selection.source)
            .and_then(|(source, id)| {
                model
                    .issue(*source, id)
                    .cloned()
                    .map(|issue| (issue, *source))
            });
        let local_error = model
            .local_error
            .clone()
            .filter(|_| selection.source == Source::Local);

        let list = if needs_key || upgrading {
            self.render_connect(upgrading, cx)
        } else {
            let composer = self.composer.as_ref().map(|c| self.render_composer(c, cx));
            let body = if self.rows.is_empty() {
                self.render_empty(cx)
            } else {
                uniform_list(
                    "tasks-rows",
                    self.rows.len(),
                    cx.processor(Self::render_rows),
                )
                .track_scroll(self.scroll.clone())
                .flex_1()
                .into_any_element()
            };
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .when_some(summary.as_ref(), |el, project| {
                    el.child(self.render_project_summary(project, cx))
                })
                .when(projects_view && !self.rows.is_empty(), |el| {
                    el.child(project_columns(&t))
                })
                .children(composer)
                .when_some(local_error, |el, error| {
                    el.child(
                        div()
                            .px(px(16.))
                            .py(px(6.))
                            .text_size(TEXT_SMALL)
                            .text_color(t.danger)
                            .child(error),
                    )
                })
                .child(body)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(t.surface)
            .text_color(t.fg)
            .child(header)
            .when(needs_upgrade && !upgrading, |el| {
                el.child(
                    h_flex()
                        .px(px(16.))
                        .py(px(6.))
                        .gap(px(10.))
                        .border_b_1()
                        .border_color(t.border)
                        .child(
                            div()
                                .text_size(TEXT_SMALL)
                                .text_color(t.fg_muted)
                                .child("Reconnect Linear to assign and edit issues."),
                        )
                        .child(
                            Button::new("tasks-upgrade-linear")
                                .label("Enable editing")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.reconnecting_linear = true;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .when_some(detail.filter(|_| !upgrading), |el, (issue, source)| {
                        el.child(self.render_detail(&issue, source, cx))
                    }),
            )
    }
}
