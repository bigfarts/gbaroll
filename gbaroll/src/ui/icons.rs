//! Lucide icons rendered from the bundled font (registered in `main`).
//! Glyphs are plain `Text`, so they inherit the surrounding/button text
//! colour automatically — no per-icon styling needed.

use iced::widget::{text, Text};
use iced::Font;

pub use lucide_icons::Icon;

/// The lucide font family (its bytes are loaded by the iced application
/// builder in `main`).
pub const LUCIDE: Font = Font::with_name("lucide");

/// An icon glyph as a `Text` widget, sized in logical pixels.
pub fn icon<'a>(icon: Icon, size: f32) -> Text<'a> {
    text(icon.unicode().to_string()).font(LUCIDE).size(size)
}
