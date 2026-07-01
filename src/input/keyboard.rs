// Keyboard event handling: maps the browser's `KeyboardEvent.code` (a
// layout-independent *physical* key identifier) to a Linux evdev keycode
// and injects it into the compositor's `wl_keyboard` seat capability. This
// passes through physical key identity only -- the server's own XKB keymap
// (configured on `seat.add_keyboard`, see compositor/state.rs) resolves the
// resulting keysym, the same way real hardware works.

use crate::compositor::CompositorState;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Keyboard event types from the browser's KeyboardEvent API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "eventType")]
pub enum KeyboardEvent {
    #[serde(rename = "keydown")]
    Down { code: String },
    #[serde(rename = "keyup")]
    Up { code: String },
}

/// Maps a browser `KeyboardEvent.code` (per the UI Events spec) to a Linux
/// evdev keycode (`linux/input-event-codes.h`).
fn evdev_keycode(code: &str) -> Option<u32> {
    Some(match code {
        // Writing system, digit row
        "Backquote" => 41,
        "Backslash" => 43,
        "BracketLeft" => 26,
        "BracketRight" => 27,
        "Comma" => 51,
        "Digit0" => 11,
        "Digit1" => 2,
        "Digit2" => 3,
        "Digit3" => 4,
        "Digit4" => 5,
        "Digit5" => 6,
        "Digit6" => 7,
        "Digit7" => 8,
        "Digit8" => 9,
        "Digit9" => 10,
        "Equal" => 13,
        "IntlBackslash" => 86,
        "IntlRo" => 89,
        "IntlYen" => 124,
        "KeyA" => 30,
        "KeyB" => 48,
        "KeyC" => 46,
        "KeyD" => 32,
        "KeyE" => 18,
        "KeyF" => 33,
        "KeyG" => 34,
        "KeyH" => 35,
        "KeyI" => 23,
        "KeyJ" => 36,
        "KeyK" => 37,
        "KeyL" => 38,
        "KeyM" => 50,
        "KeyN" => 49,
        "KeyO" => 24,
        "KeyP" => 25,
        "KeyQ" => 16,
        "KeyR" => 19,
        "KeyS" => 31,
        "KeyT" => 20,
        "KeyU" => 22,
        "KeyV" => 47,
        "KeyW" => 17,
        "KeyX" => 45,
        "KeyY" => 21,
        "KeyZ" => 44,
        "Minus" => 12,
        "Period" => 52,
        "Quote" => 40,
        "Semicolon" => 39,
        "Slash" => 53,

        // Functional keys
        "AltLeft" => 56,
        "AltRight" => 100,
        "Backspace" => 14,
        "CapsLock" => 58,
        "ContextMenu" => 127,
        "ControlLeft" => 29,
        "ControlRight" => 97,
        "Enter" => 28,
        "MetaLeft" => 125,
        "MetaRight" => 126,
        "ShiftLeft" => 42,
        "ShiftRight" => 54,
        "Space" => 57,
        "Tab" => 15,
        "Convert" => 92,
        "KanaMode" => 93,
        "Lang1" => 122,
        "Lang2" => 123,
        "NonConvert" => 94,

        // Control pad
        "Delete" => 111,
        "End" => 107,
        "Help" => 138,
        "Home" => 102,
        "Insert" => 110,
        "PageDown" => 109,
        "PageUp" => 104,

        // Arrow pad
        "ArrowDown" => 108,
        "ArrowLeft" => 105,
        "ArrowRight" => 106,
        "ArrowUp" => 103,

        // Numpad
        "NumLock" => 69,
        "Numpad0" => 82,
        "Numpad1" => 79,
        "Numpad2" => 80,
        "Numpad3" => 81,
        "Numpad4" => 75,
        "Numpad5" => 76,
        "Numpad6" => 77,
        "Numpad7" => 71,
        "Numpad8" => 72,
        "Numpad9" => 73,
        "NumpadAdd" => 78,
        "NumpadComma" => 121,
        "NumpadDecimal" => 83,
        "NumpadDivide" => 98,
        "NumpadEnter" => 96,
        "NumpadEqual" => 117,
        "NumpadMultiply" => 55,
        "NumpadSubtract" => 74,

        // Function row
        "Escape" => 1,
        "F1" => 59,
        "F2" => 60,
        "F3" => 61,
        "F4" => 62,
        "F5" => 63,
        "F6" => 64,
        "F7" => 65,
        "F8" => 66,
        "F9" => 67,
        "F10" => 68,
        "F11" => 87,
        "F12" => 88,
        "F13" => 183,
        "F14" => 184,
        "F15" => 185,
        "F16" => 186,
        "F17" => 187,
        "F18" => 188,
        "F19" => 189,
        "F20" => 190,
        "F21" => 191,
        "F22" => 192,
        "F23" => 193,
        "F24" => 194,
        "PrintScreen" => 99,
        "ScrollLock" => 70,
        "Pause" => 119,

        _ => return None,
    })
}

/// Process a keyboard event from the browser, injecting it into the
/// compositor's `wl_keyboard` seat capability.
pub fn handle_event(event: KeyboardEvent, state: &mut CompositorState) {
    let (code, pressed) = match &event {
        KeyboardEvent::Down { code } => (code, true),
        KeyboardEvent::Up { code } => (code, false),
    };
    match evdev_keycode(code) {
        Some(keycode) => state.key(keycode, pressed),
        None => warn!("Unknown KeyboardEvent.code: {}", code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::calloop::EventLoop;
    use smithay::reexports::wayland_server::Display;

    fn test_compositor_state() -> (
        EventLoop<'static, CompositorState>,
        Display<CompositorState>,
        CompositorState,
    ) {
        let mut event_loop: EventLoop<CompositorState> =
            EventLoop::try_new().expect("failed to create event loop");
        let mut display: Display<CompositorState> =
            Display::new().expect("failed to create display");
        let state = CompositorState::new(&mut event_loop, &mut display, 1920, 1080);
        (event_loop, display, state)
    }

    #[test]
    fn test_known_codes() {
        assert_eq!(evdev_keycode("KeyA"), Some(30));
        assert_eq!(evdev_keycode("Digit1"), Some(2));
        assert_eq!(evdev_keycode("ShiftLeft"), Some(42));
        assert_eq!(evdev_keycode("Enter"), Some(28));
        assert_eq!(evdev_keycode("ArrowUp"), Some(103));
        assert_eq!(evdev_keycode("Numpad5"), Some(76));
        assert_eq!(evdev_keycode("F1"), Some(59));
    }

    #[test]
    fn test_unknown_code() {
        assert_eq!(evdev_keycode("NotARealKey"), None);
    }

    #[test]
    fn test_down_and_up() {
        let (_event_loop, _display, mut comp_state) = test_compositor_state();
        handle_event(
            KeyboardEvent::Down {
                code: "KeyA".into(),
            },
            &mut comp_state,
        );
        handle_event(
            KeyboardEvent::Up {
                code: "KeyA".into(),
            },
            &mut comp_state,
        );
    }

    #[test]
    fn test_unknown_code_does_not_panic() {
        let (_event_loop, _display, mut comp_state) = test_compositor_state();
        handle_event(
            KeyboardEvent::Down {
                code: "NotARealKey".into(),
            },
            &mut comp_state,
        );
    }
}
