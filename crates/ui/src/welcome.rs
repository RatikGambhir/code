//! The window shown when no folder is open: recent folders and Open Folder.

use std::path::PathBuf;

use cockpit_persistence::WorkspaceRecord;
use gpui::{div, prelude::*, px, App, Context, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, IconName, Root, TitleBar,
};

use crate::{
    chrome::{self, caps_label},
    state::AppState,
    theme::{self, tokens, TEXT_BODY, TEXT_SMALL},
};

pub struct Welcome {
    recent: Vec<WorkspaceRecord>,
    focus_handle: FocusHandle,
    _appearance: Option<Subscription>,
}

impl Focusable for Welcome {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Welcome {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let recent = AppState::global(cx)
            .db
            .read_blocking(|r| r.recent_workspaces(12))
            .unwrap_or_default()
            .into_iter()
            .filter(|w| PathBuf::from(&w.path).is_dir())
            .collect();
        Self {
            recent,
            focus_handle: cx.focus_handle(),
            _appearance: Some(theme::follow_system_appearance(window, cx)),
        }
    }

    fn open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        crate::open_folder(path, cx);
        window.remove_window();
    }
}

impl Render for Welcome {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notifications = Root::render_notification_layer(window, cx);
        let t = tokens(cx).clone();
        let home = dirs_home();
        let mut recent = v_flex();
        for (ix, workspace) in self.recent.iter().enumerate() {
            let path = PathBuf::from(&workspace.path);
            let shown: SharedString = match home.as_ref().and_then(|h| path.strip_prefix(h).ok()) {
                Some(rest) => format!("~/{}", rest.display()).into(),
                None => workspace.path.clone().into(),
            };
            recent = recent.child(
                chrome::list_row(("recent", ix), false, cx)
                    .h(px(30.))
                    .gap(px(12.))
                    .on_click(cx.listener(move |this, _, window, cx| this.open(path.clone(), window, cx)))
                    .child(div().flex_none().text_size(TEXT_BODY).child(workspace.name.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(t.mono.clone())
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_subtle)
                            .child(shown),
                    ),
            );
        }

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(t.frame)
            .text_color(t.fg)
            .child(TitleBar::new())
            .child(
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .pb(px(36.))
                    .child(
                        v_flex()
                            .w(px(440.))
                            .p(px(28.))
                            .gap(px(22.))
                            .rounded(px(14.))
                            .border_1()
                            .border_color(t.border)
                            .bg(t.surface)
                            .when(!t.dark, |el| el.shadow_sm())
                            .child(
                                v_flex()
                                    .gap(px(10.))
                                    .child(chrome::logo_mark(26., cx))
                                    .child(
                                        div()
                                            .mt(px(6.))
                                            .text_size(px(20.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(cockpit_common::APP_NAME),
                                    )
                                    .child(
                                        div()
                                            .text_size(TEXT_BODY)
                                            .text_color(t.fg_muted)
                                            .child("Code, a real Chromium browser and Codex, in one native window."),
                                    ),
                            )
                            .child(
                                h_flex().child(
                                    Button::new("welcome-open")
                                        .icon(IconName::FolderOpen)
                                        .label("Open folder…")
                                        .primary()
                                        .on_click(|_, _, cx| crate::prompt_open_folder(cx)),
                                ),
                            )
                            .when(!self.recent.is_empty(), |el| {
                                el.child(
                                    v_flex()
                                        .gap(px(6.))
                                        .child(div().px(px(8.)).child(caps_label("Recent", cx)))
                                        .child(div().mx(px(-8.)).child(recent)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .mt(px(16.))
                            .text_size(TEXT_SMALL)
                            .text_color(t.frame_muted)
                            .child(SharedString::from(format!(
                                "Version {} · drop a folder on the Dock icon, or run `open -a Cockpit <folder>`",
                                cockpit_common::APP_VERSION
                            ))),
                    ),
            )
            .children(notifications)
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
