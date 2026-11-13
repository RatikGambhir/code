//! Shared dialogs for writing changes to connected services.

use std::{collections::HashMap, sync::Arc};

use anyhow::{ensure, Result};
use gpui::{div, prelude::*, px, App, Context, Entity, Render, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    select::{SearchableVec, Select, SelectItem, SelectState},
    v_flex, Disableable as _, IndexPath, WindowExt as _,
};

use crate::theme::{tokens, TEXT_BODY, TEXT_SMALL};

#[derive(Clone)]
pub struct Choice {
    pub id: String,
    label: SharedString,
}

impl Choice {
    pub fn new(id: impl Into<String>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

impl SelectItem for Choice {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }
}

pub enum Field {
    Text(&'static str, &'static str, String),
    Area(&'static str, &'static str, String),
    Select(&'static str, &'static str, Option<String>, Vec<Choice>),
    Hidden(&'static str, String),
    Note(String),
}

enum Control {
    Text(Entity<InputState>),
    Select(Entity<SelectState<SearchableVec<Choice>>>),
    Hidden,
    Note,
}

struct FormField {
    key: &'static str,
    label: SharedString,
    initial: String,
    control: Control,
}

impl FormField {
    fn new(field: Field, window: &mut Window, cx: &mut App) -> Self {
        let multiline = matches!(&field, Field::Area(..));
        let (key, label, initial, control) = match field {
            Field::Text(key, label, text) | Field::Area(key, label, text) => {
                let input = cx.new(|cx| {
                    let input = InputState::new(window, cx).default_value(text.clone());
                    if multiline {
                        input
                            .multi_line(true)
                            .rows(if key == "content" { 10 } else { 4 })
                    } else {
                        input
                    }
                });
                (key, label, text, Control::Text(input))
            }
            Field::Select(key, label, selected, items) => {
                let index = selected
                    .as_ref()
                    .and_then(|id| items.iter().position(|item| &item.id == id))
                    .map(IndexPath::new);
                let select = cx.new(|cx| {
                    SelectState::new(SearchableVec::new(items), index, window, cx).searchable(true)
                });
                (
                    key,
                    label,
                    selected.unwrap_or_default(),
                    Control::Select(select),
                )
            }
            Field::Hidden(key, value) => (key, "", value, Control::Hidden),
            Field::Note(text) => ("", "", text, Control::Note),
        };
        Self {
            key,
            label: label.into(),
            initial,
            control,
        }
    }

