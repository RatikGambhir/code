//! The GitHub sidebar: the signed-in account and a filterable list of the
//! account's repositories.

use std::ops::Range;

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, App, Context, Entity, FocusHandle, Focusable,
    Subscription, UniformListScrollHandle, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex, Icon, IconName, Sizable as _,
};

use super::model::{AuthState, GitHubModel};
use crate::{
    assets::icon,
    chrome::{self, panel_header},
    theme::{tokens, TEXT_BODY, TEXT_SMALL},
};

const ROW: f32 = 28.;

/// A repository row's data, copied out of the model for drawing.
struct RepoRow {
    full_name: String,
    name: String,
    /// Shown when the repository is not the viewer's own.
    owner: Option<String>,
    private: bool,
    archived: bool,
    selected: bool,
}

pub struct GitHubSidebar {
    model: Entity<GitHubModel>,
    filter: Entity<InputState>,
    /// Indices into the model's repositories that pass the filter.
    visible: Vec<usize>,
    /// The model revision and filter `visible` was built for.
    visible_for: Option<(u64, String)>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl GitHubSidebar {
    pub fn new(model: Entity<GitHubModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Find a repository…"));
        let subscriptions = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        Self {
            model,
            filter,
            visible: Vec::new(),
            visible_for: None,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn ensure_visible(&mut self, cx: &App) {
        let query = self.filter.read(cx).value().trim().to_lowercase();
        let model = self.model.read(cx);
        let key = (model.revision(), query);
        if self.visible_for.as_ref() == Some(&key) {
            return;
        }
        self.visible = model
            .repos
            .ready()
            .map(|repos| {
                repos
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| key.1.is_empty() || r.full_name.to_lowercase().contains(&key.1))
                    .map(|(ix, _)| ix)
                    .collect()
            })
            .unwrap_or_default();
        self.visible_for = Some(key);
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = tokens(cx).clone();
        let model = self.model.read(cx);
        let viewer = model.viewer().map(|u| u.login.clone()).unwrap_or_default();
        let selected = model.repo.as_ref().map(|r| r.repo.full_name.clone());
        let Some(repos) = model.repos.ready() else {
            return Vec::new();
        };
        let rows: Vec<RepoRow> = self.visible[range.start.min(self.visible.len())..range.end.min(self.visible.len())]
            .iter()
            .filter_map(|&ix| repos.get(ix))
            .map(|r| RepoRow {
                full_name: r.full_name.clone(),
                name: r.name.clone(),
                owner: (r.owner != viewer).then(|| r.owner.clone()),
                private: r.private,
                archived: r.archived,
                selected: selected.as_deref() == Some(r.full_name.as_str()),
            })
            .collect();
        rows.into_iter()
            .map(|row| {
                let mark = if row.private {
                    icon("lock")
                } else {
                    icon("book-marked")
                };
                let full_name = row.full_name.clone();
                chrome::list_row(gpui::SharedString::from(format!("gh-repo-{}", row.full_name)), row.selected, cx)
                    .h(px(ROW))
                    .child(mark.size(px(13.)).text_color(t.fg_subtle))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(4.))
                            .text_size(TEXT_BODY)
                            .when_some(row.owner, |el, owner| {
                                el.child(div().flex_none().text_color(t.fg_subtle).child(format!("{owner} /")))
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(if row.selected { t.fg } else { t.fg_muted })
                                    .child(row.name),
                            ),
                    )
                    .when(row.archived, |el| {
                        el.child(div().flex_none().text_size(TEXT_SMALL).text_color(t.fg_subtle).child("archived"))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let name = full_name.clone();
                        this.model.update(cx, |m, cx| m.select_repo(&name, cx));
                    }))
                    .into_any_element()
            })
            .collect()
    }
}

impl Focusable for GitHubSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitHubSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_visible(cx);
        let t = tokens(cx).clone();
        let model = self.model.read(cx);
        let signed_in = model.viewer().cloned();
        let checking = matches!(model.auth, AuthState::Checking | AuthState::Verifying);
        let loading = model.repos.is_loading() || model.refreshing_repos;
        let error = model.error.clone().filter(|_| signed_in.is_some());
        let total = model.repos.ready().map_or(0, Vec::len);

        let header = panel_header("GitHub", cx)
            .when(loading || checking, |el| el.child(Spinner::new().xsmall()))
            .when(signed_in.is_some() && !loading, |el| {
                el.child(
                    Button::new("gh-refresh-repos")
                        .icon(icon("refresh-cw"))
                        .ghost()
                        .xsmall()
                        .tooltip("Reload repositories")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |m, cx| m.refresh_repos(cx))
                        })),
                )
            });

        let Some(user) = signed_in else {
            return v_flex()
                .size_full()
                .track_focus(&self.focus_handle)
                .child(header)
                .child(
                    div().px(px(6.)).child(
                        chrome::list_row("gh-sign-in", true, cx)
                            .h(px(ROW))
                            .child(Icon::new(IconName::GitHub).size(px(14.)).text_color(t.fg_muted))
                            .child(div().text_size(TEXT_BODY).text_color(t.fg_muted).child(if checking {
                                "Checking sign-in…"
                            } else {
                                "Sign in to GitHub"
                            })),
                    ),
                )
                .into_any_element();
        };

        let account = h_flex()
            .flex_none()
            .mx(px(6.))
            .px(px(8.))
            .h(px(36.))
            .gap(px(8.))
            .child(crate::tasks::avatar(Some(user.name.as_deref().unwrap_or(&user.login)), 20., &t))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_size(TEXT_BODY).text_color(t.fg).child(user.login.clone()))
                    .when_some(user.name.clone().filter(|n| *n != user.login), |el, name| {
                        el.child(div().truncate().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(name))
                    }),
            )
            .child(
                Button::new("gh-sign-out")
                    .icon(icon("x"))
                    .ghost()
                    .xsmall()
                    .tooltip("Sign out of GitHub")
                    .on_click(cx.listener(|this, _, _, cx| this.model.update(cx, |m, cx| m.sign_out(cx)))),
            );

        let list = if self.visible.is_empty() {
            div()
                .px(px(14.))
                .py(px(8.))
                .text_size(TEXT_SMALL)
                .text_color(t.fg_subtle)
                .child(if loading {
                    "Loading repositories…"
                } else if total == 0 {
                    "No repositories"
                } else {
                    "No matches"
                })
                .into_any_element()
        } else {
            uniform_list("gh-repos", self.visible.len(), cx.processor(Self::render_rows))
                .track_scroll(self.scroll.clone())
                .px(px(6.))
                .flex_1()
                .into_any_element()
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(header)
            .child(account)
            .child(div().flex_none().px(px(10.)).pb(px(6.)).child(
                Input::new(&self.filter).small().prefix(Icon::new(IconName::Search).size(px(12.))),
            ))
            .child(
                h_flex()
                    .flex_none()
                    .px(px(14.))
                    .h(px(22.))
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_subtle)
                    .child(format!("Repositories · {total}")),
            )
            .when_some(error, |el, error| {
                el.child(div().px(px(14.)).py(px(4.)).text_size(TEXT_SMALL).text_color(t.danger).child(error))
            })
            .child(list)
            .into_any_element()
    }
}
