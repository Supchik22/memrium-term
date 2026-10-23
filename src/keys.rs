//! Keyboard events -> bytes for the PTY (xterm-style sequences).

use alacritty_terminal::term::TermMode;
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers};

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4)
fn mod_param(m: &Modifiers) -> u8 {
    1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8
}

fn csi(final_byte: char, m: &Modifiers, with_one: bool) -> Vec<u8> {
    let p = mod_param(m);
    if p == 1 {
        format!("\x1b[{final_byte}").into_bytes()
    } else if with_one {
        format!("\x1b[1;{p}{final_byte}").into_bytes()
    } else {
        format!("\x1b[{p}{final_byte}").into_bytes()
    }
}

fn tilde(n: u8, m: &Modifiers) -> Vec<u8> {
    let p = mod_param(m);
    if p == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{p}~").into_bytes()
    }
}

pub fn encode(event: &KeyEvent, m: &Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let arrow = |c: char| -> Vec<u8> {
        if app_cursor && mod_param(m) == 1 {
            format!("\x1bO{c}").into_bytes()
        } else {
            csi(c, m, true)
        }
    };
    let f1_4 = |c: char| -> Vec<u8> {
        if mod_param(m) == 1 {
            format!("\x1bO{c}").into_bytes()
        } else {
            csi(c, m, true)
        }
    };
    let out = match event.keysym {
        Keysym::Return | Keysym::KP_Enter => vec![b'\r'],
        Keysym::BackSpace => vec![if m.ctrl { 0x08 } else { 0x7f }],
        Keysym::Tab => vec![b'\t'],
        Keysym::ISO_Left_Tab => b"\x1b[Z".to_vec(),
        Keysym::Escape => vec![0x1b],
        Keysym::Up => arrow('A'),
        Keysym::Down => arrow('B'),
        Keysym::Right => arrow('C'),
        Keysym::Left => arrow('D'),
        Keysym::Home => {
            if app_cursor && mod_param(m) == 1 {
                b"\x1bOH".to_vec()
            } else {
                csi('H', m, true)
            }
        }
        Keysym::End => {
            if app_cursor && mod_param(m) == 1 {
                b"\x1bOF".to_vec()
            } else {
                csi('F', m, true)
            }
        }
        Keysym::Insert => tilde(2, m),
        Keysym::Delete => tilde(3, m),
        Keysym::Page_Up => tilde(5, m),
        Keysym::Page_Down => tilde(6, m),
        Keysym::F1 => f1_4('P'),
        Keysym::F2 => f1_4('Q'),
        Keysym::F3 => f1_4('R'),
        Keysym::F4 => f1_4('S'),
        Keysym::F5 => tilde(15, m),
        Keysym::F6 => tilde(17, m),
        Keysym::F7 => tilde(18, m),
        Keysym::F8 => tilde(19, m),
        Keysym::F9 => tilde(20, m),
        Keysym::F10 => tilde(21, m),
        Keysym::F11 => tilde(23, m),
        Keysym::F12 => tilde(24, m),
        Keysym::space if m.ctrl => vec![0],
        _ => {
            let text = event.utf8.as_ref().filter(|t| !t.is_empty())?;
            let mut v = Vec::new();
            if m.alt {
                v.push(0x1b);
            }
            v.extend_from_slice(text.as_bytes());
            return Some(v);
        }
    };
    if m.alt && out.len() == 1 {
        let mut v = vec![0x1b];
        v.extend(out);
        return Some(v);
    }
    Some(out)
}
