//! "Midnight Tokyo": a Tokyo Night base pushed toward neon (cyan, hot pink, acid green) on a near-black
//! indigo. We never paint a full-screen background, so a translucent terminal (e.g. kitty
//! `background_opacity`) shows through; `BG` is only used for text on a coloured chip and as the
//! colour borders fade toward.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

pub const BG: Color = Color::Rgb(0x0a, 0x0b, 0x14);
pub const FG: Color = Color::Rgb(0xb9, 0xc2, 0xe8);
pub const DIM: Color = Color::Rgb(0x4a, 0x52, 0x80);
pub const BLUE: Color = Color::Rgb(0x4d, 0x8d, 0xff);
pub const CYAN: Color = Color::Rgb(0x00, 0xe5, 0xff);
pub const GREEN: Color = Color::Rgb(0x39, 0xff, 0x88);
pub const YELLOW: Color = Color::Rgb(0xff, 0xc8, 0x57);
pub const PURPLE: Color = Color::Rgb(0xb4, 0x8c, 0xff);
pub const RED: Color = Color::Rgb(0xff, 0x4d, 0x6d);
pub const PINK: Color = Color::Rgb(0xff, 0x2e, 0x97);
pub const BORDER: Color = Color::Rgb(0x1f, 0x22, 0x40);

fn rgb(c: Color) -> (f32, f32, f32) {
    match c {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        _ => (0.0, 0.0, 0.0),
    }
}

/// Linear blend of two RGB colours: `t` = 0 gives `a`, 1 gives `b`.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let (ar, ag, ab) = rgb(a);
    let (br, bg, bb) = rgb(b);
    let t = t.clamp(0.0, 1.0);
    Color::Rgb((ar + (br - ar) * t) as u8, (ag + (bg - ag) * t) as u8, (ab + (bb - ab) * t) as u8)
}

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

/// A rounded panel: a neon-tinted border, a solid title chip, and a dim right-aligned subtitle.
pub fn panel<'a>(title: &'a str, subtitle: impl Into<Line<'a>>, accent_color: Color) -> Block<'a> {
    let head = Line::from(vec![
        Span::styled("▌", Style::default().fg(accent_color)),
        Span::styled(format!(" {title} "), Style::default().fg(BG).bg(accent_color).add_modifier(Modifier::BOLD)),
        Span::styled("▐", Style::default().fg(accent_color)),
        Span::raw(" "),
    ]);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(mix(accent_color, BORDER, 0.62)))
        .title_top(head)
        .title_top(subtitle.into().right_aligned())
}

/// An inverse "keycap" span followed by its label, for the bottom hotbar.
pub fn keycap<'a>(key: &'a str, label: &'a str, active: bool) -> Vec<Span<'a>> {
    let key_style = if active {
        Style::default().fg(BG).bg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(BG).bg(DIM).add_modifier(Modifier::BOLD)
    };
    vec![
        Span::styled(format!(" {key} "), key_style),
        Span::styled(format!(" {label}  "), if active { fg() } else { dim() }),
    ]
}

/// `text` coloured along a cyan -> pink gradient that drifts with `frame`, one colour per character.
pub fn gradient_spans(text: &str, frame: u64) -> Vec<Span<'static>> {
    let n = text.chars().count().max(1) as f32;
    text.chars()
        .enumerate()
        .map(|(i, ch)| {
            // A triangle wave over position + time, so the colour ping-pongs instead of wrapping hard.
            let p = (i as f32 / n + frame as f32 * 0.02).fract();
            let t = if p < 0.5 { p * 2.0 } else { 2.0 - p * 2.0 };
            Span::styled(ch.to_string(), Style::default().fg(mix(CYAN, PINK, t)).add_modifier(Modifier::BOLD))
        })
        .collect()
}

/// A cheap deterministic hash for the rain.
fn h(a: u64, b: u64) -> u64 {
    let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^ (x >> 32)
}

const GLYPHS: &[char] = &[
    'ｱ', 'ｲ', 'ｳ', 'ｴ', 'ｵ', 'ｶ', 'ｷ', 'ｸ', 'ｹ', 'ｺ', 'ｻ', 'ｼ', 'ｽ', 'ｾ', 'ｿ', 'ﾀ', 'ﾁ', 'ﾂ', 'ﾃ', 'ﾄ', '0', '1', '2', '3', '4', '5',
    '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F', '<', '>', '/', '|', ':', ';',
];

/// Matrix-style falling glyphs filling `width` x `height`, advancing with `frame`. The head of each
/// stream is bright and the tail fades into the background.
pub fn rain(width: u16, height: u16, frame: u64) -> Vec<Line<'static>> {
    let (w, hgt) = (width as usize, height.max(1) as u64);
    let mut rows: Vec<Vec<Span<'static>>> = (0..hgt).map(|_| Vec::with_capacity(w)).collect();
    for x in 0..w as u64 {
        let seed = h(x, 7);
        let speed = 1 + seed % 3; // rows per 4 frames-ish
        let len = 5 + (seed >> 8) % 9;
        let cycle = hgt + len + (seed >> 16) % hgt;
        let head = (frame * speed / 3 + (seed >> 24)) % cycle;
        for y in 0..hgt {
            // distance behind the head: 0 = head
            let d = head as i64 - y as i64;
            let span = if d >= 0 && (d as u64) < len {
                let t = d as f32 / len as f32;
                let ch = GLYPHS[(h(x * 131 + y, frame / 6) % GLYPHS.len() as u64) as usize];
                let color = if d == 0 { Color::Rgb(0xd8, 0xff, 0xe8) } else { mix(GREEN, BG, 0.2 + t * 0.78) };
                Span::styled(ch.to_string(), Style::default().fg(color))
            } else {
                Span::raw(" ")
            };
            rows[y as usize].push(span);
        }
    }
    rows.into_iter().map(Line::from).collect()
}

/// A 3-line block banner for the splash screen.
pub const BANNER: [&str; 3] = [
    "╔═╗╦ ╦╔═╗╔╦╗╔═╗╔╦╗╔═╗",
    "╚═╗╚╦╝╚═╗ ║ ║╣ ║║║╠═╣",
    "╚═╝ ╩ ╚═╝ ╩ ╚═╝╩ ╩╩ ╩",
];
