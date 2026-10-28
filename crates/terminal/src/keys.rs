//! Translating key presses into the byte sequences terminal programs expect
//! (xterm conventions).

/// A key press, in the vocabulary GPUI uses for keystrokes: `key` is the
/// normalized key name ("a", "enter", "left", "f5"), `text` what it types.
#[derive(Clone, Debug, Default)]
pub struct Key<'a> {
    pub key: &'a str,
    pub text: Option<&'a str>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Cmd on macOS. Never sent to the program; it is the app's modifier.
    pub platform: bool,
}

/// The xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
fn modifier_param(key: &Key<'_>) -> u8 {
    1 + key.shift as u8 + 2 * key.alt as u8 + 4 * key.ctrl as u8
}

/// Encodes `key`, or returns `None` when the terminal should not consume it.
pub fn encode(key: &Key<'_>, application_cursor: bool) -> Option<Vec<u8>> {
    if key.platform {
        return None;
    }
    let modified = key.shift || key.alt || key.ctrl;
    let param = modifier_param(key);

    // Cursor keys: SS3 form in application mode, CSI otherwise, and the
    // `CSI 1;m X` form whenever modifiers are held.
    let cursor = |final_byte: char| -> Vec<u8> {
        if modified {
            format!("\x1b[1;{param}{final_byte}").into_bytes()
        } else if application_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        }
    };
    // Editing keys: `CSI n ~`, or `CSI n;m ~` with modifiers.
    let tilde = |code: u8| -> Vec<u8> {
        if modified {
            format!("\x1b[{code};{param}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    };

    let bytes = match key.key {
        "enter" => {
            if key.alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        "tab" => {
            if key.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        "backspace" => {
            if key.ctrl {
                b"\x08".to_vec()
            } else if key.alt {
                b"\x1b\x7f".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        "escape" => b"\x1b".to_vec(),
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" | "f2" | "f3" | "f4" => {
            let final_byte = match key.key {
                "f1" => 'P',
                "f2" => 'Q',
                "f3" => 'R',
                _ => 'S',
            };
            if modified {
                format!("\x1b[1;{param}{final_byte}").into_bytes()
            } else {
                format!("\x1bO{final_byte}").into_bytes()
            }
        }
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "space" if key.ctrl => vec![0],
        _ => return encode_text_key(key),
    };
    Some(bytes)
}

fn encode_text_key(key: &Key<'_>) -> Option<Vec<u8>> {
    let base = if key.key == "space" { " " } else { key.key };

    if key.ctrl {
        let mut chars = base.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        let control = match c.to_ascii_lowercase() {
            c @ 'a'..='z' => c as u8 - b'a' + 1,
            '@' | '2' => 0,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '-' | '7' => 0x1f,
            '/' => 0x1f,
            '?' | '8' => 0x7f,
            _ => return None,
        };
        let mut bytes = Vec::with_capacity(2);
        if key.alt {
            bytes.push(0x1b);
        }
        bytes.push(control);
        return Some(bytes);
    }

    if key.alt {
        // Option as Meta: ESC followed by the unmodified character, which is
        // what shells and editors bind word motions to.
        let mut chars = base.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let c = if key.shift { c.to_ascii_uppercase() } else { c };
            let mut bytes = vec![0x1b];
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            return Some(bytes);
        }
    }

    let text = key.text.filter(|t| !t.is_empty())?;
    Some(text.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Key<'_> {
        Key {
            key: name,
            ..Default::default()
        }
    }

    #[test]
    fn plain_text_is_sent_as_typed() {
        let k = Key {
            key: "a",
            text: Some("a"),
            ..Default::default()
        };
        assert_eq!(encode(&k, false).unwrap(), b"a");
        let shifted = Key {
            key: "a",
            text: Some("A"),
            shift: true,
            ..Default::default()
        };
        assert_eq!(encode(&shifted, false).unwrap(), b"A");
    }

    #[test]
    fn control_letters_map_to_c0_codes() {
        let k = Key {
            key: "c",
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(encode(&k, false).unwrap(), vec![3]);
        let d = Key {
            key: "d",
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(encode(&d, false).unwrap(), vec![4]);
        let bracket = Key {
            key: "[",
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(encode(&bracket, false).unwrap(), vec![0x1b]);
    }

    #[test]
    fn arrows_follow_cursor_key_mode() {
        assert_eq!(encode(&key("up"), false).unwrap(), b"\x1b[A");
        assert_eq!(encode(&key("up"), true).unwrap(), b"\x1bOA");
        let shift_left = Key {
            key: "left",
            shift: true,
            ..Default::default()
        };
        assert_eq!(encode(&shift_left, true).unwrap(), b"\x1b[1;2D");
        let ctrl_right = Key {
            key: "right",
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(encode(&ctrl_right, false).unwrap(), b"\x1b[1;5C");
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(encode(&key("enter"), false).unwrap(), b"\r");
        assert_eq!(encode(&key("backspace"), false).unwrap(), b"\x7f");
        assert_eq!(encode(&key("delete"), false).unwrap(), b"\x1b[3~");
        assert_eq!(encode(&key("pageup"), false).unwrap(), b"\x1b[5~");
        assert_eq!(encode(&key("f1"), false).unwrap(), b"\x1bOP");
        assert_eq!(encode(&key("f5"), false).unwrap(), b"\x1b[15~");
        assert_eq!(encode(&key("f12"), false).unwrap(), b"\x1b[24~");
        let shift_tab = Key {
            key: "tab",
            shift: true,
            ..Default::default()
        };
        assert_eq!(encode(&shift_tab, false).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn option_acts_as_meta() {
        let k = Key {
            key: "b",
            text: Some("∫"),
            alt: true,
            ..Default::default()
        };
        assert_eq!(encode(&k, false).unwrap(), b"\x1bb");
    }

    #[test]
    fn command_shortcuts_are_left_to_the_app() {
        let k = Key {
            key: "c",
            text: Some("c"),
            platform: true,
            ..Default::default()
        };
        assert!(encode(&k, false).is_none());
    }

    #[test]
    fn unknown_named_keys_without_text_are_ignored() {
        assert!(encode(&key("f99"), false).is_none());
    }
}
