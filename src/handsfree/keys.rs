//! Key names shared by the release hotkey and `windows_input` chords.
//!
//! Virtual-key codes are layout independent (`VK_*`).

use crate::ui::UiError;

/// Chord token -> virtual-key code.
pub fn vk_of(name: &str) -> Option<u16> {
    let n = name;
    let vk = match n {
        "ctrl" | "control" => 0x11,
        "alt" => 0x12,
        "shift" => 0x10,
        "win" | "lwin" => 0x5B,
        "rwin" => 0x5C,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "space" => 0x20,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "capslock" => 0x14,
        "numlock" => 0x90,
        "printscreen" => 0x2C,
        "pause" => 0x13,
        "apps" | "menu" => 0x5D,
        _ => return fkey_or_alnum(n),
    };
    Some(vk)
}

fn fkey_or_alnum(n: &str) -> Option<u16> {
    if let Some(d) = n.strip_prefix('f')
        && let Ok(i) = d.parse::<u16>()
        && (1..=24).contains(&i)
        && !d.starts_with('0')
        && !d.starts_with('+')
    {
        return Some(0x70 + i - 1);
    }
    let mut cs = n.chars();
    match (cs.next(), cs.next()) {
        (Some(c @ 'a'..='z'), None) => Some(0x41 + (c as u16 - 'a' as u16)),
        (Some(c @ '0'..='9'), None) => Some(0x30 + (c as u16 - '0' as u16)),
        _ => None,
    }
}

/// Keys that need `KEYEVENTF_EXTENDEDKEY`.
pub fn is_extended(vk: u16) -> bool {
    matches!(
        vk,
        0x2E | 0x2D
            | 0x24
            | 0x23
            | 0x21
            | 0x22
            | 0x25
            | 0x26
            | 0x27
            | 0x28
            | 0x5B
            | 0x5C
            | 0x90
            | 0x2C
            | 0x5D
            | 0x91
    )
}

pub fn is_modifier(vk: u16) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C)
}

/// One `key` chord (`"ctrl+shift+s"`): modifiers then the main key.
/// Errors are `invalid_arguments`.
pub fn parse_chord(chord: &str) -> Result<Vec<u16>, UiError> {
    let parts: Vec<String> = chord
        .to_lowercase()
        .split('+')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(UiError::new("invalid_arguments", "empty key chord"));
    }
    parts
        .iter()
        .map(|p| {
            vk_of(p).ok_or_else(|| UiError::new("invalid_arguments", format!("unknown key '{p}'")))
        })
        .collect()
}

/// Release hotkey bit masks (session hooks): ctrl 1, alt 2, shift 4, win 8.
pub const MOD_CTRL: u32 = 1;
pub const MOD_ALT: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
pub const MOD_WIN: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u16,
    /// `Ctrl+Alt+Shift+H`.
    pub label: String,
}

const HOTKEY_NAMED: &[&str] = &[
    "esc",
    "escape",
    "space",
    "enter",
    "tab",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "pause",
    "pageup",
    "pagedown",
];

fn hotkey_key_ok(k: &str) -> bool {
    HOTKEY_NAMED.contains(&k) || fkey_or_alnum(k).is_some()
}

fn bad(m: &str) -> UiError {
    UiError::new("invalid_arguments", m)
}

/// Parse a release hotkey (at least two modifiers plus one key).
pub fn parse_hotkey(text: &str) -> Result<Hotkey, UiError> {
    let parts: Vec<String> = text
        .split('+')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 {
        return Err(bad(
            "release_hotkey needs modifiers plus one key, e.g. ctrl+alt+shift+h",
        ));
    }
    let (mods, key) = parts.split_at(parts.len() - 1);
    let key = &key[0];
    let mut bits = 0u32;
    for m in mods {
        bits |= match m.as_str() {
            "ctrl" | "control" => MOD_CTRL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" => MOD_WIN,
            _ => {
                return Err(bad(
                    "release_hotkey modifiers must be ctrl, alt, shift or win",
                ));
            }
        };
    }
    if bits.count_ones() < 2 {
        return Err(bad(
            "release_hotkey needs at least two different modifiers so it cannot be pressed by accident",
        ));
    }
    if !hotkey_key_ok(key) {
        return Err(bad(
            "release_hotkey key must be a letter, digit, f1-f24, esc, space, enter, tab, backspace, delete, insert, home, end, pause, pageup or pagedown",
        ));
    }
    let vk = vk_of(key).expect("validated key has a VK");
    let label = parts
        .iter()
        .map(|p| {
            if p.chars().count() == 1 {
                p.to_uppercase()
            } else {
                let mut c = p.chars();
                let first = c.next().map(|f| f.to_uppercase().collect::<String>());
                first.unwrap_or_default() + c.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join("+");
    Ok(Hotkey {
        mods: bits,
        vk,
        label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_default_and_labels() {
        let h = parse_hotkey("ctrl+alt+shift+h").unwrap();
        assert_eq!(h.label, "Ctrl+Alt+Shift+H");
        assert_eq!(h.vk, 0x48);
        assert_eq!(h.mods, MOD_CTRL | MOD_ALT | MOD_SHIFT);
        let h = parse_hotkey(" Control + WIN + f12 ").unwrap();
        assert_eq!(h.label, "Control+Win+F12");
        assert_eq!(h.vk, 0x7B);
        assert_eq!(h.mods, MOD_CTRL | MOD_WIN);
        assert_eq!(parse_hotkey("ctrl+alt+pagedown").unwrap().vk, 0x22);
        assert_eq!(parse_hotkey("ctrl+alt+0").unwrap().vk, 0x30);
    }

    #[test]
    fn hotkey_rejections() {
        let msg = |t: &str| parse_hotkey(t).unwrap_err().message;
        assert!(msg("h").contains("needs modifiers plus one key"));
        assert!(msg("ctrl++").contains("needs modifiers plus one key"));
        assert!(msg("ctrl+meta+h").contains("must be ctrl, alt, shift or win"));
        assert!(msg("ctrl+control+h").contains("at least two different modifiers"));
        assert!(msg("ctrl+h").contains("at least two different modifiers"));
        assert!(msg("ctrl+alt+left").contains("key must be a letter"));
        assert!(msg("ctrl+alt+f25").contains("key must be a letter"));
        assert!(msg("ctrl+alt+shift").contains("key must be a letter"));
        assert_eq!(parse_hotkey("x").unwrap_err().kind, "invalid_arguments");
    }

    #[test]
    fn chords_are_layout_independent_vks() {
        assert_eq!(parse_chord("Ctrl+Shift+S").unwrap(), vec![0x11, 0x10, 0x53]);
        assert_eq!(parse_chord("alt+f4").unwrap(), vec![0x12, 0x73]);
        assert_eq!(parse_chord("win+r").unwrap(), vec![0x5B, 0x52]);
        assert_eq!(parse_chord("ctrl+").unwrap(), vec![0x11]);
        assert_eq!(parse_chord("+").unwrap_err().message, "empty key chord");
        assert_eq!(
            parse_chord("ctrl+foo").unwrap_err().message,
            "unknown key 'foo'"
        );
        assert!(parse_chord("f01").is_err());
        assert!(is_extended(0x25) && !is_extended(0x41));
    }
}
