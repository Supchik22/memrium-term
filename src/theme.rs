//! Colour palette (a cool, glassy dark theme) and colour resolution for terminal cells.

use chebrium::canvas::Rgb;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

pub const FG: Rgb = [0xf2, 0xf2, 0xf2];
pub const BG: Rgb = [0x1c, 0x1c, 0x1e];
pub const CURSOR: Rgb = [0xc8, 0xc8, 0xc8];
pub const ACCENT: Rgb = [0x3a, 0x8c, 0xff];

// macOS Terminal "Pro" palette
const ANSI: [Rgb; 16] = [
    [0x00, 0x00, 0x00], [0xc2, 0x36, 0x21], [0x25, 0xbc, 0x24], [0xad, 0xad, 0x27],
    [0x49, 0x2e, 0xe1], [0xd3, 0x38, 0xd3], [0x33, 0xbb, 0xc8], [0xcb, 0xcc, 0xcd],
    [0x81, 0x83, 0x83], [0xfc, 0x39, 0x1f], [0x31, 0xe7, 0x22], [0xea, 0xec, 0x23],
    [0x58, 0x33, 0xff], [0xf9, 0x35, 0xf8], [0x14, 0xf0, 0xf0], [0xe9, 0xeb, 0xeb],
];

fn indexed(i: usize) -> Rgb {
    match i {
        0..=15 => ANSI[i],
        16..=231 => {
            let n = i - 16;
            let lvl = |v: usize| if v == 0 { 0 } else { (55 + 40 * v) as u8 };
            [lvl(n / 36), lvl((n / 6) % 6), lvl(n % 6)]
        }
        232..=255 => {
            let v = (8 + 10 * (i - 232)) as u8;
            [v, v, v]
        }
        _ => FG,
    }
}

fn dim(c: Rgb) -> Rgb {
    [(c[0] as f32 * 0.66) as u8, (c[1] as f32 * 0.66) as u8, (c[2] as f32 * 0.66) as u8]
}

/// Resolve a terminal colour to RGB, honouring palette overrides set by the program (OSC 4/10/11).
pub fn resolve(color: Color, colors: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Indexed(i) => colors[i as usize].map(|c| [c.r, c.g, c.b]).unwrap_or_else(|| indexed(i as usize)),
        Color::Named(n) => {
            if let Some(c) = colors[n] {
                return [c.r, c.g, c.b];
            }
            match n {
                NamedColor::Foreground | NamedColor::BrightForeground => FG,
                NamedColor::DimForeground => dim(FG),
                NamedColor::Background => BG,
                NamedColor::Cursor => CURSOR,
                NamedColor::DimBlack => dim(ANSI[0]),
                NamedColor::DimRed => dim(ANSI[1]),
                NamedColor::DimGreen => dim(ANSI[2]),
                NamedColor::DimYellow => dim(ANSI[3]),
                NamedColor::DimBlue => dim(ANSI[4]),
                NamedColor::DimMagenta => dim(ANSI[5]),
                NamedColor::DimCyan => dim(ANSI[6]),
                NamedColor::DimWhite => dim(ANSI[7]),
                other => ANSI[(other as usize).min(15)],
            }
        }
    }
}

pub fn dimmed(c: Rgb) -> Rgb {
    dim(c)
}

/// Foreground colour; bold text uses the bright variant of the 8 base colours (as Terminal.app does).
pub fn resolve_fg(color: Color, colors: &Colors, bold: bool) -> Rgb {
    let color = match color {
        Color::Named(n) if bold && (n as usize) < 8 => Color::Indexed(n as u8 + 8),
        Color::Indexed(i) if bold && i < 8 => Color::Indexed(i + 8),
        c => c,
    };
    resolve(color, colors)
}
