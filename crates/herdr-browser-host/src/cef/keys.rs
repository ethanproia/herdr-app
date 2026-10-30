//! DOM `code` to the Windows virtual key code Chromium's KeyEvent wants.

pub fn windows_key_code(code: &str, key: &str) -> i32 {
    if let Some(c) = code.strip_prefix("Key").filter(|c| c.len() == 1) {
        return c.as_bytes()[0] as i32; // 'A'..'Z' = 0x41..0x5A
    }
    if let Some(d) = code.strip_prefix("Digit").filter(|d| d.len() == 1) {
        return d.as_bytes()[0] as i32; // '0'..'9' = 0x30..0x39
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<i32>().ok()).filter(|n| (1..=24).contains(n)) {
        return 0x6F + n; // F1 = 0x70
    }
    if let Some(d) = code.strip_prefix("Numpad").and_then(|d| d.parse::<i32>().ok()) {
        return 0x60 + d;
    }
    match code {
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Enter" | "NumpadEnter" => 0x0D,
        "ShiftLeft" | "ShiftRight" => 0x10,
        "ControlLeft" | "ControlRight" => 0x11,
        "AltLeft" | "AltRight" => 0x12,
        "CapsLock" => 0x14,
        "Escape" => 0x1B,
        "Space" => 0x20,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "End" => 0x23,
        "Home" => 0x24,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "Semicolon" => 0xBA,
        "Equal" => 0xBB,
        "Comma" => 0xBC,
        "Minus" => 0xBD,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "BracketLeft" => 0xDB,
        "Backslash" => 0xDC,
        "BracketRight" => 0xDD,
        "Quote" => 0xDE,
        _ => key.chars().next().filter(|c| c.is_ascii_alphanumeric()).map_or(0, |c| c.to_ascii_uppercase() as i32),
    }
}
