//! Tokyo Night palette and shared widget helpers. We never paint a full-screen background,
//! so a translucent terminal (e.g. kitty `background_opacity`) shows through.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

pub const BG: Color = Color::Rgb(0x1a, 0x1b, 0x26);
pub const FG: Color = Color::Rgb(0xc0, 0xca, 0xf5);
pub const DIM: Color = Color::Rgb(0x56, 0x5f, 0x89);
pub const BLUE: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
pub const CYAN: Color = Color::Rgb(0x7d, 0xcf, 0xff);
pub const GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a);
pub const YELLOW: Color = Color::Rgb(0xe0, 0xaf, 0x68);
pub const PURPLE: Color = Color::Rgb(0xbb, 0x9a, 0xf7);
pub const RED: Color = Color::Rgb(0xf7, 0x76, 0x8e);
pub const BORDER: Color = Color::Rgb(0x2a, 0x2e, 0x42);

pub fn dim() -> Style {
    Style::default().fg(DIM)
}

pub fn fg() -> Style {
    Style::default().fg(FG)
}

pub fn accent(c: Color) -> Style {
    Style::default().fg(c)
}

pub fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

/// A rounded panel with a `◆ TITLE` header and a dim right-aligned subtitle, like velo.
pub fn panel<'a>(title: &'a str, subtitle: impl Into<Line<'a>>, accent_color: Color) -> Block<'a> {
    let head = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(accent_color)),
        Span::styled(title, bold(accent_color)),
        Span::raw(" "),
    ]);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title_top(head)
        .title_top(subtitle.into().right_aligned())
}

/// An inverse "keycap" span followed by its label, for the bottom hotbar.
pub fn keycap<'a>(key: &'a str, label: &'a str, active: bool) -> Vec<Span<'a>> {
    let key_style = if active {
        Style::default().fg(BG).bg(BLUE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(BG).bg(DIM).add_modifier(Modifier::BOLD)
    };
    vec![
        Span::styled(format!(" {key} "), key_style),
        Span::styled(format!(" {label}  "), if active { fg() } else { dim() }),
    ]
}
