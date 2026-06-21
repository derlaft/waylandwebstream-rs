// Pointer (mouse/pen) event handling and coordinate mapping.
//
// Touch contacts have their own dedicated TouchHandler/wl_touch path; this
// module only covers browser PointerEvents for mouse and pen/stylus devices
// (pointerType "mouse" | "pen") -- the client filters out pointerType
// "touch" so the same physical contact isn't injected twice.

use crate::compositor::CompositorState;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// Linux kernel button codes (linux/input-event-codes.h) that wl_pointer.button expects.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// Maps a browser `PointerEvent.button` index to a Linux button code.
fn linux_button_code(button: i32) -> Option<u32> {
    match button {
        0 => Some(BTN_LEFT),
        1 => Some(BTN_MIDDLE),
        2 => Some(BTN_RIGHT),
        3 => Some(BTN_SIDE),
        4 => Some(BTN_EXTRA),
        _ => None,
    }
}

/// Pointer event types from the browser's Pointer Events API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "eventType")]
pub enum MouseEvent {
    #[serde(rename = "pointerdown")]
    Down { pointer: PointerPoint },
    #[serde(rename = "pointermove")]
    Move { pointer: PointerPoint },
    #[serde(rename = "pointerup")]
    Up { pointer: PointerPoint },
    #[serde(rename = "pointercancel")]
    Cancel { pointer: PointerPoint },
    #[serde(rename = "wheel")]
    Wheel {
        x: f64,
        y: f64,
        #[serde(rename = "deltaX")]
        delta_x: f64,
        #[serde(rename = "deltaY")]
        delta_y: f64,
    },
}

fn default_pointer_type() -> String {
    "mouse".to_string()
}

/// A single pointer sample from the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointerPoint {
    /// X coordinate relative to the video element (0.0 to 1.0)
    pub x: f64,
    /// Y coordinate relative to the video element (0.0 to 1.0)
    pub y: f64,
    /// `PointerEvent.button` index; only meaningful for down/up.
    #[serde(default)]
    pub button: i32,
    /// "mouse" or "pen" -- touch contacts are routed through `TouchHandler` instead.
    #[serde(rename = "pointerType", default = "default_pointer_type")]
    pub pointer_type: String,
    /// Pressure/force, 0.0 to 1.0 (meaningful for pen tablets).
    #[serde(default)]
    pub pressure: f64,
}

/// Pointer handler that maps browser coordinates into compositor space and
/// injects the result into Smithay's `wl_pointer` seat capability.
pub struct MouseHandler {
    width: u32,
    height: u32,
}

impl MouseHandler {
    pub fn new(width: u32, height: u32) -> Self {
        debug!("Creating mouse handler for {}x{}", width, height);
        Self { width, height }
    }

    /// Update the compositor dimensions for coordinate mapping
    pub fn set_dimensions(&mut self, width: u32, height: u32) {
        debug!("Mouse handler: updating dimensions to {}x{}", width, height);
        self.width = width;
        self.height = height;
    }

    /// Convert normalized coordinates (0.0-1.0) to compositor coordinates
    fn to_compositor_coords(&self, x: f64, y: f64) -> (f64, f64) {
        (x * self.width as f64, y * self.height as f64)
    }

