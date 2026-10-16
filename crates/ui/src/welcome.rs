//! The window shown when no folder is open: recent folders and Open Folder.

use std::path::PathBuf;

use cockpit_persistence::WorkspaceRecord;
use gpui::{div, prelude::*, px, App, Context, FocusHandle, Focusable, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, ActiveTheme as _, IconName, Root,
};

use crate::{assets::icon, state::AppState};

pub struct Welcome {
    recent: Vec<WorkspaceRecord>,
    focus_handle: FocusHandle,
}

impl Focusable for Welcome {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Welcome {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let recent = AppState::global(cx)
            .db
            .read_blocking(|r| r.recent_workspaces(12))
            .unwrap_or_default()
            .into_iter()
            .filter(|w| PathBuf::from(&w.path).is_dir())
            .collect();
        Self { recent, focus_handle: cx.focus_handle() }
    }

    fn open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        crate::open_folder(path, cx);
        window.remove_window();
    }
}

impl Render for Welcome {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notifications = Root::render_notification_layer(window, cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let home = dirs_home();
        let mut recent = v_flex().gap_1();
        for (ix, workspace) in self.recent.iter().enumerate() {
            let path = PathBuf::from(&workspace.path);
            let shown: SharedString = match home.as_ref().and_then(|h| path.strip_prefix(h).ok()) {
                Some(rest) => format!("~/{}", rest.display()).into(),
                None => workspace.path.clone().into(),
            };
            recent = recent.child(
                h_flex()
                    .id(("recent", ix))
                    .px_2()
                    .py_1()
                    .gap_3()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|el| el.bg(theme.list_hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.open(path.clone(), window, cx)))
                    .child(div().text_sm().child(workspace.name.clone()))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(shown)),
            );
        }

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(theme.background)
            .text_color(theme.foreground)
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(460.))
                    .gap_6()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(icon("code-xml").size(px(28.)))
                                    .child(div().text_2xl().child(cockpit_common::APP_NAME)),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(muted)
                                    .child("Code, a real Chromium browser and Codex, in one native window."),
                            ),
                    )
                    .child(
                        h_flex().gap_2().child(
                            Button::new("welcome-open")
                                .icon(IconName::FolderOpen)
                                .label("Open Folder…")
                                .primary()
                                .on_click(|_, _, cx| crate::prompt_open_folder(cx)),
                        ),
                    )
                    .when(!self.recent.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .gap_2()
                                .child(div().text_xs().text_color(muted).child("RECENT"))
                                .child(recent),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Tip: run `cockpit <folder>` from a terminal to open a folder directly."),
                    ),
            )
            .child(div().absolute().bottom_2().text_xs().text_color(muted).child(format!(
                "Version {}",
                cockpit_common::APP_VERSION
            )))
            .children(notifications)
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
