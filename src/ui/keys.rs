use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn encode(key: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let arrow = |c: u8| {
        if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    let mut out = match key.code {
        KeyCode::Char(c) if ctrl => match c {
            'a'..='z' => vec![c as u8 - b'a' + 1],
            'A'..='Z' => vec![c as u8 - b'A' + 1],
            ' ' | '@' => vec![0],
            '[' => vec![0x1b],
            '\\' => vec![0x1c],
            ']' => vec![0x1d],
            '^' => vec![0x1e],
            '_' => vec![0x1f],
            _ => c.to_string().into_bytes(),
        },
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => arrow(b'A'),
        KeyCode::Down => arrow(b'B'),
        KeyCode::Right => arrow(b'C'),
        KeyCode::Left => arrow(b'D'),
        KeyCode::Home => arrow(b'H'),
        KeyCode::End => arrow(b'F'),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + n - 1],
        KeyCode::F(n) => match n {
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => vec![],
        },
        _ => vec![],
    };
    if alt && !out.is_empty() {
        out.insert(0, 0x1b);
    }
    out
}

pub fn wheel(up: bool, col: u16, row: u16, enc: vt100::MouseProtocolEncoding) -> Vec<u8> {
    let button: u32 = if up { 64 } else { 65 };
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    match enc {
        vt100::MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{x};{y}M").into_bytes(),
        vt100::MouseProtocolEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for v in [button, x, y] {
                let c = char::from_u32(v + 32).unwrap_or(' ');
                out.extend(c.to_string().into_bytes());
            }
            out
        }
        vt100::MouseProtocolEncoding::Default => {
            let b = |v: u32| (v + 32).min(255) as u8;
            vec![0x1b, b'[', b'M', b(button), b(x), b(y)]
        }
    }
}

pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn encodes_control_alt_and_cursor_keys() {
        assert_eq!(
            encode(k(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            [3]
        );
        assert_eq!(
            encode(k(KeyCode::Char('x'), KeyModifiers::ALT), false),
            b"\x1bx"
        );
        assert_eq!(encode(k(KeyCode::Up, KeyModifiers::NONE), false), b"\x1b[A");
        assert_eq!(encode(k(KeyCode::Up, KeyModifiers::NONE), true), b"\x1bOA");
        assert_eq!(encode(k(KeyCode::Enter, KeyModifiers::NONE), false), b"\r");
        assert_eq!(
            encode(k(KeyCode::F(2), KeyModifiers::NONE), false),
            b"\x1bOQ"
        );
    }

    #[test]
    fn encodes_wheel_events() {
        use vt100::MouseProtocolEncoding as E;
        assert_eq!(wheel(true, 4, 9, E::Sgr), b"\x1b[<64;5;10M");
        assert_eq!(wheel(false, 0, 0, E::Sgr), b"\x1b[<65;1;1M");
        assert_eq!(wheel(true, 0, 0, E::Default), b"\x1b[M`!!");
        assert_eq!(
            wheel(false, 300, 0, E::Default),
            [0x1b, b'[', b'M', 97, 255, 33]
        );
        assert_eq!(wheel(true, 300, 0, E::Utf8), "\x1b[M`\u{14d}!".as_bytes());
    }
}