    /// Process a pointer event from the browser, injecting it into the
    /// compositor's `wl_pointer` seat capability.
    pub fn handle_event(&mut self, event: MouseEvent, state: &mut CompositorState) {
        match event {
            MouseEvent::Down { pointer } => {
                let (x, y) = self.to_compositor_coords(pointer.x, pointer.y);
                debug!("Pointer down at ({:.1}, {:.1}) button={}", x, y, pointer.button);
                state.pointer_motion(x, y);
                match linux_button_code(pointer.button) {
                    Some(button) => state.pointer_button(button, true),
                    None => warn!("Unknown pointer button: {}", pointer.button),
                }
                state.pointer_frame();
            }
            MouseEvent::Move { pointer } => {
                let (x, y) = self.to_compositor_coords(pointer.x, pointer.y);
                state.pointer_motion(x, y);
                state.pointer_frame();
            }
            MouseEvent::Up { pointer } => {
                let (x, y) = self.to_compositor_coords(pointer.x, pointer.y);
                debug!("Pointer up at ({:.1}, {:.1}) button={}", x, y, pointer.button);
                state.pointer_motion(x, y);
                match linux_button_code(pointer.button) {
                    Some(button) => state.pointer_button(button, false),
                    None => warn!("Unknown pointer button: {}", pointer.button),
                }
                state.pointer_frame();
            }
            MouseEvent::Cancel { .. } => {
                // The browser took the pointer over for its own purposes (e.g. a
                // scroll/zoom gesture). wl_pointer has no cancel notion and the
                // client never sends this while a button is held (see
                // client.html), so there's nothing to reconcile here.
            }
            MouseEvent::Wheel { x, y, delta_x, delta_y } => {
                let (cx, cy) = self.to_compositor_coords(x, y);
                state.pointer_motion(cx, cy);
                state.pointer_axis(delta_x, delta_y);
                state.pointer_frame();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::calloop::EventLoop;
    use smithay::reexports::wayland_server::Display;

    fn test_compositor_state() -> (EventLoop<'static, CompositorState>, Display<CompositorState>, CompositorState) {
        let mut event_loop: EventLoop<CompositorState> =
            EventLoop::try_new().expect("failed to create event loop");
        let mut display: Display<CompositorState> =
            Display::new().expect("failed to create display");
        let state = CompositorState::new(&mut event_loop, &mut display, 1920, 1080);
        (event_loop, display, state)
    }

    #[test]
    fn test_mouse_handler_creation() {
        let handler = MouseHandler::new(1920, 1080);
        assert_eq!(handler.width, 1920);
        assert_eq!(handler.height, 1080);
    }

    #[test]
    fn test_coordinate_mapping() {
        let handler = MouseHandler::new(1920, 1080);
        let (x, y) = handler.to_compositor_coords(0.5, 0.5);
        assert_eq!(x, 960.0);
        assert_eq!(y, 540.0);
    }

    #[test]
    fn test_dimension_update() {
        let mut handler = MouseHandler::new(1920, 1080);
        handler.set_dimensions(3840, 2160);
        let (x, y) = handler.to_compositor_coords(0.5, 0.5);
        assert_eq!(x, 1920.0);
        assert_eq!(y, 1080.0);
    }

    #[test]
    fn test_button_mapping() {
        assert_eq!(linux_button_code(0), Some(BTN_LEFT));
        assert_eq!(linux_button_code(1), Some(BTN_MIDDLE));
        assert_eq!(linux_button_code(2), Some(BTN_RIGHT));
        assert_eq!(linux_button_code(3), Some(BTN_SIDE));
        assert_eq!(linux_button_code(4), Some(BTN_EXTRA));
        assert_eq!(linux_button_code(99), None);
    }

    #[test]
    fn test_down_and_up() {
        let (_event_loop, _display, mut comp_state) = test_compositor_state();
        let mut handler = MouseHandler::new(1920, 1080);

        handler.handle_event(
            MouseEvent::Down {
                pointer: PointerPoint { x: 0.5, y: 0.5, button: 0, pointer_type: "mouse".into(), pressure: 0.5 },
            },
            &mut comp_state,
        );
        handler.handle_event(
            MouseEvent::Move {
                pointer: PointerPoint { x: 0.6, y: 0.5, button: 0, pointer_type: "mouse".into(), pressure: 0.5 },
            },
            &mut comp_state,
        );
        handler.handle_event(
            MouseEvent::Up {
                pointer: PointerPoint { x: 0.6, y: 0.5, button: 0, pointer_type: "mouse".into(), pressure: 0.5 },
            },
            &mut comp_state,
        );
    }

    #[test]
    fn test_wheel() {
        let (_event_loop, _display, mut comp_state) = test_compositor_state();
        let mut handler = MouseHandler::new(1920, 1080);

        handler.handle_event(
            MouseEvent::Wheel { x: 0.5, y: 0.5, delta_x: 0.0, delta_y: 12.0 },
            &mut comp_state,
        );
    }
}
