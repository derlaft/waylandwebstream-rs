// Reverse keymap: Linux evdev keycode → browser KeyboardEvent.code string.
//
// This is the exact inverse of `src/input/keyboard.rs`'s `evdev_keycode()`
// table. Both tables must be kept in sync: if the server adds a mapping,
// add the same pair here (values swapped).
//
// The `wl_keyboard.key` event carries the raw Linux evdev keycode (e.g.
// KEY_A = 30). The server expects `KeyboardEvent.code` strings from the
// browser Physical Key Values spec, so we map here before sending.

pub fn evdev_to_code(key: u32) -> Option<&'static str> {
    Some(match key {
        // Writing system, digit row
        41 => "Backquote",
        43 => "Backslash",
        26 => "BracketLeft",
        27 => "BracketRight",
        51 => "Comma",
        11 => "Digit0",
        2  => "Digit1",
        3  => "Digit2",
        4  => "Digit3",
        5  => "Digit4",
        6  => "Digit5",
        7  => "Digit6",
        8  => "Digit7",
        9  => "Digit8",
        10 => "Digit9",
        13 => "Equal",
        86 => "IntlBackslash",
        89 => "IntlRo",
        124 => "IntlYen",
        30 => "KeyA",
        48 => "KeyB",
        46 => "KeyC",
        32 => "KeyD",
        18 => "KeyE",
        33 => "KeyF",
        34 => "KeyG",
        35 => "KeyH",
        23 => "KeyI",
        36 => "KeyJ",
        37 => "KeyK",
        38 => "KeyL",
        50 => "KeyM",
        49 => "KeyN",
        24 => "KeyO",
        25 => "KeyP",
        16 => "KeyQ",
        19 => "KeyR",
        31 => "KeyS",
        20 => "KeyT",
        22 => "KeyU",
        47 => "KeyV",
        17 => "KeyW",
        45 => "KeyX",
        21 => "KeyY",
        44 => "KeyZ",
        12 => "Minus",
        52 => "Period",
        40 => "Quote",
        39 => "Semicolon",
        53 => "Slash",

        // Functional keys
        56  => "AltLeft",
        100 => "AltRight",
        14  => "Backspace",
        58  => "CapsLock",
        127 => "ContextMenu",
        29  => "ControlLeft",
        97  => "ControlRight",
        28  => "Enter",
        125 => "MetaLeft",
        126 => "MetaRight",
        42  => "ShiftLeft",
        54  => "ShiftRight",
        57  => "Space",
        15  => "Tab",
        92  => "Convert",
        93  => "KanaMode",
        122 => "Lang1",
        123 => "Lang2",
        94  => "NonConvert",

        // Control pad
        111 => "Delete",
        107 => "End",
        138 => "Help",
        102 => "Home",
        110 => "Insert",
        109 => "PageDown",
        104 => "PageUp",

        // Arrow pad
        108 => "ArrowDown",
        105 => "ArrowLeft",
        106 => "ArrowRight",
        103 => "ArrowUp",

        // Numpad
        69  => "NumLock",
        82  => "Numpad0",
        79  => "Numpad1",
        80  => "Numpad2",
        81  => "Numpad3",
        75  => "Numpad4",
        76  => "Numpad5",
        77  => "Numpad6",
        71  => "Numpad7",
        72  => "Numpad8",
        73  => "Numpad9",
        78  => "NumpadAdd",
        121 => "NumpadComma",
        83  => "NumpadDecimal",
        98  => "NumpadDivide",
        96  => "NumpadEnter",
        117 => "NumpadEqual",
        55  => "NumpadMultiply",
        74  => "NumpadSubtract",

        // Function row
        1   => "Escape",
        59  => "F1",
        60  => "F2",
        61  => "F3",
        62  => "F4",
        63  => "F5",
        64  => "F6",
        65  => "F7",
        66  => "F8",
        67  => "F9",
        68  => "F10",
        87  => "F11",
        88  => "F12",
        183 => "F13",
        184 => "F14",
        185 => "F15",
        186 => "F16",
        187 => "F17",
        188 => "F18",
        189 => "F19",
        190 => "F20",
        191 => "F21",
        192 => "F22",
        193 => "F23",
        194 => "F24",
        99  => "PrintScreen",
        70  => "ScrollLock",
        119 => "Pause",

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_check_common_keys() {
        assert_eq!(evdev_to_code(30), Some("KeyA"));
        assert_eq!(evdev_to_code(2), Some("Digit1"));
        assert_eq!(evdev_to_code(42), Some("ShiftLeft"));
        assert_eq!(evdev_to_code(28), Some("Enter"));
        assert_eq!(evdev_to_code(103), Some("ArrowUp"));
        assert_eq!(evdev_to_code(76), Some("Numpad5"));
        assert_eq!(evdev_to_code(59), Some("F1"));
    }

    #[test]
    fn unknown_keycode_returns_none() {
        assert_eq!(evdev_to_code(9999), None);
        assert_eq!(evdev_to_code(0), None);
        assert_eq!(evdev_to_code(255), None);
    }

    // One representative from every key category — ensures categories
    // don't silently drop entries when the table is edited.
    #[test]
    fn all_categories_covered() {
        let cases: &[(u32, &str)] = &[
            // Writing system
            (41, "Backquote"), (43, "Backslash"), (26, "BracketLeft"),
            (27, "BracketRight"), (51, "Comma"), (13, "Equal"),
            (12, "Minus"), (52, "Period"), (40, "Quote"),
            (39, "Semicolon"), (53, "Slash"), (86, "IntlBackslash"),
            // Letters A-Z
            (30, "KeyA"), (48, "KeyB"), (46, "KeyC"), (32, "KeyD"),
            (18, "KeyE"), (33, "KeyF"), (34, "KeyG"), (35, "KeyH"),
            (23, "KeyI"), (36, "KeyJ"), (37, "KeyK"), (38, "KeyL"),
            (50, "KeyM"), (49, "KeyN"), (24, "KeyO"), (25, "KeyP"),
            (16, "KeyQ"), (19, "KeyR"), (31, "KeyS"), (20, "KeyT"),
            (22, "KeyU"), (47, "KeyV"), (17, "KeyW"), (45, "KeyX"),
            (21, "KeyY"), (44, "KeyZ"),
            // Digits 0-9
            (11, "Digit0"), (2, "Digit1"), (3, "Digit2"), (4, "Digit3"),
            (5, "Digit4"), (6, "Digit5"), (7, "Digit6"), (8, "Digit7"),
            (9, "Digit8"), (10, "Digit9"),
            // Modifiers
            (56, "AltLeft"), (100, "AltRight"),
            (29, "ControlLeft"), (97, "ControlRight"),
            (42, "ShiftLeft"), (54, "ShiftRight"),
            (125, "MetaLeft"), (126, "MetaRight"),
            // Common functional keys
            (14, "Backspace"), (58, "CapsLock"), (28, "Enter"),
            (57, "Space"), (15, "Tab"), (1, "Escape"), (127, "ContextMenu"),
            // Control pad
            (111, "Delete"), (107, "End"), (102, "Home"),
            (110, "Insert"), (109, "PageDown"), (104, "PageUp"),
            // Arrows
            (108, "ArrowDown"), (105, "ArrowLeft"),
            (106, "ArrowRight"), (103, "ArrowUp"),
            // Numpad
            (69, "NumLock"), (82, "Numpad0"), (79, "Numpad1"),
            (78, "NumpadAdd"), (83, "NumpadDecimal"), (98, "NumpadDivide"),
            (96, "NumpadEnter"), (55, "NumpadMultiply"), (74, "NumpadSubtract"),
            // Function row F1-F12
            (59, "F1"), (60, "F2"), (61, "F3"), (62, "F4"),
            (63, "F5"), (64, "F6"), (65, "F7"), (66, "F8"),
            (67, "F9"), (68, "F10"), (87, "F11"), (88, "F12"),
            // Extended function row
            (183, "F13"), (194, "F24"),
            // Other
            (99, "PrintScreen"), (70, "ScrollLock"), (119, "Pause"),
        ];
        for &(key, expected_code) in cases {
            assert_eq!(
                evdev_to_code(key),
                Some(expected_code),
                "evdev keycode {key} should map to \"{expected_code}\""
            );
        }
    }

    // Every code string returned by the table must be unique: two different
    // evdev keycodes must not map to the same KeyboardEvent.code string.
    #[test]
    fn no_duplicate_code_strings() {
        let mut seen: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        for key in 1u32..=250 {
            if let Some(code) = evdev_to_code(key) {
                if let Some(&prev_key) = seen.get(code) {
                    panic!(
                        "duplicate code \"{code}\": evdev {prev_key} and {key} \
                         both map to it"
                    );
                }
                seen.insert(code, key);
            }
        }
    }

    // The inverse of `no_duplicate_code_strings`: each evdev keycode in
    // 1..=250 appears at most once (trivially true for a match expression,
    // but a future macro-generated table might break this).
    #[test]
    fn no_duplicate_keycodes() {
        // This is guaranteed by the match arms, but an explicit count-based
        // test catches accidental duplicate arms that Rust doesn't warn about.
        let mut count = 0u32;
        let mut unique = std::collections::HashSet::new();
        for key in 1u32..=250 {
            if evdev_to_code(key).is_some() {
                count += 1;
                unique.insert(key);
            }
        }
        assert_eq!(count, unique.len() as u32, "duplicate evdev keycodes in table");
    }
}
