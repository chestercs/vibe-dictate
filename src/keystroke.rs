//! Parse a transcribed utterance as a single keystroke combo and send
//! it via SendInput.
//!
//! The intent is an "interactive" dictation mode: if the user says only
//! a key name ("escape") or a combo ("control shift s"), we press that
//! combo instead of pasting the raw text. Anything that doesn't cleanly
//! parse as a combo falls back to the normal text-injection path —
//! that's how "kizárólag egy parseolható valódi billentyű kombináció"
//! stays the trigger condition.

use anyhow::Result;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_0, VK_1, VK_2, VK_3, VK_4, VK_5, VK_6, VK_7, VK_8, VK_9, VK_A, VK_B,
    VK_BACK, VK_C, VK_CAPITAL, VK_CONTROL, VK_D, VK_DELETE, VK_DOWN, VK_E, VK_END, VK_ESCAPE,
    VK_F, VK_F1, VK_F10, VK_F11, VK_F12, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8,
    VK_F9, VK_G, VK_H, VK_HOME, VK_I, VK_INSERT, VK_J, VK_K, VK_L, VK_LEFT, VK_LWIN, VK_M,
    VK_MENU, VK_N, VK_NEXT, VK_O, VK_P, VK_PAUSE, VK_PRIOR, VK_Q, VK_R, VK_RETURN, VK_RIGHT,
    VK_S, VK_SCROLL, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_T, VK_TAB, VK_U, VK_UP, VK_V, VK_W,
    VK_X, VK_Y, VK_Z,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifierMask {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

impl ModifierMask {
    pub const fn empty() -> Self {
        Self {
            ctrl: false,
            shift: false,
            alt: false,
            win: false,
        }
    }
    pub fn is_empty(self) -> bool {
        !self.ctrl && !self.shift && !self.alt && !self.win
    }
    fn with_ctrl(mut self) -> Self { self.ctrl = true; self }
    fn with_shift(mut self) -> Self { self.shift = true; self }
    fn with_alt(mut self) -> Self { self.alt = true; self }
    fn with_win(mut self) -> Self { self.win = true; self }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    /// A named key (Escape, F1, Enter, PageUp…). Always unambiguous.
    Named,
    /// A single letter or digit. Ambiguous when bare — "a" could be the
    /// letter being typed or the Alt+A of a combo. We refuse to treat
    /// bare alphanumerics as a keystroke.
    Alpha,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keystroke {
    pub mods: ModifierMask,
    pub vk: VIRTUAL_KEY,
}

/// Parse free-form speech as a keystroke. Returns `None` when the text
/// doesn't cleanly map to exactly one key (+ optional modifiers), so the
/// caller can fall back to pasting the text as-is.
///
/// Tolerant of sentence punctuation ("Escape.") and case, but strict
/// about structure — any unrecognized token bails.
pub fn parse_speech_keystroke(text: &str) -> Option<Keystroke> {
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }

    // Drop sentence punctuation and normalize separators so "Ctrl+Shift+S"
    // and "Ctrl Shift S" and "control, shift, s." all tokenize the same.
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '.' | ',' | '!' | '?' | ';' | ':' | '+' | '-' => ' ',
            other => other,
        })
        .collect();
    let lower = cleaned.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }

    let mut mods = ModifierMask::empty();
    let mut key: Option<(VIRTUAL_KEY, KeyKind)> = None;
    let mut i = 0;
    while i < tokens.len() {
        // Try two-word named keys first ("page up", "caps lock", "print
        // screen"). Concatenate the two tokens and look them up; fall
        // through to single-token handling when no match.
        if i + 1 < tokens.len() {
            let joined = format!("{}{}", tokens[i], tokens[i + 1]);
            if let Some(vk) = lookup_named_key(&joined) {
                if key.is_some() {
                    return None;
                }
                key = Some((vk, KeyKind::Named));
                i += 2;
                continue;
            }
        }

        if let Some(m) = lookup_modifier(tokens[i]) {
            mods = apply_mod(mods, m);
            i += 1;
            continue;
        }

        if let Some(vk) = lookup_named_key(tokens[i]) {
            if key.is_some() {
                return None;
            }
            key = Some((vk, KeyKind::Named));
            i += 1;
            continue;
        }

        if tokens[i].chars().count() == 1 {
            let ch = tokens[i].chars().next().unwrap();
            if let Some(vk) = letter_or_digit_vk(ch) {
                if key.is_some() {
                    return None;
                }
                key = Some((vk, KeyKind::Alpha));
                i += 1;
                continue;
            }
        }

        // Any unknown token means this isn't cleanly a keystroke — bail.
        return None;
    }

    let (vk, kind) = key?;
    // A bare alphanumeric is too ambiguous to hijack — if the user just
    // said "a", they probably wanted to type "a". Require a modifier or
    // a named key to opt in.
    if mods.is_empty() && kind == KeyKind::Alpha {
        return None;
    }
    Some(Keystroke { mods, vk })
}

/// Which modifier a token represents. Mapped onto ModifierMask via
/// apply_mod so ModifierMask itself stays a plain flags struct.
#[derive(Debug, Clone, Copy)]
enum ModToken { Ctrl, Shift, Alt, Win }

fn apply_mod(mods: ModifierMask, m: ModToken) -> ModifierMask {
    match m {
        ModToken::Ctrl => mods.with_ctrl(),
        ModToken::Shift => mods.with_shift(),
        ModToken::Alt => mods.with_alt(),
        ModToken::Win => mods.with_win(),
    }
}

