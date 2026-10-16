//! Platform-neutral input events for a browser tab, and their translation
//! into the codes Chromium expects.
//!
//! Chromium speaks Windows virtual-key codes (`VK_*`) on every platform, plus
//! the native macOS hardware key code. GPUI reports normalized key names
//! ("enter", "left", "a"), which are mapped here.
//!
//! The key tables and the "wait for AppKit's text decision" rule follow
//! bokuweb/gpui-cef (MIT OR Apache-2.0), which worked out how text input has
//! to reach an off-screen Chromium on macOS.

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    /// Command on macOS.
    pub command: bool,
    pub function: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Mouse input in the page's coordinate space (CSS pixels from its top-left).
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MouseInput {
    Move {
        x: f32,
        y: f32,
        pressed: Option<MouseButton>,
        modifiers: Modifiers,
    },
    Down {
        x: f32,
        y: f32,
        button: MouseButton,
        click_count: u32,
        modifiers: Modifiers,
    },
    Up {
        x: f32,
        y: f32,
        button: MouseButton,
        click_count: u32,
        modifiers: Modifiers,
    },
    Wheel {
        x: f32,
        y: f32,
        /// Pixels to scroll; positive `dy` scrolls content down.
        delta_x: f32,
        delta_y: f32,
        modifiers: Modifiers,
    },
    Leave,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    /// GPUI's normalized key name.
    pub key: String,
    /// The text the key would type, if any.
    pub key_char: Option<String>,
    pub modifiers: Modifiers,
}

// Chromium's `cef_event_flags_t` bits.
pub const EVENTFLAG_SHIFT_DOWN: u32 = 1 << 1;
pub const EVENTFLAG_CONTROL_DOWN: u32 = 1 << 2;
pub const EVENTFLAG_ALT_DOWN: u32 = 1 << 3;
pub const EVENTFLAG_LEFT_MOUSE_BUTTON: u32 = 1 << 4;
pub const EVENTFLAG_MIDDLE_MOUSE_BUTTON: u32 = 1 << 5;
pub const EVENTFLAG_RIGHT_MOUSE_BUTTON: u32 = 1 << 6;
pub const EVENTFLAG_COMMAND_DOWN: u32 = 1 << 7;

pub fn modifier_flags(modifiers: &Modifiers) -> u32 {
    let mut flags = 0;
    if modifiers.shift {
        flags |= EVENTFLAG_SHIFT_DOWN;
    }
    if modifiers.control {
        flags |= EVENTFLAG_CONTROL_DOWN;
    }
    if modifiers.alt {
        flags |= EVENTFLAG_ALT_DOWN;
    }
    if modifiers.command {
        flags |= EVENTFLAG_COMMAND_DOWN;
    }
    flags
}

pub fn button_flag(button: Option<MouseButton>) -> u32 {
    match button {
        Some(MouseButton::Left) => EVENTFLAG_LEFT_MOUSE_BUTTON,
        Some(MouseButton::Middle) => EVENTFLAG_MIDDLE_MOUSE_BUTTON,
        Some(MouseButton::Right) => EVENTFLAG_RIGHT_MOUSE_BUTTON,
        None => 0,
    }
}

/// Whether a key should wait for the platform input method to decide if it
/// produces text. Printable keys without shortcut modifiers do: their text
/// arrives through the IME path (which also handles dead keys and CJK
/// composition), and sending a CHAR event for them as well would type twice.
/// Return and Tab carry text in GPUI but AppKit treats them as commands, so
/// they must be sent as raw keys.
pub fn waits_for_text_input(key: &KeyInput) -> bool {
    let m = key.modifiers;
    !m.control
        && !m.command
        && !m.function
        && key
            .key_char
            .as_deref()
            .is_some_and(|text| !text.is_empty() && text.chars().all(|c| !c.is_control()))
}

/// GPUI key name to Windows virtual-key code; 0 when unknown.
pub fn windows_key_code(key: &str) -> i32 {
    match key {
        "backspace" => 0x08,
        "tab" => 0x09,
        "enter" => 0x0D,
        "shift" => 0x10,
        "ctrl" | "control" => 0x11,
        "alt" => 0x12,
        "capslock" => 0x14,
        "escape" => 0x1B,
        "space" => 0x20,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "cmd" | "platform" => 0x5B,
        ";" => 0xBA,
        "=" => 0xBB,
        "," => 0xBC,
        "-" => 0xBD,
        "." => 0xBE,
        "/" => 0xBF,
        "`" => 0xC0,
        "[" => 0xDB,
        "\\" => 0xDC,
        "]" => 0xDD,
        "'" => 0xDE,
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => c.to_ascii_uppercase() as i32,
                (Some('f'), Some(_)) => match key[1..].parse::<i32>() {
                    Ok(n) if (1..=24).contains(&n) => 0x70 + n - 1,
                    _ => 0,
                },
                _ => 0,
            }
        }
    }
}

