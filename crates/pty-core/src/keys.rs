//! Named keys encoded as xterm-compatible input bytes.
use anyhow::{Result, bail};

enum Special {
    /// CSI or SS3 key identified by a final byte: arrows, Home, End and F1-F4.
    Final {
        byte: u8,
        cursor: bool,
    },
    /// `CSI n ~` key.
    Tilde(u8),
    Plain(&'static [u8]),
}

fn special(name: &str) -> Option<Special> {
    use Special::*;
    Some(match name {
        "up" => Final {
            byte: b'A',
            cursor: true,
        },
        "down" => Final {
            byte: b'B',
            cursor: true,
        },
        "right" => Final {
            byte: b'C',
            cursor: true,
        },
        "left" => Final {
            byte: b'D',
            cursor: true,
        },
        "home" => Final {
            byte: b'H',
            cursor: true,
        },
        "end" => Final {
            byte: b'F',
            cursor: true,
        },
        "f1" => Final {
            byte: b'P',
            cursor: false,
        },
        "f2" => Final {
            byte: b'Q',
            cursor: false,
        },
        "f3" => Final {
            byte: b'R',
            cursor: false,
        },
        "f4" => Final {
            byte: b'S',
            cursor: false,
        },
        "insert" => Tilde(2),
        "delete" | "del" => Tilde(3),
        "pageup" | "pgup" => Tilde(5),
        "pagedown" | "pgdn" => Tilde(6),
        "f5" => Tilde(15),
        "f6" => Tilde(17),
        "f7" => Tilde(18),
        "f8" => Tilde(19),
        "f9" => Tilde(20),
        "f10" => Tilde(21),
        "f11" => Tilde(23),
        "f12" => Tilde(24),
        "enter" | "return" => Plain(b"\r"),
        "tab" => Plain(b"\t"),
        "esc" | "escape" => Plain(b"\x1b"),
        "backspace" | "bs" => Plain(b"\x7f"),
        "space" => Plain(b" "),
        _ => return None,
    })
}

fn control(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c as u8 - b'a' + 1,
        '@' | ' ' => 0,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    })
}

/// Encodes one key such as `Enter`, `Up`, `C-c`, `M-x`, `S-Tab`, `C-Left` or `F5`.
/// Prefixes `C-`, `M-` (or `A-`) and `S-` add Ctrl, Alt and Shift. Unmodified arrows,
/// Home and End use SS3 when the application enabled cursor-key mode.
pub fn encode(name: &str, application_cursor: bool) -> Result<Vec<u8>> {
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    let mut key = name;
    while key.len() > 2 && key.as_bytes()[1] == b'-' {
        match key.as_bytes()[0].to_ascii_uppercase() {
            b'C' => ctrl = true,
            b'M' | b'A' => alt = true,
            b'S' => shift = true,
            _ => break,
        }
        key = &key[2..];
    }
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if shift {
            bail!("{name}: use the shifted character instead of S-");
        }
        let mut bytes = if alt { vec![0x1b] } else { vec![] };
        match (ctrl, control(c)) {
            (true, Some(code)) => bytes.push(code),
            (true, None) => bail!("{name}: no Ctrl code for {c:?}"),
            (false, _) => bytes.extend_from_slice(c.to_string().as_bytes()),
        }
        return Ok(bytes);
    }
    let Some(special) = special(&key.to_ascii_lowercase()) else {
        bail!("unknown key {name:?}");
    };
    Ok(match special {
        Special::Final { byte, cursor } if modifier == 1 => {
            if cursor && !application_cursor {
                vec![0x1b, b'[', byte]
            } else {
                vec![0x1b, b'O', byte]
            }
        }
        Special::Final { byte, .. } => {
            let mut bytes = format!("\x1b[1;{modifier}").into_bytes();
            bytes.push(byte);
            bytes
        }
        Special::Tilde(code) if modifier == 1 => format!("\x1b[{code}~").into_bytes(),
        Special::Tilde(code) => format!("\x1b[{code};{modifier}~").into_bytes(),
        Special::Plain(b"\t") if shift && !ctrl => {
            let mut bytes = if alt { vec![0x1b] } else { vec![] };
            bytes.extend_from_slice(b"\x1b[Z");
            bytes
        }
        Special::Plain(b" ") if ctrl && !shift => {
            if alt {
                vec![0x1b, 0]
            } else {
                vec![0]
            }
        }
        Special::Plain(_) if ctrl || shift => bail!("{name}: unsupported modifier"),
        Special::Plain(bytes) if alt => [b"\x1b".as_slice(), bytes].concat(),
        Special::Plain(bytes) => bytes.to_vec(),
    })
}

/// Encodes keys in order as one input.
pub fn encode_all(names: &[String], application_cursor: bool) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for name in names {
        bytes.extend(encode(name, application_cursor)?);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(name: &str) -> Vec<u8> {
        encode(name, false).unwrap()
    }

    #[test]
    fn plain_control_and_meta_keys() {
        assert_eq!(key("Enter"), b"\r");
        assert_eq!(key("esc"), b"\x1b");
        assert_eq!(key("C-c"), b"\x03");
        assert_eq!(key("C-D"), b"\x04");
        assert_eq!(key("C-["), b"\x1b");
        assert_eq!(key("C-Space"), b"\0");
        assert_eq!(key("M-x"), b"\x1bx");
        assert_eq!(key("M-Enter"), b"\x1b\r");
        assert_eq!(key("S-Tab"), b"\x1b[Z");
        assert_eq!(key("q"), b"q");
        assert_eq!(key("-"), b"-");
        assert_eq!(key("é"), "é".as_bytes());
    }

    #[test]
    fn cursor_keys_follow_application_mode_and_modifiers() {
        assert_eq!(key("Up"), b"\x1b[A");
        assert_eq!(encode("Up", true).unwrap(), b"\x1bOA");
        assert_eq!(encode("C-Left", true).unwrap(), b"\x1b[1;5D");
        assert_eq!(key("S-Up"), b"\x1b[1;2A");
        assert_eq!(key("M-Right"), b"\x1b[1;3C");
        assert_eq!(key("F1"), b"\x1bOP");
        assert_eq!(key("F5"), b"\x1b[15~");
        assert_eq!(key("C-Delete"), b"\x1b[3;5~");
        assert_eq!(key("PageDown"), b"\x1b[6~");
    }

    #[test]
    fn rejects_unknown_and_unsupported_keys() {
        assert!(encode("Hyper", false).is_err());
        assert!(encode("S-a", false).is_err());
        assert!(encode("C-Enter", false).is_err());
        assert!(encode("C-1", false).is_err());
        assert!(encode("C--", false).is_err());
        assert_eq!(
            encode_all(&["C-a".into(), "k".into()], false).unwrap(),
            b"\x01k"
        );
    }
}
