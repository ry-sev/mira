//! Icons drawn on a 16 px grid with 1.5 px strokes, and the asset source that
//! serves them to GPUI.

use gpui_kit::AssetSource;
use gpui_kit::Rems;
use gpui_kit::Result;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::Svg;
use gpui_kit::svg;
use std::borrow::Cow;

/// Every icon the window draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    ArrowRight,
    Body,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    Clock,
    Close,
    Copy,
    Download,
    Error,
    Filter,
    Folder,
    Hash,
    Hex,
    Host,
    Inspector,
    Lock,
    Network,
    Overview,
    Pretty,
    Record,
    Recording,
    Search,
    Settings,
    Shield,
    Star,
    Target,
    Terminal,
    Trash,
    Warning,
    Wrap,
}

impl Icon {
    /// Every icon, for the asset listing and its test.
    pub const ALL: [Self; 31] = [
        Self::ArrowRight,
        Self::Body,
        Self::ChevronDown,
        Self::ChevronLeft,
        Self::ChevronRight,
        Self::Clock,
        Self::Close,
        Self::Copy,
        Self::Download,
        Self::Error,
        Self::Filter,
        Self::Folder,
        Self::Hash,
        Self::Hex,
        Self::Host,
        Self::Inspector,
        Self::Lock,
        Self::Network,
        Self::Overview,
        Self::Pretty,
        Self::Record,
        Self::Recording,
        Self::Search,
        Self::Settings,
        Self::Shield,
        Self::Star,
        Self::Target,
        Self::Terminal,
        Self::Trash,
        Self::Warning,
        Self::Wrap,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::ArrowRight => "arrow-right",
            Self::Body => "body",
            Self::ChevronDown => "chevron-down",
            Self::ChevronLeft => "chevron-left",
            Self::ChevronRight => "chevron-right",
            Self::Clock => "clock",
            Self::Close => "close",
            Self::Copy => "copy",
            Self::Download => "download",
            Self::Error => "error",
            Self::Filter => "filter",
            Self::Folder => "folder",
            Self::Hash => "hash",
            Self::Hex => "hex",
            Self::Host => "host",
            Self::Inspector => "inspector",
            Self::Lock => "lock",
            Self::Network => "network",
            Self::Overview => "overview",
            Self::Pretty => "pretty",
            Self::Record => "record",
            Self::Recording => "recording",
            Self::Search => "search",
            Self::Settings => "settings",
            Self::Shield => "shield",
            Self::Star => "star",
            Self::Target => "target",
            Self::Terminal => "terminal",
            Self::Trash => "trash",
            Self::Warning => "warning",
            Self::Wrap => "wrap",
        }
    }

    /// The asset path GPUI loads the icon from.
    pub fn path(self) -> SharedString {
        format!("icons/{}.svg", self.name()).into()
    }

    /// The icon's strokes and fills, inside a 16 by 16 view box.
    fn shapes(self) -> &'static str {
        match self {
            Self::ArrowRight => r#"<path d="M3 8h10M9 4l4 4-4 4"/>"#,
            Self::Body => {
                r#"<path d="M3.25 1.75h6.25l3.25 3.25v9.25h-9.5zM9.5 1.75V5h3.25M5.5 8h5M5.5 10.75h5"/>"#
            }
            Self::ChevronDown => r#"<path d="M3.5 6L8 10.5 12.5 6"/>"#,
            Self::ChevronLeft => r#"<path d="M10 3.5L5.5 8l4.5 4.5"/>"#,
            Self::ChevronRight => r#"<path d="M6 3.5L10.5 8 6 12.5"/>"#,
            Self::Clock => r#"<circle cx="8" cy="8" r="5.75"/><path d="M8 4.75V8l2.25 1.5"/>"#,
            Self::Close => r#"<path d="M4 4l8 8M12 4l-8 8"/>"#,
            Self::Copy => r#"<path d="M5.5 5.5h8v8h-8zM10.5 5.5v-3h-8v8h3"/>"#,
            Self::Download => r#"<path d="M8 2v8M4.5 6.5L8 10l3.5-3.5M2.5 13.5h11"/>"#,
            Self::Error => {
                r#"<circle cx="8" cy="8" r="5.75"/><path d="M5.75 5.75l4.5 4.5M10.25 5.75l-4.5 4.5"/>"#
            }
            Self::Filter => r#"<path d="M2 3h12l-4.5 5.25v4.5l-3-1.5v-3z"/>"#,
            Self::Folder => r#"<path d="M1.75 3.25h4.5l1.5 1.5h6.5v8.5H1.75z"/>"#,
            Self::Hash => r#"<path d="M6.25 2L4.75 14M11.25 2l-1.5 12M2.5 5.5h11M2 10.5h11"/>"#,
            Self::Hex => r#"<path d="M8 1.75l5.5 3.125v6.25L8 14.25l-5.5-3.125v-6.25z"/>"#,
            Self::Host => {
                r#"<circle cx="8" cy="8" r="5.75"/><path d="M2.25 8h11.5M8 2.25c-2.2 2.2-2.2 9.3 0 11.5M8 2.25c2.2 2.2 2.2 9.3 0 11.5"/>"#
            }
            Self::Inspector => r#"<path d="M2 3h12v10H2zM10 3v10"/>"#,
            Self::Lock => r#"<path d="M3.5 7.5h9v6h-9zM5.5 7.5V5a2.5 2.5 0 0 1 5 0v2.5"/>"#,
            Self::Network => r#"<path d="M1.75 3.5h5M3.75 6.5h6M6.75 9.5h4M8.75 12.5h5.5"/>"#,
            Self::Overview => {
                r#"<path d="M2.5 2.5h4.25v4.25H2.5zM9.25 2.5h4.25v4.25H9.25zM2.5 9.25h4.25v4.25H2.5zM9.25 9.25h4.25v4.25H9.25z"/>"#
            }
            Self::Pretty => r#"<path d="M2 3h12M5 6.33h9M5 9.66h9M2 13h12"/>"#,
            Self::Record => {
                r##"<circle cx="8" cy="8" r="5.75"/><circle cx="8" cy="8" r="3" fill="#000" stroke="none"/>"##
            }
            Self::Recording => {
                r##"<circle cx="8" cy="8" r="5.75"/><path d="M5.75 5.75h4.5v4.5h-4.5z" fill="#000" stroke="none"/>"##
            }
            Self::Search => r#"<circle cx="7" cy="7" r="4.25"/><path d="M10.25 10.25L14 14"/>"#,
            Self::Settings => {
                r#"<path d="M12.75 6.81L14.32 7L14.32 9L12.75 9.19L12.2 10.52L13.18 11.76L11.76 13.18L10.52 12.2L9.19 12.75L9 14.32L7 14.32L6.81 12.75L5.48 12.2L4.24 13.18L2.82 11.76L3.8 10.52L3.25 9.19L1.68 9L1.68 7L3.25 6.81L3.8 5.48L2.82 4.24L4.24 2.82L5.48 3.8L6.81 3.25L7 1.68L9 1.68L9.19 3.25L10.52 3.8L11.76 2.82L13.18 4.24L12.2 5.48z"/><circle cx="8" cy="8" r="2"/>"#
            }
            Self::Shield => {
                r#"<path d="M8 1.75l5.25 2v4c0 3.25-2.25 5.25-5.25 6.5-3-1.25-5.25-3.25-5.25-6.5v-4zM5.75 8l1.5 1.5 3-3"/>"#
            }
            // Mira's mark: the app icon's star, its four diffraction spikes
            // around a bright core, filled so it holds at title-bar size.
            Self::Star => {
                r##"<path d="M8 .75L9.1 6.9 15.25 8 9.1 9.1 8 15.25 6.9 9.1 .75 8 6.9 6.9z" fill="#000" stroke="none"/><circle cx="8" cy="8" r="2.1" fill="#000" stroke="none"/>"##
            }
            Self::Target => {
                r##"<circle cx="8" cy="8" r="4.25"/><circle cx="8" cy="8" r="1.25" fill="#000" stroke="none"/><path d="M8 1v2.5M8 12.5V15M1 8h2.5M12.5 8H15"/>"##
            }
            Self::Terminal => r#"<path d="M1.75 2.75h12.5v10.5H1.75zM4.5 6l2 2-2 2M8 10.25h3.5"/>"#,
            Self::Trash => {
                r#"<path d="M2.5 4h11M6 4V2.25h4V4M3.75 4l.75 9.75h7L12.25 4M6.5 6.75v4.5M9.5 6.75v4.5"/>"#
            }
            Self::Warning => {
                r##"<path d="M8 2l6.25 11.25H1.75z"/><path d="M8 6.5v3"/><circle cx="8" cy="11.25" r=".9" fill="#000" stroke="none"/>"##
            }
            Self::Wrap => {
                r#"<path d="M2 4h12M2 8h9.25a2.25 2.25 0 0 1 0 4.5H8M9.75 11L8 12.5l1.75 1.5M2 12.5h3.5"/>"#
            }
        }
    }

    /// The whole SVG document.
    ///
    /// Its size matches its view box: GPUI draws an SVG's content without
    /// mapping the view box, so a document whose units differ from its size
    /// renders as a speck. Black, because GPUI paints the icon's shape in the
    /// element's text color and only reads the alpha.
    fn document(self) -> String {
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="#000" stroke-width="1.5" stroke-linecap="square" stroke-linejoin="miter">{}</svg>"##,
            self.shapes()
        )
    }

    fn from_path(path: &str) -> Option<Self> {
        let name = path.strip_prefix("icons/")?.strip_suffix(".svg")?;
        Self::ALL.into_iter().find(|icon| icon.name() == name)
    }
}

/// An icon at `size`, in the secondary ink unless its caller recolors it.
///
/// An SVG does not inherit its parent's text color, and one with no color
/// of its own is not painted at all, so every icon starts with one.
pub fn icon(name: Icon, size: Rems) -> Svg {
    svg()
        .path(name.path())
        .size(size)
        .flex_none()
        .text_color(crate::theme::colors().ink_2)
}

/// Everything the window loads by path.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(Icon::from_path(path).map(|icon| Cow::Owned(icon.document().into_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Icon::ALL
            .into_iter()
            .map(Icon::path)
            .filter(|asset| asset.starts_with(path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_every_icon_is_served() {
        // A missing icon draws as nothing at all, with no error anywhere.
        for icon in Icon::ALL {
            let bytes = Assets
                .load(&icon.path())
                .expect("loads")
                .expect("is served");
            let text = String::from_utf8(bytes.into_owned()).expect("utf-8");
            assert!(text.starts_with("<svg"), "{icon:?}");
        }
    }

    #[test]
    fn test_unknown_paths_are_not_served() {
        assert!(Assets.load("icons/nope.svg").expect("loads").is_none());
        assert!(
            Assets
                .load("elsewhere/search.svg")
                .expect("loads")
                .is_none()
        );
    }
}
