//! Rex theme colors. Asks the terminal for its foreground, background, and a few
//! palette colors (OSC 10, 11, and 4) and mixes the green/red backgrounds from
//! them, so the panel matches any theme.

use std::io::Write;
use std::time::{Duration, Instant};

use ratatui::style::Color;

pub type Rgb = (u8, u8, u8);

const ANSI_QUERIED: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

#[derive(Clone, Copy, PartialEq, Default)]
pub struct Detected {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub ansi: [Option<Rgb>; 16],
}

/// Asks the terminal for its colors. Needs raw mode on, and must not run while
/// crossterm is reading events. The DA1 reply marks the end: every terminal
/// answers it, so we never wait for nothing.
pub fn query(timeout: Duration) -> Option<Detected> {
    let mut q = String::from("\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    for i in ANSI_QUERIED {
        q.push_str(&format!("\x1b]4;{i};?\x1b\\"));
    }
    q.push_str("\x1b[c");
    let mut out = std::io::stdout();
    out.write_all(q.as_bytes()).ok()?;
    out.flush().ok()?;

    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as i32) };
        if n <= 0 {
            break;
        }
        let mut chunk = [0u8; 1024];
        let r = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if r <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..r as usize]);
        if has_da1(&buf) {
            break;
        }
    }
    let d = parse(&String::from_utf8_lossy(&buf));
    (d.bg.is_some() || d.fg.is_some()).then_some(d)
}

fn has_da1(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    s.match_indices("\x1b[?").any(|(i, _)| {
        s[i + 3..]
            .chars()
            .find(|c| !(c.is_ascii_digit() || *c == ';'))
            .is_some_and(|c| c == 'c')
    })
}

fn parse(s: &str) -> Detected {
    let mut d = Detected::default();
    for part in s.split("\x1b]").skip(1) {
        let end = part.find(['\x07', '\x1b']).unwrap_or(part.len());
        let body = &part[..end];
        let Some((head, color)) = body.split_once(";rgb:") else { continue };
        let Some(rgb) = parse_rgb(color) else { continue };
        match head {
            "10" => d.fg = Some(rgb),
            "11" => d.bg = Some(rgb),
            h => {
                if let Some(idx) = h.strip_prefix("4;").and_then(|i| i.parse::<u8>().ok())
                    && let Some(pos) = ANSI_QUERIED.iter().position(|&q| q == idx)
                {
                    d.ansi[pos] = Some(rgb);
                }
            }
        }
    }
    d
}

fn parse_rgb(s: &str) -> Option<Rgb> {
    let mut it = s.split('/').map(|h| {
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len() as u32)) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    });
    Some((it.next()??, it.next()??, it.next()??))
}

/// Every color the panel uses.
pub struct Palette {
    pub ansi: [Rgb; 16],
    pub fg: Color,
    pub muted: Color,
    pub faint: Color,
    pub rule: Color,
    pub sel_bg: Color,
    pub pill_bg: Color,
    pub green: Color,
    pub red: Color,
    pub yellow: Color,
    pub blue: Color,
    pub annotation: Color,
    pub selection_fg: Color,
    pub search_fg: Color,
    pub add_bg: Color,
    pub add_gutter: Color,
    pub add_emph: Color,
    pub del_bg: Color,
    pub del_gutter: Color,
    pub del_emph: Color,
    pub hunk_bg: Color,
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

fn luma(c: Rgb) -> f32 {
    (0.2126 * c.0 as f32 + 0.7152 * c.1 as f32 + 0.0722 * c.2 as f32) / 255.0
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

fn contrast_text((r, g, b): Rgb) -> Color {
    let linear = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
    if luminance > 0.179 { Color::Rgb(0, 0, 0) } else { Color::Rgb(255, 255, 255) }
}

impl Palette {
    pub fn from(d: Option<Detected>) -> Self {
        let d = d.unwrap_or_default();
        let dark = d.bg.map(|b| luma(b) < 0.5).unwrap_or(true);
        let (bg, fg) = match dark {
            true => (d.bg.unwrap_or((24, 24, 27)), d.fg.unwrap_or((228, 228, 231))),
            false => (d.bg.unwrap_or((255, 255, 255)), d.fg.unwrap_or((36, 41, 47))),
        };
        let pick = |i: usize, dark_c: Rgb, light_c: Rgb| d.ansi[i].unwrap_or(if dark { dark_c } else { light_c });
        let red = pick(1, (248, 81, 73), (207, 34, 46));
        let green = pick(2, (63, 185, 80), (26, 127, 55));
        let yellow = pick(3, (210, 153, 34), (154, 103, 0));
        let blue = pick(4, (88, 166, 255), (9, 105, 218));
        let fallback = [bg, red, green, yellow, blue, (188,140,255), (57,197,207), fg,
            mix(fg,bg,0.62), red, green, yellow, blue, (188,140,255), (57,197,207), fg];

        // Dark backgrounds need a bit more color for the tint to show.
        let (row, gutter, emph) = if dark { (0.17, 0.27, 0.40) } else { (0.11, 0.20, 0.30) };
        Self {
            ansi: std::array::from_fn(|i| d.ansi[i].unwrap_or(fallback[i])),
            fg: Color::Reset,
            muted: rgb(mix(fg, bg, 0.40)),
            faint: rgb(mix(fg, bg, 0.62)),
            rule: rgb(mix(fg, bg, 0.86)),
            sel_bg: rgb(mix(bg, fg, if dark { 0.10 } else { 0.07 })),
            pill_bg: rgb(mix(bg, fg, if dark { 0.12 } else { 0.08 })),
            green: rgb(green),
            red: rgb(red),
            yellow: rgb(yellow),
            blue: rgb(blue),
            annotation: rgb(if dark { (215,166,95) } else { (137,89,32) }),
            selection_fg: contrast_text(blue),
            search_fg: contrast_text(yellow),
            add_bg: rgb(mix(bg, green, row)),
            add_gutter: rgb(mix(bg, green, gutter)),
            add_emph: rgb(mix(bg, green, emph)),
            del_bg: rgb(mix(bg, red, row)),
            del_gutter: rgb(mix(bg, red, gutter)),
            del_emph: rgb(mix(bg, red, emph)),
            hunk_bg: rgb(mix(bg, blue, if dark { 0.12 } else { 0.07 })),
        }
    }
}
