//! Shared TermVault-style palette, independent of the terminal's ANSI colors.
use ratatui::{
    style::{Color, Modifier, Style},
    widgets::{Block, Borders},
};

pub const BACKGROUND: Color = Color::Rgb(10, 10, 10);
pub const SURFACE: Color = Color::Rgb(19, 19, 19);
pub const TOAST: Color = Color::Rgb(30, 30, 30);
pub const TOAST_EDGE: Color = Color::Rgb(72, 72, 72);
pub const TEXT: Color = Color::Rgb(230, 230, 230);
pub const MUTED: Color = Color::Rgb(128, 128, 128);
pub const BORDER: Color = Color::Rgb(48, 48, 48);
pub const PRIMARY: Color = Color::Rgb(255, 177, 132);
pub const SECONDARY: Color = Color::Rgb(177, 139, 230);
pub const SUCCESS: Color = Color::Rgb(125, 207, 143);
pub const WARNING: Color = Color::Rgb(229, 192, 123);
pub const ERROR: Color = Color::Rgb(243, 139, 168);
pub const INK: Color = Color::Rgb(0, 0, 0);
pub const DIMMED: Color = Color::Rgb(65, 65, 65);

// Compatibility aliases for existing UI callers.
pub const BG: Color = BACKGROUND;
pub const FG: Color = TEXT;
pub const BLACK: Color = INK;
pub const WHITE: Color = TEXT;

pub fn base() -> Style {
    Style::default().fg(TEXT).bg(BACKGROUND)
}

pub fn surface() -> Style {
    base().bg(SURFACE)
}

pub fn selected() -> Style {
    surface().fg(INK).bg(PRIMARY).add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    base().fg(MUTED)
}

pub fn dialog() -> Style {
    surface()
}

pub fn block(title: &str) -> Block<'static> {
    Block::default()
        .title(title.to_owned())
        .borders(Borders::ALL)
        .style(surface())
        .border_style(Style::default().fg(BORDER))
        .title_style(Style::default().fg(SECONDARY))
}

pub fn active_block(title: &str) -> Block<'static> {
    block(title)
        .border_style(Style::default().fg(PRIMARY))
        .title_style(Style::default().fg(PRIMARY))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_styles_use_explicit_palette() {
        assert_eq!(base().fg, Some(TEXT));
        assert_eq!(base().bg, Some(BACKGROUND));
        assert_eq!(dialog().bg, Some(SURFACE));
        assert_eq!(dim().fg, Some(MUTED));
        assert_eq!(selected().fg, Some(INK));
        assert_eq!(selected().bg, Some(PRIMARY));
        assert!(selected().add_modifier.contains(Modifier::BOLD));
    }
}
