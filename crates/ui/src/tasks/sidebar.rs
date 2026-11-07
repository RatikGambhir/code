//! The Tasks sidebar: this workspace's views and projects, then Linear's.

use cockpit_tasks::{ProjectState, Source};
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, FocusHandle, Focusable, ScrollHandle,
    SharedString, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    spinner::Spinner,
    v_flex, Icon, IconName, Sizable as _,
};

use super::{
    model::{Selection, TasksModel, View},
    widgets,
};
use crate::{
    assets::icon,
    chrome::{self, panel_header},
    theme::{tokens, TEXT_BODY, TEXT_CAPTION, TEXT_SMALL},
};

pub struct TasksSidebar {
    model: Entity<TasksModel>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl TasksSidebar {
    pub fn new(model: Entity<TasksModel>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe(&model, |_, _, cx| cx.notify())];
        Self {
            model,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn row(
        &self,
        id: impl Into<SharedString>,
        selection: Selection,
        leading: Icon,
        label: impl Into<SharedString>,
        count: Option<usize>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let t = tokens(cx).clone();
        let selected = self.model.read(cx).selection == selection;
        chrome::list_row(id.into(), selected, cx)
            .h(px(28.))
            .child(leading)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(TEXT_BODY)
                    .text_color(if selected { t.fg } else { t.fg_muted })
                    .child(label.into()),
            )
            .when_some(count.filter(|c| *c > 0), |el, count| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_subtle)
                        .child(count.to_string()),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model
                    .update(cx, |m, cx| m.select(selection.clone(), cx));
            }))
    }

    fn section_label(label: impl Into<SharedString>, cx: &App) -> gpui::Div {
        let t = tokens(cx);
        h_flex()
            .h(px(28.))
            .mt(px(10.))
            .px(px(8.))
            .gap(px(6.))
            .text_size(TEXT_CAPTION)
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(t.fg_subtle)
            .child(div().flex_1().min_w_0().truncate().child(label.into()))
    }

    /// The source's view rows and its open projects.
    fn source_rows(&self, source: Source, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = tokens(cx).clone();
        let model = self.model.read(cx);
        let board = model.board(source);
        let open = board
            .issues
            .iter()
            .filter(|i| !i.status.kind.is_closed())
            .count();
        let mine = board
            .issues
            .iter()
            .filter(|i| i.mine && !i.status.kind.is_closed())
            .count();
        let projects: Vec<(String, String, Option<String>, usize)> = board
            .projects
            .iter()
            .filter(|p| !matches!(p.state, ProjectState::Completed | ProjectState::Canceled))
            .map(|p| {
                let count = board
                    .issues
                    .iter()
                    .filter(|i| {
                        i.project_id.as_deref() == Some(p.id.as_str()) && !i.status.kind.is_closed()
                    })
                    .count();
                (p.id.clone(), p.name.clone(), p.color.clone(), count)
            })
            .collect();
        let key = match source {
            Source::Local => "local",
            Source::Linear => "linear",
        };

        let mut rows = Vec::new();
        if source == Source::Linear {
            rows.push(
                self.row(
                    "tasks-linear-mine",
                    Selection {
                        source,
                        view: View::MyIssues,
                    },
                    Icon::new(IconName::Inbox)
                        .size(px(14.))
                        .text_color(t.fg_muted),
                    "My issues",
                    Some(mine),
                    cx,
                )
                .into_any_element(),
            );
        }
        rows.push(
            self.row(
                format!("tasks-{key}-issues"),
                Selection {
                    source,
                    view: View::Issues,
                },
                icon("list-todo").size(px(14.)).text_color(t.fg_muted),
                if source == Source::Linear {
                    "All issues"
                } else {
                    "Issues"
                },
                Some(open),
                cx,
            )
            .into_any_element(),
        );
        rows.push(
            self.row(
                format!("tasks-{key}-projects"),
                Selection {
                    source,
                    view: View::Projects,
                },
                icon("project").size(px(14.)).text_color(t.fg_muted),
                "Projects",
                None,
                cx,
            )
            .into_any_element(),
        );
        for (id, name, color, count) in projects {
            rows.push(
                self.row(
                    format!("tasks-{key}-project-{id}"),
                    Selection {
                        source,
                        view: View::Project(id),
                    },
                    widgets::project_icon(color.as_deref(), px(13.), &t),
                    name,
                    Some(count),
                    cx,
                )
                .pl(px(20.))
                .into_any_element(),
            );
        }
        rows
    }
}

impl Focusable for TasksSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TasksSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let model = self.model.read(cx);
        let workspace = model.workspace_name();
        let linear = &model.linear;
        let (has_key, loading, error) = (
            linear.has_key,
            linear.loading || linear.connecting,
            linear.error.clone(),
        );
        let org = linear.viewer.as_ref().and_then(|v| v.organization.clone());
        let connect_selected = !has_key && model.selection.source == Source::Linear;

        let mut list = v_flex().px(px(6.)).pb(px(12.)).gap(px(1.));
        list = list.child(Self::section_label(workspace, cx));
        list = list.children(self.source_rows(Source::Local, cx));

        let linear_label = match org {
            Some(org) => format!("Linear · {org}"),
            None => "Linear".to_string(),
        };
        list = list.child(
            Self::section_label(linear_label, cx)
                .when(loading, |el| el.child(Spinner::new().xsmall()))
                .when(has_key && !loading, |el| {
                    el.child(
                        Button::new("tasks-linear-refresh")
                            .icon(icon("refresh-cw"))
                            .ghost()
                            .xsmall()
                            .tooltip("Pull from Linear")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |m, cx| m.refresh_linear(cx))
                            })),
                    )
                }),
        );
        if has_key {
            list = list.children(self.source_rows(Source::Linear, cx));
        } else {
            list = list.child(
                chrome::list_row("tasks-linear-connect", connect_selected, cx)
                    .h(px(28.))
                    .child(
                        Icon::new(IconName::Plus)
                            .size(px(14.))
                            .text_color(t.fg_muted),
                    )
                    .child(
                        div()
                            .text_size(TEXT_BODY)
                            .text_color(t.fg_muted)
                            .child("Connect Linear"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model.update(cx, |m, cx| {
                            m.select(
                                Selection {
                                    source: Source::Linear,
                                    view: View::Issues,
                                },
                                cx,
                            )
                        });
                    })),
            );
        }
        if let Some(error) = error.filter(|_| has_key) {
            list = list.child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(TEXT_SMALL)
                    .text_color(t.danger)
                    .child(error),
            );
        }

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(panel_header("Tasks", cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("tasks-sidebar")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .child(list),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
    }
}