/// GPUI key name to the macOS hardware key code (`kVK_*`); 0 when unknown.
pub fn mac_key_code(key: &str) -> i32 {
    match key {
        "a" => 0, "s" => 1, "d" => 2, "f" => 3, "h" => 4, "g" => 5, "z" => 6, "x" => 7,
        "c" => 8, "v" => 9, "b" => 11, "q" => 12, "w" => 13, "e" => 14, "r" => 15,
        "y" => 16, "t" => 17, "1" => 18, "2" => 19, "3" => 20, "4" => 21, "6" => 22,
        "5" => 23, "=" => 24, "9" => 25, "7" => 26, "-" => 27, "8" => 28, "0" => 29,
        "]" => 30, "o" => 31, "u" => 32, "[" => 33, "i" => 34, "p" => 35, "enter" => 36,
        "l" => 37, "j" => 38, "'" => 39, "k" => 40, ";" => 41, "\\" => 42, "," => 43,
        "/" => 44, "n" => 45, "m" => 46, "." => 47, "tab" => 48, "space" => 49, "`" => 50,
        "backspace" => 51, "escape" => 53, "cmd" | "platform" => 55, "shift" => 56,
        "capslock" => 57, "alt" => 58, "ctrl" | "control" => 59,
        "f5" => 96, "f6" => 97, "f7" => 98, "f3" => 99, "f8" => 100, "f9" => 101,
        "f11" => 103, "f13" => 105, "f16" => 106, "f14" => 107, "f10" => 109, "f12" => 111,
        "f15" => 113, "insert" => 114, "home" => 115, "pageup" => 116, "delete" => 117,
        "f4" => 118, "end" => 119, "f2" => 120, "pagedown" => 121, "f1" => 122,
        "left" => 123, "right" => 124, "down" => 125, "up" => 126,
        _ => 0,
    }
}

/// The first UTF-16 code unit of `text` (0 for empty), which is what CEF's
/// key events carry as `character`.
pub fn first_utf16(text: &str) -> u16 {
    text.encode_utf16().next().unwrap_or_default()
}

/// UTF-16 code units for committed text, with AppKit's LF for Return turned
/// into the CR Chromium's keyboard path expects.
pub fn text_char_codes(text: &str) -> Vec<u16> {
    text.encode_utf16()
        .map(|unit| if unit == b'\n' as u16 { b'\r' as u16 } else { unit })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, text: Option<&str>, modifiers: Modifiers) -> KeyInput {
        KeyInput {
            key: name.into(),
            key_char: text.map(Into::into),
            modifiers,
        }
    }

    #[test]
    fn flags_combine() {
        let flags = modifier_flags(&Modifiers { shift: true, command: true, ..Default::default() })
            | button_flag(Some(MouseButton::Left));
        assert_eq!(flags, EVENTFLAG_SHIFT_DOWN | EVENTFLAG_COMMAND_DOWN | EVENTFLAG_LEFT_MOUSE_BUTTON);
        assert_eq!(button_flag(None), 0);
    }

    #[test]
    fn printable_keys_wait_for_the_input_method() {
        assert!(waits_for_text_input(&key("a", Some("a"), Modifiers::default())));
        assert!(waits_for_text_input(&key("space", Some(" "), Modifiers::default())));
        assert!(!waits_for_text_input(&key("enter", Some("\n"), Modifiers::default())));
        assert!(!waits_for_text_input(&key("tab", Some("\t"), Modifiers::default())));
        assert!(!waits_for_text_input(&key("left", None, Modifiers::default())));
        let cmd = Modifiers { command: true, ..Default::default() };
        assert!(!waits_for_text_input(&key("a", Some("a"), cmd)));
    }

    #[test]
    fn virtual_key_codes() {
        assert_eq!(windows_key_code("enter"), 0x0D);
        assert_eq!(windows_key_code("z"), 'Z' as i32);
        assert_eq!(windows_key_code("7"), '7' as i32);
        assert_eq!(windows_key_code("f12"), 0x7B);
        assert_eq!(windows_key_code("f99"), 0);
        assert_eq!(windows_key_code("あ"), 0);
        for named in ["backspace", "delete", "escape", "left", "right", "up", "down", "home", "end", "pageup", "pagedown"] {
            assert_ne!(windows_key_code(named), 0, "{named}");
            assert_ne!(mac_key_code(named), 0, "{named}");
        }
        assert_eq!(mac_key_code("x"), 7);
    }

    #[test]
    fn text_is_utf16_and_return_is_cr() {
        assert_eq!(text_char_codes("a\n"), vec!['a' as u16, '\r' as u16]);
        assert_eq!(text_char_codes("日本").len(), 2);
        assert_eq!(text_char_codes("\u{1F600}").len(), 2, "astral chars are surrogate pairs");
        assert_eq!(first_utf16(""), 0);
    }
}
