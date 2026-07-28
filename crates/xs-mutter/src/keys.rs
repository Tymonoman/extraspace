//! Linux evdev keycodes and chord parsing.
//!
//! Mutter's `NotifyKeyboardKeycode` takes raw **evdev** codes, not X11 keycodes.
//! The two differ by a constant 8 (X11 = evdev + 8), and mixing them up produces
//! keystrokes that are plausible but wrong -- `a` arriving as `Escape` and so on
//! -- so the distinction is worth stating rather than leaving to be rediscovered.

/// Codes from `linux/input-event-codes.h`.
pub mod code {
    pub const LEFTSHIFT: u32 = 42;
    pub const LEFTCTRL: u32 = 29;
    pub const LEFTALT: u32 = 56;
    pub const LEFTMETA: u32 = 125;

    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const UP: u32 = 103;
    pub const DOWN: u32 = 108;

    pub const ENTER: u32 = 28;
    pub const ESC: u32 = 1;
    pub const TAB: u32 = 15;
    pub const SPACE: u32 = 57;
}

/// A parsed key combination: modifiers to hold, and the key to press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Vec<u32>,
    pub key: u32,
}

/// Parses `"super+shift+right"` into evdev codes.
///
/// Returns `None` for anything unrecognised rather than guessing, since a wrong
/// keycode injected into the user's session is worse than doing nothing.
pub fn parse_chord(spec: &str) -> Option<Chord> {
    let parts: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let (key_name, modifier_names) = parts.split_last()?;

    let modifiers = modifier_names
        .iter()
        .map(|m| match m.to_ascii_lowercase().as_str() {
            "shift" => Some(code::LEFTSHIFT),
            "ctrl" | "control" => Some(code::LEFTCTRL),
            "alt" => Some(code::LEFTALT),
            "super" | "meta" | "win" => Some(code::LEFTMETA),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;

    let key = key_code(key_name)?;
    Some(Chord { modifiers, key })
}

fn key_code(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "left" => code::LEFT,
        "right" => code::RIGHT,
        "up" => code::UP,
        "down" => code::DOWN,
        "enter" | "return" => code::ENTER,
        "esc" | "escape" => code::ESC,
        "tab" => code::TAB,
        "space" => code::SPACE,
        // A lone modifier is a valid chord: "super" opens the overview.
        "super" | "meta" | "win" => code::LEFTMETA,
        "shift" => code::LEFTSHIFT,
        "ctrl" | "control" => code::LEFTCTRL,
        "alt" => code::LEFTALT,
        // Letters, using the QWERTY row layout evdev actually uses.
        s if s.len() == 1 => {
            let c = s.chars().next()?;
            const ROW1: &str = "qwertyuiop";
            const ROW2: &str = "asdfghjkl";
            const ROW3: &str = "zxcvbnm";
            if let Some(i) = ROW1.find(c) {
                16 + i as u32
            } else if let Some(i) = ROW2.find(c) {
                30 + i as u32
            } else if let Some(i) = ROW3.find(c) {
                44 + i as u32
            } else if c.is_ascii_digit() {
                // '1'..'9' are 2..10, and '0' is 11.
                match c {
                    '0' => 11,
                    d => 1 + d.to_digit(10)?,
                }
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_move_window_shortcut() {
        let c = parse_chord("super+shift+right").unwrap();
        assert_eq!(c.modifiers, vec![code::LEFTMETA, code::LEFTSHIFT]);
        assert_eq!(c.key, code::RIGHT);
    }

    #[test]
    fn parses_a_lone_modifier() {
        let c = parse_chord("super").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.key, code::LEFTMETA);
    }

    #[test]
    fn letters_use_evdev_positions_not_alphabetical_order() {
        // 'q' is the first key of the top row, not the 17th letter.
        assert_eq!(parse_chord("q").unwrap().key, 16);
        assert_eq!(parse_chord("a").unwrap().key, 30);
        assert_eq!(parse_chord("z").unwrap().key, 44);
    }

    #[test]
    fn digits_are_offset_because_1_is_not_0() {
        assert_eq!(parse_chord("1").unwrap().key, 2);
        assert_eq!(parse_chord("9").unwrap().key, 10);
        assert_eq!(parse_chord("0").unwrap().key, 11);
    }

    #[test]
    fn rejects_nonsense_rather_than_guessing() {
        // Injecting a wrong keycode into a live session is worse than nothing.
        assert!(parse_chord("hyper+right").is_none());
        assert!(parse_chord("super+nope").is_none());
        assert!(parse_chord("").is_none());
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(
            parse_chord("SUPER+Shift+RIGHT"),
            parse_chord("super+shift+right")
        );
    }
}
