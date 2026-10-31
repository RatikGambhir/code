//! Application-wide state, shared by every window.

use std::{path::PathBuf, rc::Rc, sync::Arc};

use cockpit_agents::{AgentRuntime, ClaudeLaunch, CodexLaunch, Provider};
use cockpit_browser::BrowserEngine;
use cockpit_common::AppPaths;
use cockpit_persistence::DbHandle;
use gpui::{App, Global};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

/// User preferences, stored as JSON under the `app_settings` key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemePreference,
    /// The accent everything is tinted by, as `#rrggbb`.
    pub accent: String,
    /// How strongly the window frame takes the accent, 0 to 1.
    pub tint: f32,
    /// Monospace face for code, file names and the terminal; the first
    /// installed of a few good ones when unset.
    pub mono_font: Option<String>,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
    /// Which agent the AI panel talks to.
    pub agent_provider: Provider,
    /// Overrides where `codex` is looked for.
    pub codex_path: Option<String>,
    /// Codex's model and effort for new turns (`None` = its default).
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    /// Overrides where `claude` is looked for.
    pub claude_path: Option<String>,
    /// Claude's model and effort for new turns (`None` = Claude Opus 5.5 at
    /// its default effort).
    pub claude_model: Option<String>,
    pub claude_effort: Option<String>,
    /// Offer the embedded browser to the agent as tools.
    pub agent_browser_tools: bool,
    pub search_url: String,
    pub home_page: String,
    pub restore_tabs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemePreference::System,
            accent: crate::theme::DEFAULT_ACCENT.into(),
            tint: crate::theme::DEFAULT_TINT,
            mono_font: None,
            editor_font_size: 13.0,
            terminal_font_size: 13.0,
            agent_provider: Provider::OpenAi,
            codex_path: None,
            default_model: None,
            default_effort: None,
            claude_path: None,
            claude_model: None,
            claude_effort: None,
            agent_browser_tools: true,
            search_url: cockpit_browser::address::DEFAULT_SEARCH_URL.into(),
            home_page: "about:blank".into(),
            restore_tabs: true,
        }
    }
}

pub const SETTINGS_KEY: &str = "app_settings";

pub struct AppState {
    pub paths: AppPaths,
    pub db: DbHandle,
    pub browser: Rc<dyn BrowserEngine>,
    pub agents: Arc<AgentRuntime>,
    pub settings: Settings,
    /// Profile directory Chromium uses for cookies and storage.
    pub browser_profile: PathBuf,
    /// The profile's row, for history.
    pub browser_profile_id: Option<i64>,
}

impl Global for AppState {}

impl AppState {
    pub fn global(cx: &App) -> &AppState {
        cx.global::<AppState>()
    }

    /// Where to find `codex`: the configured path, else discovery.
    pub fn codex_launch(&self) -> Option<CodexLaunch> {
        match &self.settings.codex_path {
            Some(path) if !path.is_empty() => Some(CodexLaunch::new(path)),
            _ => CodexLaunch::discover(),
        }
    }

    /// Where to find `claude`: the configured path, else discovery.
    pub fn claude_launch(&self) -> Option<ClaudeLaunch> {
        match &self.settings.claude_path {
            Some(path) if !path.is_empty() => Some(ClaudeLaunch::new(path)),
            _ => ClaudeLaunch::discover(),
        }
    }

    pub fn save_settings(cx: &mut App, update: impl FnOnce(&mut Settings)) {
        let state = cx.global_mut::<AppState>();
        update(&mut state.settings);
        let settings = state.settings.clone();
        state
            .db
            .write_detached("settings", move |w| w.set_setting_json(SETTINGS_KEY, &settings));
    }
}
