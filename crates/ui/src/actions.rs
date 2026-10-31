//! Commands and their default key bindings.

use gpui::{actions, App, KeyBinding};

actions!(
    cockpit,
    [
        OpenFolder,
        Save,
        CloseTab,
        NextTab,
        PreviousTab,
        SplitPane,
        NewBrowserTab,
        NewTerminal,
        ToggleSidebar,
        ToggleTerminal,
        ToggleAgentPanel,
        ShowExplorer,
        ShowSourceControl,
        ShowSearch,
        CommandPalette,
        QuickOpen,
        FocusAgent,
        NewAgentThread,
        InterruptAgent,
        ToggleTheme,
        CustomizeTheme,
        NavigateBack,
        NavigateForward,
        RestartLanguageServers,
        About,
        Quit,
    ]
);

actions!(
    browser,
    [
        FocusAddressBar,
        Reload,
        GoBack,
        GoForward,
        ShowDevTools,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        Copy,
        Cut,
        Paste,
        SelectAll,
        Undo,
        Redo,
        PageTab,
        PageBackTab,
    ]
);

actions!(editor, [GoToDefinition, FindReferences]);

actions!(
    terminal,
    [TerminalPaste, TerminalClear, TerminalCopyScreen, TerminalTab, TerminalBackTab]
);

/// Key context of the browser view.
pub const BROWSER_CONTEXT: &str = "BrowserView";
/// Key context of a terminal.
pub const TERMINAL_CONTEXT: &str = "Terminal";
/// Key context of a code editor tab.
pub const CODE_EDITOR_CONTEXT: &str = "CodeEditor";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(key_bindings());
}

fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-o", OpenFolder, None),
        KeyBinding::new("cmd-s", Save, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("cmd-shift-]", NextTab, None),
        KeyBinding::new("cmd-shift-[", PreviousTab, None),
        KeyBinding::new("cmd-\\", SplitPane, None),
        KeyBinding::new("cmd-t", NewBrowserTab, None),
        KeyBinding::new("ctrl-`", NewTerminal, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-j", ToggleTerminal, None),
        KeyBinding::new("cmd-alt-b", ToggleAgentPanel, None),
        KeyBinding::new("cmd-shift-e", ShowExplorer, None),
        KeyBinding::new("cmd-shift-g", ShowSourceControl, None),
        KeyBinding::new("cmd-shift-f", ShowSearch, None),
        KeyBinding::new("cmd-shift-p", CommandPalette, None),
        KeyBinding::new("cmd-p", QuickOpen, None),
        KeyBinding::new("cmd-l", FocusAgent, None),
        KeyBinding::new("cmd-shift-n", NewAgentThread, None),
        KeyBinding::new("ctrl-c", InterruptAgent, Some("AgentComposer")),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl--", NavigateBack, None),
        KeyBinding::new("ctrl-shift--", NavigateForward, None),
        // Code editor (⌘-click and ⌘. come from the editor itself)
        KeyBinding::new("f12", GoToDefinition, Some(CODE_EDITOR_CONTEXT)),
        KeyBinding::new("shift-f12", FindReferences, Some(CODE_EDITOR_CONTEXT)),
        // The agent composer sends on Enter, so Shift-Enter is its newline.
        KeyBinding::new(
            "shift-enter",
            gpui_component::input::Enter { secondary: true },
            Some("AgentComposer > Input"),
        ),
        // Browser
        KeyBinding::new("cmd-l", FocusAddressBar, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-r", Reload, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-[", GoBack, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-]", GoForward, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-alt-i", ShowDevTools, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-=", ZoomIn, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd--", ZoomOut, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-0", ResetZoom, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-c", Copy, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-x", Cut, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-z", Undo, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-shift-z", Redo, Some(BROWSER_CONTEXT)),
        // gpui-component's Root uses Tab to move focus; inside the page and
        // the terminal it has to reach the content instead.
        KeyBinding::new("tab", PageTab, Some(BROWSER_CONTEXT)),
        KeyBinding::new("shift-tab", PageBackTab, Some(BROWSER_CONTEXT)),
        KeyBinding::new("tab", TerminalTab, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("shift-tab", TerminalBackTab, Some(TERMINAL_CONTEXT)),
        // Terminal
        KeyBinding::new("cmd-v", TerminalPaste, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("cmd-k", TerminalClear, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("cmd-shift-c", TerminalCopyScreen, Some(TERMINAL_CONTEXT)),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_binding_parses() {
        // GPUI panics on a malformed keystroke or context; catch it here
        // rather than at launch.
        assert!(super::key_bindings().len() > 40);
    }
}
