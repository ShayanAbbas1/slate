//! Slate's icon set.
//!
//! GPUI renders an SVG by asking the application's [`AssetSource`] for a file,
//! and gpui-component names Lucide files without shipping any. Rather than
//! vendoring another project's artwork into the repository, Slate depends on
//! `icondata_lu` — Lucide as Rust data — and serves the documents from memory
//! at the paths GPUI asks for. Nothing is read from disk, so this works the
//! same from `cargo run` and from a bundled `.app`.
//!
//! GPUI paints an SVG as a mask tinted by the element's text colour, so the
//! `currentColor` in the source is never used: an icon is whatever colour the
//! theme token beside it is.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_component::Icon;
use icondata_core::IconData;

/// The icons Slate can draw, by the path GPUI asks for.
///
/// The gpui-component widgets ask for their own paths — those are Lucide names
/// too, so they resolve here as well. Add a row when something asks for one;
/// an unlisted path simply draws nothing.
const ICONS: [(&str, &IconData); 22] = [
    ("icons/chevron-down.svg", icondata_lu::LuChevronDown),
    ("icons/chevron-right.svg", icondata_lu::LuChevronRight),
    ("icons/chevron-left.svg", icondata_lu::LuChevronLeft),
    ("icons/chevron-up.svg", icondata_lu::LuChevronUp),
    ("icons/check.svg", icondata_lu::LuCheck),
    ("icons/close.svg", icondata_lu::LuX),
    ("icons/ellipsis.svg", icondata_lu::LuEllipsis),
    ("icons/loader-circle.svg", icondata_lu::LuLoaderCircle),
    ("icons/minus.svg", icondata_lu::LuMinus),
    ("icons/plus.svg", icondata_lu::LuPlus),
    ("icons/search.svg", icondata_lu::LuSearch),
    ("icons/arrow-up.svg", icondata_lu::LuArrowUp),
    ("icons/arrow-down.svg", icondata_lu::LuArrowDown),
    ("icons/database.svg", icondata_lu::LuDatabase),
    ("icons/table.svg", icondata_lu::LuTable),
    ("icons/layers.svg", icondata_lu::LuLayers),
    ("icons/eye.svg", icondata_lu::LuEye),
    ("icons/hard-drive.svg", icondata_lu::LuHardDrive),
    ("icons/globe.svg", icondata_lu::LuGlobe),
    ("icons/list-tree.svg", icondata_lu::LuListTree),
    ("icons/square-function.svg", icondata_lu::LuSquareFunction),
    ("icons/square-play.svg", icondata_lu::LuSquarePlay),
];

/// Slate's own names for the icons it draws, so a call site names a thing
/// rather than a file.
pub mod icon {
    pub const CHEVRON_DOWN: &str = "icons/chevron-down.svg";
    pub const CHEVRON_RIGHT: &str = "icons/chevron-right.svg";
    pub const SEARCH: &str = "icons/search.svg";
    pub const DATABASE: &str = "icons/database.svg";
    pub const TABLE: &str = "icons/table.svg";
    pub const PARTITIONED_TABLE: &str = "icons/layers.svg";
    pub const VIEW: &str = "icons/eye.svg";
    pub const MATERIALIZED_VIEW: &str = "icons/hard-drive.svg";
    pub const FOREIGN_TABLE: &str = "icons/globe.svg";
    pub const FUNCTION: &str = "icons/square-function.svg";
    pub const PROCEDURE: &str = "icons/square-play.svg";
    pub const STRUCTURE: &str = "icons/list-tree.svg";
}

pub fn icon(path: &'static str) -> Icon {
    Icon::empty().path(path)
}

pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, data)| Cow::Owned(document(data).into_bytes())))
    }

    fn list(&self, _: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().map(|(name, _)| (*name).into()).collect())
    }
}

/// Wrap Lucide's path data in the SVG document GPUI's renderer expects.
fn document(icon: &IconData) -> String {
    let attribute = |name: &str, value: Option<&str>| {
        value
            .map(|value| format!(r#" {name}="{value}""#))
            .unwrap_or_default()
    };

    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg"{}{}{}{}{}{}{}>{}</svg>"#,
        attribute("viewBox", icon.view_box),
        attribute("width", icon.width),
        attribute("height", icon.height),
        attribute("fill", icon.fill),
        attribute("stroke", icon.stroke),
        attribute("stroke-width", icon.stroke_width),
        attribute("stroke-linecap", icon.stroke_linecap),
        icon.data
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_icon_resolves_to_a_document() {
        // A name with no entry in `ICONS` draws nothing at all, and a missing
        // icon is invisible rather than loud -- so the check has to be here.
        for path in [
            icon::CHEVRON_DOWN,
            icon::CHEVRON_RIGHT,
            icon::SEARCH,
            icon::DATABASE,
            icon::TABLE,
            icon::PARTITIONED_TABLE,
            icon::VIEW,
            icon::MATERIALIZED_VIEW,
            icon::FOREIGN_TABLE,
            icon::FUNCTION,
            icon::PROCEDURE,
            icon::STRUCTURE,
        ] {
            let loaded = Icons.load(path).unwrap();
            let document = loaded.unwrap_or_else(|| panic!("{path} has no icon"));
            let document = String::from_utf8(document.to_vec()).unwrap();
            assert!(document.starts_with("<svg"), "{path}: {document}");
            assert!(document.contains("<path"), "{path} drew nothing");
        }
    }
}
