//! Keys in the window, turned into the terminal key events the viewer
//! already answers to, so every binding works the same in both.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gpui::Keystroke;

/// What the window does with a key before the viewer sees it.
#[derive(Debug, PartialEq, Eq)]
pub enum Clipboard {
    Copy,
    Paste,
}

/// Copy and paste the way Linux desktops spell them: Ctrl-C and Ctrl-V,
/// with Shift as terminals take them, and Ctrl-Insert and Shift-Insert,
/// which Omarchy's Super-C and Super-V send.
pub fn clipboard(k: &Keystroke) -> Option<Clipboard> {
    let m = &k.modifiers;
    if m.alt || m.platform {
        return None;
    }
    match (k.key.as_str(), m.control, m.shift) {
        ("c", true, _) | ("insert", true, false) => Some(Clipboard::Copy),
        ("v", true, _) | ("insert", false, true) => Some(Clipboard::Paste),
        _ => None,
    }
}

/// The terminal key event for a keystroke; `None` for keys the viewer has
/// no use for, and for anything with Super held, which is the desktop's.
pub fn translate(k: &Keystroke) -> Option<KeyEvent> {
    let m = &k.modifiers;
    if m.platform {
        return None;
    }
    let mut mods = KeyModifiers::NONE;
    if m.control {
        mods |= KeyModifiers::CONTROL;
    }
    if m.alt {
        mods |= KeyModifiers::ALT;
    }
    if m.shift {
        mods |= KeyModifiers::SHIFT;
    }
    let code = match k.key.as_str() {
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "tab" if m.shift => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        key => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(typed(k, c)),
                _ => match key.strip_prefix('f').and_then(|n| n.parse().ok()) {
                    Some(n) if (1..=24).contains(&n) => KeyCode::F(n),
                    _ => return None,
                },
            }
        }
    };
    Some(KeyEvent::new(code, mods))
}

/// The character a key types. With Ctrl or Alt the key's own character,
/// as terminals report it (Shift makes a letter capital); otherwise what
/// the keyboard layout made of it.
fn typed(k: &Keystroke, key: char) -> char {
    let m = &k.modifiers;
    if m.control || m.alt {
        return if m.shift {
            key.to_ascii_uppercase()
        } else {
            key
        };
    }
    let mut typed = k.key_char.as_deref().unwrap_or("").chars();
    match (typed.next(), typed.next()) {
        (Some(c), None) => c,
        _ if m.shift => key.to_ascii_uppercase(),
        _ => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn stroke(key: &str, key_char: Option<&str>, m: Modifiers) -> Keystroke {
        Keystroke {
            modifiers: m,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    fn plain(key: &str, ch: &str) -> Keystroke {
        stroke(key, Some(ch), Modifiers::none())
    }

    fn code(k: &Keystroke) -> Option<(KeyCode, KeyModifiers)> {
        translate(k).map(|e| (e.code, e.modifiers))
    }

    #[test]
    fn letters_and_symbols_come_through_as_typed() {
        assert_eq!(
            code(&plain("j", "j")),
            Some((KeyCode::Char('j'), KeyModifiers::NONE))
        );
        let shift = Modifiers::shift();
        assert_eq!(
            code(&stroke("g", Some("G"), shift)),
            Some((KeyCode::Char('G'), KeyModifiers::SHIFT))
        );
        // Shift is dropped from symbols: the key is what was typed.
        assert_eq!(
            code(&plain("<", "<")),
            Some((KeyCode::Char('<'), KeyModifiers::NONE))
        );
        assert_eq!(
            code(&plain(":", ":")),
            Some((KeyCode::Char(':'), KeyModifiers::NONE))
        );
        // Another layout's letter, typed on a key GPUI names in ASCII.
        assert_eq!(
            code(&plain("a", "ä")),
            Some((KeyCode::Char('ä'), KeyModifiers::NONE))
        );
    }

    #[test]
    fn control_and_alt_keep_the_key() {
        assert_eq!(
            code(&stroke("k", None, Modifiers::control())),
            Some((KeyCode::Char('k'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            code(&stroke("space", None, Modifiers::control())),
            Some((KeyCode::Char(' '), KeyModifiers::CONTROL))
        );
        assert_eq!(
            code(&stroke("<", Some("<"), Modifiers::alt())),
            Some((KeyCode::Char('<'), KeyModifiers::ALT))
        );
        assert_eq!(
            code(&stroke("left", None, Modifiers::alt())),
            Some((KeyCode::Left, KeyModifiers::ALT))
        );
    }

    #[test]
    fn named_keys() {
        assert_eq!(
            code(&plain("enter", "\n")).map(|c| c.0),
            Some(KeyCode::Enter)
        );
        assert_eq!(code(&plain("escape", "")).map(|c| c.0), Some(KeyCode::Esc));
        assert_eq!(
            code(&plain("space", " ")).map(|c| c.0),
            Some(KeyCode::Char(' '))
        );
        assert_eq!(
            code(&plain("pagedown", "")).map(|c| c.0),
            Some(KeyCode::PageDown)
        );
        assert_eq!(code(&plain("f5", "")).map(|c| c.0), Some(KeyCode::F(5)));
        assert_eq!(
            code(&stroke("tab", None, Modifiers::shift())).map(|c| c.0),
            Some(KeyCode::BackTab)
        );
        assert_eq!(code(&plain("shift", "")), None);
    }

    #[test]
    fn super_is_the_desktops() {
        let m = Modifiers {
            platform: true,
            ..Modifiers::none()
        };
        assert_eq!(code(&stroke("c", None, m)), None);
        assert_eq!(clipboard(&stroke("c", None, m)), None);
    }

    #[test]
    fn clipboard_keys() {
        let ctrl = Modifiers::control();
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Modifiers::none()
        };
        assert_eq!(clipboard(&stroke("c", None, ctrl)), Some(Clipboard::Copy));
        assert_eq!(
            clipboard(&stroke("c", None, ctrl_shift)),
            Some(Clipboard::Copy)
        );
        assert_eq!(
            clipboard(&stroke("insert", None, ctrl)),
            Some(Clipboard::Copy)
        );
        assert_eq!(clipboard(&stroke("v", None, ctrl)), Some(Clipboard::Paste));
        assert_eq!(
            clipboard(&stroke("v", None, ctrl_shift)),
            Some(Clipboard::Paste)
        );
        assert_eq!(
            clipboard(&stroke("insert", None, Modifiers::shift())),
            Some(Clipboard::Paste)
        );
        assert_eq!(clipboard(&plain("c", "c")), None);
        assert_eq!(clipboard(&stroke("v", None, Modifiers::alt())), None);
    }
}
