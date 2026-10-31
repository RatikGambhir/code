//! Asset source: gpui-component's icon set plus Hangar's own icons.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};
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
