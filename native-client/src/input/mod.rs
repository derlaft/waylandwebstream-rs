// Input handling for Phase 7.
//
// `keymap` maps Linux evdev keycodes to browser KeyboardEvent.code strings.
// The actual Wayland event dispatch (wl_pointer, wl_keyboard) is in
// `display/mod.rs` since the Dispatch impls must be on `DisplayState`.

pub mod keymap;