fn lookup_modifier(tok: &str) -> Option<ModToken> {
    match tok {
        "ctrl" | "control" => Some(ModToken::Ctrl),
        "shift" => Some(ModToken::Shift),
        "alt" | "option" => Some(ModToken::Alt),
        "win" | "windows" | "super" | "meta" | "cmd" | "command" => Some(ModToken::Win),
        _ => None,
    }
}

fn lookup_named_key(tok: &str) -> Option<VIRTUAL_KEY> {
    Some(match tok {
        "escape" | "esc" => VK_ESCAPE,
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "space" | "spacebar" => VK_SPACE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" | "pgdown" => VK_NEXT,
        "up" | "uparrow" | "arrowup" => VK_UP,
        "down" | "downarrow" | "arrowdown" => VK_DOWN,
        "left" | "leftarrow" | "arrowleft" => VK_LEFT,
        "right" | "rightarrow" | "arrowright" => VK_RIGHT,
        "capslock" | "caps" => VK_CAPITAL,
        "scrolllock" | "scroll" => VK_SCROLL,
        "numlock" => VIRTUAL_KEY(0x90),
        "pause" | "break" => VK_PAUSE,
        "printscreen" | "prtsc" | "prtscn" | "sysrq" => VK_SNAPSHOT,
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        _ => return None,
    })
}

fn letter_or_digit_vk(ch: char) -> Option<VIRTUAL_KEY> {
    Some(match ch.to_ascii_lowercase() {
        'a' => VK_A,
        'b' => VK_B,
        'c' => VK_C,
        'd' => VK_D,
        'e' => VK_E,
        'f' => VK_F,
        'g' => VK_G,
        'h' => VK_H,
        'i' => VK_I,
        'j' => VK_J,
        'k' => VK_K,
        'l' => VK_L,
        'm' => VK_M,
        'n' => VK_N,
        'o' => VK_O,
        'p' => VK_P,
        'q' => VK_Q,
        'r' => VK_R,
        's' => VK_S,
        't' => VK_T,
        'u' => VK_U,
        'v' => VK_V,
        'w' => VK_W,
        'x' => VK_X,
        'y' => VK_Y,
        'z' => VK_Z,
        '0' => VK_0,
        '1' => VK_1,
        '2' => VK_2,
        '3' => VK_3,
        '4' => VK_4,
        '5' => VK_5,
        '6' => VK_6,
        '7' => VK_7,
        '8' => VK_8,
        '9' => VK_9,
        _ => return None,
    })
}

/// Press modifiers (in order), tap the key, then release modifiers in
/// reverse order. Batched into a single SendInput call so the target
/// window sees the combo atomically.
pub fn send_combo(combo: Keystroke) -> Result<()> {
    let mut events: Vec<INPUT> = Vec::with_capacity(10);
    if combo.mods.ctrl {
        events.push(make_vk_input(VK_CONTROL, false));
    }
    if combo.mods.shift {
        events.push(make_vk_input(VK_SHIFT, false));
    }
    if combo.mods.alt {
        events.push(make_vk_input(VK_MENU, false));
    }
    if combo.mods.win {
        events.push(make_vk_input(VK_LWIN, false));
    }
    events.push(make_vk_input(combo.vk, false));
    events.push(make_vk_input(combo.vk, true));
    if combo.mods.win {
        events.push(make_vk_input(VK_LWIN, true));
    }
    if combo.mods.alt {
        events.push(make_vk_input(VK_MENU, true));
    }
    if combo.mods.shift {
        events.push(make_vk_input(VK_SHIFT, true));
    }
    if combo.mods.ctrl {
        events.push(make_vk_input(VK_CONTROL, true));
    }

    let cbsize = std::mem::size_of::<INPUT>() as i32;
    let n = unsafe { SendInput(&events, cbsize) };
    if (n as usize) < events.len() {
        let err = unsafe { GetLastError() };
        log::warn!(
            "SendInput combo partial: {}/{} events (err {:?}) — focused window may block injected input",
            n,
            events.len(),
            err
        );
    }
    Ok(())
}

fn make_vk_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_named_key() {
        let k = parse_speech_keystroke("escape").unwrap();
        assert_eq!(k.mods, ModifierMask::empty());
        assert_eq!(k.vk, VK_ESCAPE);
    }

    #[test]
    fn parses_with_trailing_punctuation() {
        assert!(parse_speech_keystroke("Escape.").is_some());
        assert!(parse_speech_keystroke("enter!").is_some());
    }

    #[test]
    fn parses_combo() {
        let k = parse_speech_keystroke("Ctrl Shift S").unwrap();
        assert!(k.mods.ctrl);
        assert!(k.mods.shift);
        assert_eq!(k.vk, VK_S);
    }

    #[test]
    fn parses_plus_separator() {
        let k = parse_speech_keystroke("Ctrl+Alt+Del").unwrap();
        assert!(k.mods.ctrl);
        assert!(k.mods.alt);
        assert_eq!(k.vk, VK_DELETE);
    }

    #[test]
    fn parses_two_word_key() {
        let k = parse_speech_keystroke("page up").unwrap();
        assert_eq!(k.vk, VK_PRIOR);
    }

    #[test]
    fn rejects_bare_letter() {
        assert!(parse_speech_keystroke("a").is_none());
        assert!(parse_speech_keystroke("1").is_none());
    }

    #[test]
    fn rejects_sentence() {
        assert!(parse_speech_keystroke("hello world").is_none());
        assert!(parse_speech_keystroke("please escape the room").is_none());
    }

    #[test]
    fn rejects_two_keys() {
        assert!(parse_speech_keystroke("escape enter").is_none());
    }
}
