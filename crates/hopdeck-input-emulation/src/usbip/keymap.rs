//! evdev (Linux input-event-codes.h) → USB HID usage table translation.
//!
//! HopDeck's wire format carries raw evdev key codes
//! (`input_event::scancode::Linux` values as u32); a USB HID keyboard
//! reports Usage Page 0x07 (Keyboard/Keypad) usage ids. The mapping below
//! covers the codes produced by HopDeck's own capture backends; unknown
//! codes are dropped with a warning instead of being forwarded as garbage
//! reports. Media / consumer-page keys are out of scope for the PoC.

/// Translate an evdev key code to a HID keyboard usage id.
pub fn linux_to_usage(code: u32) -> Option<u8> {
    match code {
        // letters (evdev codes are QWERTY-ordered, HID usages are linear)
        30 => Some(0x04), // A
        48 => Some(0x05), // B
        46 => Some(0x06), // C
        32 => Some(0x07), // D
        18 => Some(0x08), // E
        33 => Some(0x09), // F
        34 => Some(0x0A), // G
        35 => Some(0x0B), // H
        23 => Some(0x0C), // I
        36 => Some(0x0D), // J
        37 => Some(0x0E), // K
        38 => Some(0x0F), // L
        50 => Some(0x10), // M
        49 => Some(0x11), // N
        24 => Some(0x12), // O
        25 => Some(0x13), // P
        16 => Some(0x14), // Q
        19 => Some(0x15), // R
        31 => Some(0x16), // S
        20 => Some(0x17), // T
        22 => Some(0x18), // U
        47 => Some(0x19), // V
        17 => Some(0x1A), // W
        45 => Some(0x1B), // X
        21 => Some(0x1C), // Y
        44 => Some(0x1D), // Z

        // digits row
        2 => Some(0x1E),  // 1
        3 => Some(0x1F),  // 2
        4 => Some(0x20),  // 3
        5 => Some(0x21),  // 4
        6 => Some(0x22),  // 5
        7 => Some(0x23),  // 6
        8 => Some(0x24),  // 7
        9 => Some(0x25),  // 8
        10 => Some(0x26), // 9
        11 => Some(0x27), // 0

        // control block
        28 => Some(0x28), // Enter
        1 => Some(0x29),  // Esc
        14 => Some(0x2A), // Backspace
        15 => Some(0x2B), // Tab
        57 => Some(0x2C), // Space
        12 => Some(0x2D), // Minus
        13 => Some(0x2E), // Equal
        26 => Some(0x2F), // LeftBrace
        27 => Some(0x30), // RightBrace
        43 => Some(0x31), // Backslash
        39 => Some(0x33), // Semicolon
        40 => Some(0x34), // Apostrophe
        41 => Some(0x35), // Grave
        51 => Some(0x36), // Comma
        52 => Some(0x37), // Dot
        53 => Some(0x38), // Slash
        58 => Some(0x39), // CapsLock

        // function row: F1..F10 are linear, F11/F12 are not
        59..=68 => Some(0x3A + (code - 59) as u8), // F1..F10
        87 => Some(0x44),                          // F11
        88 => Some(0x45),                          // F12
        183..=194 => Some(0x68 + (code - 183) as u8), // F13..F24

        // lock / screen keys
        99 => Some(0x46),  // PrintScreen
        70 => Some(0x47),  // ScrollLock
        119 => Some(0x48), // Pause

        // navigation block
        110 => Some(0x49), // Insert
        102 => Some(0x4A), // Home
        104 => Some(0x4B), // PageUp
        111 => Some(0x4C), // Delete
        107 => Some(0x4D), // End
        109 => Some(0x4E), // PageDown
        106 => Some(0x4F), // Right
        105 => Some(0x50), // Left
        108 => Some(0x51), // Down
        103 => Some(0x52), // Up

        // keypad
        69 => Some(0x53),  // NumLock
        98 => Some(0x54),  // KP /
        55 => Some(0x55),  // KP *
        74 => Some(0x56),  // KP -
        78 => Some(0x57),  // KP +
        96 => Some(0x58),  // KP Enter
        79 => Some(0x59),  // KP 1
        80 => Some(0x5A),  // KP 2
        81 => Some(0x5B),  // KP 3
        75 => Some(0x5C),  // KP 4
        76 => Some(0x5D),  // KP 5
        77 => Some(0x5E),  // KP 6
        71 => Some(0x5F),  // KP 7
        72 => Some(0x60),  // KP 8
        73 => Some(0x61),  // KP 9
        82 => Some(0x62),  // KP 0
        83 => Some(0x63),  // KP .
        124 => Some(0x64), // 102ND (non-US backslash)
        127 => Some(0x65), // Compose
        116 => Some(0x66), // Power
        117 => Some(0x67), // KP =

        // modifiers
        29 => Some(0xE0),  // LeftCtrl
        42 => Some(0xE1),  // LeftShift
        56 => Some(0xE2),  // LeftAlt
        125 => Some(0xE3), // LeftMeta
        97 => Some(0xE4),  // RightCtrl
        54 => Some(0xE5),  // RightShift
        100 => Some(0xE6), // RightAlt
        126 => Some(0xE7), // RightMeta

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::linux_to_usage;

    #[test]
    fn letters_follow_qwerty_not_alphabetical_order() {
        assert_eq!(linux_to_usage(30), Some(0x04)); // KEY_A
        assert_eq!(linux_to_usage(31), Some(0x16)); // KEY_S is not 0x05
        assert_eq!(linux_to_usage(44), Some(0x1D)); // KEY_Z
    }

    #[test]
    fn digits_and_control_block() {
        assert_eq!(linux_to_usage(2), Some(0x1E)); // KEY_1
        assert_eq!(linux_to_usage(11), Some(0x27)); // KEY_0
        assert_eq!(linux_to_usage(28), Some(0x28)); // Enter
        assert_eq!(linux_to_usage(1), Some(0x29)); // Esc
        assert_eq!(linux_to_usage(57), Some(0x2C)); // Space
    }

    #[test]
    fn function_row_is_linear_until_f10_then_jumps() {
        assert_eq!(linux_to_usage(59), Some(0x3A)); // F1
        assert_eq!(linux_to_usage(68), Some(0x43)); // F10
        assert_eq!(linux_to_usage(87), Some(0x44)); // F11 (evdev 87, not 69)
        assert_eq!(linux_to_usage(88), Some(0x45)); // F12 (evdev 88, not 70)
        assert_eq!(linux_to_usage(69), Some(0x53)); // evdev 69 is NumLock, not F11
    }

    #[test]
    fn modifiers_cover_both_sides() {
        assert_eq!(linux_to_usage(29), Some(0xE0)); // LeftCtrl
        assert_eq!(linux_to_usage(42), Some(0xE1)); // LeftShift
        assert_eq!(linux_to_usage(125), Some(0xE3)); // LeftMeta
        assert_eq!(linux_to_usage(97), Some(0xE4)); // RightCtrl
        assert_eq!(linux_to_usage(126), Some(0xE7)); // RightMeta
    }

    #[test]
    fn unknown_codes_are_dropped() {
        assert_eq!(linux_to_usage(0), None);
        assert_eq!(linux_to_usage(u32::MAX), None);
    }
}