    fn value(&self, cx: &App) -> String {
        match &self.control {
            Control::Text(input) => input.read(cx).value().to_string(),
            Control::Select(select) => select
                .read(cx)
                .selected_value()
                .cloned()
                .unwrap_or_default(),
            Control::Hidden | Control::Note => self.initial.clone(),
        }
    }
}

pub struct Values(HashMap<&'static str, (String, String)>);

impl Values {
    pub fn get(&self, key: &str) -> &str {
        self.0.get(key).map_or("", |(value, _)| value)
    }

    pub fn required(&self, key: &str, label: &str) -> Result<&str> {
        let value = self.get(key).trim();
        ensure!(!value.is_empty(), "Enter {label}.");
        Ok(value)
    }

    pub fn changed(&self, key: &str) -> bool {
        self.0
            .get(key)
            .is_some_and(|(value, initial)| value != initial)
    }
}

pub enum Fields {
    Ready(Vec<Field>),
    Load(Box<LoadFields>),
}

type LoadFields = dyn Fn() -> Result<Vec<Field>> + Send + Sync;
type Save<T> = dyn Fn(Values) -> Result<T> + Send + Sync;
type OnSaved<T> = dyn Fn(T, &mut App);

struct Form<T> {
    fields: Vec<FormField>,
    loader: Option<Arc<LoadFields>>,
    save: Arc<Save<T>>,
    on_saved: Box<OnSaved<T>>,
    submit_label: &'static str,
    loading: bool,
    saving: bool,
    error: Option<String>,
}

pub fn open<T: Send + 'static>(
    title: impl Into<SharedString>,
    submit_label: &'static str,
    fields: Fields,
    save: impl Fn(Values) -> Result<T> + Send + Sync + 'static,
    on_saved: impl Fn(T, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let title = title.into();
    let form = cx.new(|cx| {
        let (fields, loader) = match fields {
            Fields::Ready(fields) => (
                fields
                    .into_iter()
                    .map(|field| FormField::new(field, window, cx))
                    .collect(),
                None,
            ),
            Fields::Load(load) => (Vec::new(), Some(load.into())),
        };
        let mut form = Form {
            fields,
            loader,
            save: Arc::new(save),
            on_saved: Box::new(on_saved),
            submit_label,
            loading: false,
            saving: false,
            error: None,
        };
        form.load(window, cx);
        form
    });
    window.open_dialog(cx, move |dialog, _, _| {
        let state = form.clone();
        dialog
            .title(title.clone())
            .w(px(540.))
            .overlay_closable(false)
            .close_button(false)
            .on_cancel(move |_, _, cx| !state.read(cx).saving)
            .child(form.clone())
    });
}

impl<T: Send + 'static> Form<T> {
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(loader) = self.loader.clone() else {
            return;
        };
        self.loading = true;
        self.error = None;
        let task = cx.background_spawn(async move { loader() });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    this.loading = false;
                    match result {
                        Ok(fields) => {
                            this.fields = fields
                                .into_iter()
                                .map(|field| FormField::new(field, window, cx))
                                .collect();
                            this.loader = None;
                        }
                        Err(err) => this.error = Some(format!("{err:#}")),
                    }
                    cx.notify();
                })
                .ok();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.loader.is_some() {
            return;
        }
        let values = Values(
            self.fields
                .iter()
                .filter(|field| !matches!(field.control, Control::Note))
                .map(|field| (field.key, (field.value(cx), field.initial.clone())))
                .collect(),
        );
        let save = self.save.clone();
        self.saving = true;
        self.error = None;
        let task = cx.background_spawn(async move { save(values) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            cx.update(|window, cx| {
                this.update(cx, |this, cx| match result {
                    Ok(value) => {
                        (this.on_saved)(value, cx);
                        window.close_dialog(cx);
                    }
                    Err(err) => {
                        this.saving = false;
                        this.error = Some(format!("{err:#}"));
                        cx.notify();
                    }
                })
                .ok();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

impl<T: Send + 'static> Render for Form<T> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        v_flex()
            .gap(px(12.))
            .child(
                v_flex()
                    .id("integration-form-fields")
                    .max_h(window.viewport_size().height * 0.53)
                    .overflow_y_scroll()
                    .gap(px(10.))
                    .when(self.loading, |el| el.child("Loading options…"))
                    .children(self.fields.iter().filter_map(|field| {
                        let control = match &field.control {
                            Control::Text(input) => {
                                Input::new(input).disabled(self.saving).into_any_element()
                            }
                            Control::Select(select) => Select::new(select)
                                .disabled(self.saving)
                                .w_full()
                                .into_any_element(),
                            Control::Note => {
                                return Some(
                                    div()
                                        .text_size(TEXT_BODY)
                                        .child(field.initial.clone())
                                        .into_any_element(),
                                )
                            }
                            Control::Hidden => return None,
                        };
                        Some(
                            v_flex()
                                .gap(px(4.))
                                .child(
                                    div()
                                        .text_size(TEXT_SMALL)
                                        .text_color(t.fg_muted)
                                        .child(field.label.clone()),
                                )
                                .child(control)
                                .into_any_element(),
                        )
                    })),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_color(t.danger)
                        .text_size(TEXT_SMALL)
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("integration-form-cancel")
                            .label("Cancel")
                            .disabled(self.saving)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("integration-form-submit")
                            .label(if self.loader.is_some() {
                                "Retry"
                            } else {
                                self.submit_label
                            })
                            .primary()
                            .loading(self.saving)
                            .disabled(self.loading || self.saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.loader.is_some() {
                                    this.load(window, cx);
                                } else {
                                    this.submit(window, cx);
                                }
                            })),
                    ),
            )
    }
}
