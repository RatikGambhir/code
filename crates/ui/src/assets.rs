//! Asset source: gpui-component's icon set plus Hangar's own icons.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};
use gpui_component::Icon;

/// Extra Lucide icons (ISC licensed, see `assets/icons/LICENSE-lucide.txt`).
const ICONS: &[(&str, &[u8])] = &[
    ("icons/cockpit/arrow-down-to-dot.svg", include_bytes!("../../../assets/icons/arrow-down-to-dot.svg")),
    ("icons/cockpit/arrow-right-to-line.svg", include_bytes!("../../../assets/icons/arrow-right-to-line.svg")),
    ("icons/cockpit/arrow-up-from-dot.svg", include_bytes!("../../../assets/icons/arrow-up-from-dot.svg")),
    ("icons/cockpit/bug.svg", include_bytes!("../../../assets/icons/bug.svg")),
    ("icons/cockpit/list-tree.svg", include_bytes!("../../../assets/icons/list-tree.svg")),
    ("icons/cockpit/pause.svg", include_bytes!("../../../assets/icons/pause.svg")),
    ("icons/cockpit/redo-dot.svg", include_bytes!("../../../assets/icons/redo-dot.svg")),
    ("icons/cockpit/arrow-up.svg", include_bytes!("../../../assets/icons/arrow-up.svg")),
    ("icons/cockpit/circle-dot.svg", include_bytes!("../../../assets/icons/circle-dot.svg")),
    ("icons/cockpit/circle-stop.svg", include_bytes!("../../../assets/icons/circle-stop.svg")),
    ("icons/cockpit/code-xml.svg", include_bytes!("../../../assets/icons/code-xml.svg")),
    ("icons/cockpit/columns-2.svg", include_bytes!("../../../assets/icons/columns-2.svg")),
    ("icons/cockpit/file-diff.svg", include_bytes!("../../../assets/icons/file-diff.svg")),
    ("icons/cockpit/files.svg", include_bytes!("../../../assets/icons/files.svg")),
    ("icons/cockpit/git-branch.svg", include_bytes!("../../../assets/icons/git-branch.svg")),
    ("icons/cockpit/git-commit-horizontal.svg", include_bytes!("../../../assets/icons/git-commit-horizontal.svg")),
    ("icons/cockpit/git-compare.svg", include_bytes!("../../../assets/icons/git-compare.svg")),
    ("icons/cockpit/history.svg", include_bytes!("../../../assets/icons/history.svg")),
    ("icons/cockpit/list-checks.svg", include_bytes!("../../../assets/icons/list-checks.svg")),
    ("icons/cockpit/play.svg", include_bytes!("../../../assets/icons/play.svg")),
    ("icons/cockpit/plus.svg", include_bytes!("../../../assets/icons/plus.svg")),
    ("icons/cockpit/refresh-cw.svg", include_bytes!("../../../assets/icons/refresh-cw.svg")),
    ("icons/cockpit/rotate-cw.svg", include_bytes!("../../../assets/icons/rotate-cw.svg")),
    ("icons/cockpit/shield-check.svg", include_bytes!("../../../assets/icons/shield-check.svg")),
    ("icons/cockpit/sparkles.svg", include_bytes!("../../../assets/icons/sparkles.svg")),
    ("icons/cockpit/square.svg", include_bytes!("../../../assets/icons/square.svg")),
    ("icons/cockpit/terminal.svg", include_bytes!("../../../assets/icons/terminal.svg")),
    ("icons/cockpit/wrench.svg", include_bytes!("../../../assets/icons/wrench.svg")),
    ("icons/cockpit/x.svg", include_bytes!("../../../assets/icons/x.svg")),
    ("icons/cockpit/archive.svg", include_bytes!("../../../assets/icons/archive.svg")),
    ("icons/cockpit/external-link.svg", include_bytes!("../../../assets/icons/external-link.svg")),
    ("icons/cockpit/message-square.svg", include_bytes!("../../../assets/icons/message-square.svg")),
    ("icons/cockpit/square-pen.svg", include_bytes!("../../../assets/icons/square-pen.svg")),
    ("icons/cockpit/notebook-pen.svg", include_bytes!("../../../assets/icons/notebook-pen.svg")),
];

/// Zed's typefaces: IBM Plex Sans for the interface and Lilex (IBM Plex
/// Mono's letters with programming ligatures) for code. Both are under the
/// SIL Open Font License 1.1; see `assets/fonts/*/LICENSE.txt`.
pub const UI_FONT: &str = "IBM Plex Sans";
pub const MONO_FONT: &str = "Lilex";

/// The faces the interface uses: regular, italic, and the medium, semibold
/// and bold weights for emphasis.
const FONTS: &[&[u8]] = &[
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"),
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf"),
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Medium.ttf"),
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Bold.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Italic.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Medium.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Bold.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-BoldItalic.ttf"),
];

/// Makes the bundled fonts available to the text system. Returns false if
/// they could not be loaded (the interface then falls back to system fonts).
pub fn load_fonts(cx: &App) -> bool {
    let fonts = FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        tracing::warn!("loading the bundled fonts: {err:#}");
        return false;
    }
    // The text system quietly substitutes a fallback for a family it cannot
    // find, and the nearest weight for a missing face: check both.
    let text = cx.text_system();
    let resolves = |family: &'static str, weights: &[gpui::FontWeight]| {
        let ids: Vec<_> = weights
            .iter()
            .map(|&weight| text.resolve_font(&gpui::Font { weight, ..gpui::font(SharedString::new_static(family)) }))
            .collect();
        let found = text.get_font_for_id(ids[0]).is_some_and(|font| font.family.as_ref() == family);
        let distinct = ids.iter().enumerate().all(|(i, id)| !ids[..i].contains(id));
        found && distinct
    };
    use gpui::FontWeight as W;
    let ok = resolves(UI_FONT, &[W::NORMAL, W::MEDIUM, W::SEMIBOLD, W::BOLD]) && resolves(MONO_FONT, &[W::NORMAL, W::MEDIUM, W::BOLD]);
    if ok {
        tracing::debug!("bundled fonts loaded: {UI_FONT}, {MONO_FONT}");
    } else {
        tracing::warn!("the bundled fonts loaded but do not resolve; using system fonts");
    }
    ok
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = ICONS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut list = gpui_component_assets::Assets.list(path)?;
        list.extend(
            ICONS
                .iter()
                .filter(|(name, _)| name.starts_with(path))
                .map(|(name, _)| SharedString::from(*name)),
        );
        Ok(list)
    }
}

/// One of Hangar's own icons, by file stem (`"git-branch"`).
pub fn icon(name: &str) -> Icon {
    Icon::empty().path(SharedString::from(format!("icons/cockpit/{name}.svg")))
}
