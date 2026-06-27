// Touch event handling and coordinate mapping

use crate::compositor::CompositorState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, warn};

/// Touch event types from the browser
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "eventType")]
pub enum TouchEvent {
    #[serde(rename = "touchstart")]
    Start { touches: Vec<TouchPoint> },
    #[serde(rename = "touchmove")]
    Move { touches: Vec<TouchPoint> },
    #[serde(rename = "touchend")]
    End { touches: Vec<TouchPoint> },
    #[serde(rename = "touchcancel")]
    Cancel { touches: Vec<TouchPoint> },
}

/// A single touch point from the browser
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchPoint {
    /// Unique identifier for this touch
    pub identifier: i32,
    /// X coordinate relative to the video element (0.0 to 1.0)
    pub x: f64,
    /// Y coordinate relative to the video element (0.0 to 1.0)
    pub y: f64,
    /// Pressure/force of the touch (0.0 to 1.0)
    #[serde(default)]
    pub pressure: f64,
}

/// Internal state for a tracked touch
#[derive(Debug, Clone)]
struct TouchState {
    /// Current X position in compositor coordinates
    x: f64,
    /// Current Y position in compositor coordinates
    y: f64,
    /// Touch pressure
    pressure: f64,
}

/// Touch handler that manages touch state and coordinate mapping
pub struct TouchHandler {
    /// Current compositor width
    width: u32,
    /// Current compositor height
    height: u32,
    /// Active touches tracked by browser identifier
    active_touches: HashMap<i32, TouchState>,
}

impl TouchHandler {
    pub fn new(width: u32, height: u32) -> Self {
        debug!("Creating touch handler for {}x{}", width, height);
        Self {
            width,
            height,
            active_touches: HashMap::new(),
        }
    }

    /// Update the compositor dimensions for coordinate mapping
    pub fn set_dimensions(&mut self, width: u32, height: u32) {
        debug!("Touch handler: updating dimensions to {}x{}", width, height);
        self.width = width;
        self.height = height;
    }

    /// Convert normalized coordinates (0.0-1.0) to compositor coordinates
    fn to_compositor_coords(&self, x: f64, y: f64) -> (f64, f64) {
        let comp_x = x * self.width as f64;
        let comp_y = y * self.height as f64;
        (comp_x, comp_y)
    }

    /// Process a touch event from the browser, injecting it into the
    /// compositor's `wl_touch` seat capability.
    pub fn handle_event(&mut self, event: TouchEvent, state: &mut CompositorState) {
        match event {
            TouchEvent::Start { touches } => {
                debug!("Touch start: {} touches", touches.len());
                for touch in touches {
                    let (x, y) = self.to_compositor_coords(touch.x, touch.y);
                    let touch_state = TouchState {
                        x,
                        y,
                        pressure: touch.pressure,
                    };
                    self.active_touches.insert(touch.identifier, touch_state);
                    debug!(
                        "Touch {} down at ({:.1}, {:.1}) pressure={:.2}",
                        touch.identifier, x, y, touch.pressure
                    );
                    state.touch_down(touch.identifier, x, y);
                }
                state.touch_frame();
            }
            TouchEvent::Move { touches } => {
                debug!("Touch move: {} touches", touches.len());
                for touch in touches {
                    let (x, y) = self.to_compositor_coords(touch.x, touch.y);
                    if let Some(touch_state) = self.active_touches.get_mut(&touch.identifier) {
                        touch_state.x = x;
                        touch_state.y = y;
                        touch_state.pressure = touch.pressure;
                        debug!(
                            "Touch {} moved to ({:.1}, {:.1}) pressure={:.2}",
                            touch.identifier, x, y, touch.pressure
                        );
                        state.touch_motion(touch.identifier, x, y);
                    } else {
                        warn!("Touch move for unknown touch: {}", touch.identifier);
                    }
                }
                state.touch_frame();
            }
            TouchEvent::End { touches } => {
                debug!("Touch end: {} touches", touches.len());
                for touch in touches {
                    if let Some(touch_state) = self.active_touches.remove(&touch.identifier) {
                        debug!(
                            "Touch {} up at ({:.1}, {:.1})",
                            touch.identifier, touch_state.x, touch_state.y
                        );
                        state.touch_up(touch.identifier);
                    } else {
                        warn!("Touch end for unknown touch: {}", touch.identifier);
                    }
                }
                state.touch_frame();
            }
            TouchEvent::Cancel { .. } => {
                // wl_touch.cancel is a global cancellation of the entire touch
                // sequence, not a per-contact release. All active contacts are
                // invalidated at once. Clear the full map rather than iterating
                // the event's touch list: the client-side filter may have already
                // dropped some identifiers (e.g., contacts whose coordinates went
                // off-screen before the cancel fired), so the list can be
                // incomplete.
                debug!("Touch cancel: clearing {} active touches", self.active_touches.len());
                self.active_touches.clear();
                state.touch_cancel();
            }
        }
    }

}

#[cfg(test)]
impl TouchHandler {
    /// Get the number of currently active touches. Test-only: nothing in
    /// the running server currently needs this at runtime.
    pub fn active_touch_count(&self) -> usize {
        self.active_touches.len()
    }

    /// Clear all active touches. Test-only: nothing in the running server
    /// currently calls this (e.g. on client disconnect).
    pub fn clear_touches(&mut self) {
        debug!("Clearing {} active touches", self.active_touches.len());
        self.active_touches.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::calloop::EventLoop;
    use smithay::reexports::wayland_server::Display;

    /// Builds a real `CompositorState` (event loop + display + Smithay
    /// seat/space) so tests can drive `handle_event` exactly as `main.rs`
    /// does, rather than just exercising coordinate math.
    fn test_compositor_state() -> (EventLoop<'static, CompositorState>, Display<CompositorState>, CompositorState) {
        let mut event_loop: EventLoop<CompositorState> =
            EventLoop::try_new().expect("failed to create event loop");
        let mut display: Display<CompositorState> =
            Display::new().expect("failed to create display");
        let state = CompositorState::new(&mut event_loop, &mut display, 1920, 1080);
        (event_loop, display, state)
    }

    #[test]
    fn test_touch_handler_creation() {
        let handler = TouchHandler::new(1920, 1080);
        assert_eq!(handler.width, 1920);
        assert_eq!(handler.height, 1080);
        assert_eq!(handler.active_touch_count(), 0);
    }

    #[test]
    fn test_coordinate_mapping() {
        let handler = TouchHandler::new(1920, 1080);
        
        // Test center of screen
        let (x, y) = handler.to_compositor_coords(0.5, 0.5);
        assert_eq!(x, 960.0);
        assert_eq!(y, 540.0);
        
        // Test top-left corner
        let (x, y) = handler.to_compositor_coords(0.0, 0.0);
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);
        
        // Test bottom-right corner
        let (x, y) = handler.to_compositor_coords(1.0, 1.0);
        assert_eq!(x, 1920.0);
        assert_eq!(y, 1080.0);
    }

    #[test]
    fn test_touch_start_and_end() {
        let (_event_loop, _display, mut comp_state) = test_compositor_state();
        let mut handler = TouchHandler::new(1920, 1080);

        // Start a touch
        let event = TouchEvent::Start {
            touches: vec![TouchPoint {
                identifier: 0,
                x: 0.5,
                y: 0.5,
                pressure: 0.8,
            }],
        };
        handler.handle_event(event, &mut comp_state);
        assert_eq!(handler.active_touch_count(), 1);

        // End the touch
        let event = TouchEvent::End {
            touches: vec![TouchPoint {
                identifier: 0,
                x: 0.5,
                y: 0.5,
                pressure: 0.8,
            }],
        };
        handler.handle_event(event, &mut comp_state);
        assert_eq!(handler.active_touch_count(), 0);
    }

    #[test]
    fn test_multiple_touches() {
        let (_event_loop, _display, mut comp_state) = test_compositor_state();
        let mut handler = TouchHandler::new(1920, 1080);

        // Start two touches
        let event = TouchEvent::Start {
            touches: vec![
                TouchPoint {
                    identifier: 0,
                    x: 0.25,
                    y: 0.25,
                    pressure: 0.8,
                },
                TouchPoint {
                    identifier: 1,
                    x: 0.75,
                    y: 0.75,
                    pressure: 0.9,
                },
            ],
        };
        handler.handle_event(event, &mut comp_state);
        assert_eq!(handler.active_touch_count(), 2);

        // Move both touches
        let event = TouchEvent::Move {
            touches: vec![
                TouchPoint {
                    identifier: 0,
                    x: 0.3,
                    y: 0.3,
                    pressure: 0.8,
                },
                TouchPoint {
                    identifier: 1,
                    x: 0.7,
                    y: 0.7,
                    pressure: 0.9,
                },
            ],
        };
        handler.handle_event(event, &mut comp_state);
        assert_eq!(handler.active_touch_count(), 2);

        // End one touch
        let event = TouchEvent::End {
            touches: vec![TouchPoint {
                identifier: 0,
                x: 0.3,
                y: 0.3,
                pressure: 0.8,
            }],
        };
        handler.handle_event(event, &mut comp_state);
        assert_eq!(handler.active_touch_count(), 1);

        // End the other touch
        let event = TouchEvent::End {
            touches: vec![TouchPoint {
                identifier: 1,
                x: 0.7,
                y: 0.7,
                pressure: 0.9,
            }],
        };
        handler.handle_event(event, &mut comp_state);
        assert_eq!(handler.active_touch_count(), 0);
    }

    #[test]
    fn test_dimension_update() {
        let mut handler = TouchHandler::new(1920, 1080);
        
        // Update dimensions
        handler.set_dimensions(3840, 2160);
        assert_eq!(handler.width, 3840);
        assert_eq!(handler.height, 2160);
        
        // Test coordinate mapping with new dimensions
        let (x, y) = handler.to_compositor_coords(0.5, 0.5);
        assert_eq!(x, 1920.0);
        assert_eq!(y, 1080.0);
    }

    /// `touchcancel` must clear ALL active touches, not just the ones listed in
    /// the event. The browser's changedTouches list can be incomplete (e.g. when
    /// coordinates went off-screen and the client dropped some identifiers), and
    /// wl_touch.cancel is a global protocol-level cancellation in any case.
    #[test]
    fn test_cancel_clears_all_active_touches_even_when_event_list_is_empty() {
        let (_event_loop, _display, mut comp_state) = test_compositor_state();
        let mut handler = TouchHandler::new(1920, 1080);

        // Start two touches.
        handler.handle_event(
            TouchEvent::Start {
                touches: vec![
                    TouchPoint { identifier: 0, x: 0.3, y: 0.3, pressure: 0.5 },
                    TouchPoint { identifier: 1, x: 0.7, y: 0.7, pressure: 0.5 },
                ],
            },
            &mut comp_state,
        );
        assert_eq!(handler.active_touch_count(), 2);

        // Cancel with an EMPTY touch list — simulates the client dropping all
        // identifiers because both contacts were off-screen when cancel fired.
        // The handler must still clear both active touches.
        handler.handle_event(TouchEvent::Cancel { touches: vec![] }, &mut comp_state);
        assert_eq!(handler.active_touch_count(), 0);
    }

    #[test]
    fn test_clear_touches() {
        let (_event_loop, _display, mut comp_state) = test_compositor_state();
        let mut handler = TouchHandler::new(1920, 1080);

        // Start multiple touches
        let event = TouchEvent::Start {
            touches: vec![
                TouchPoint {
                    identifier: 0,
                    x: 0.25,
                    y: 0.25,
                    pressure: 0.8,
                },
                TouchPoint {
                    identifier: 1,
                    x: 0.75,
                    y: 0.75,
                    pressure: 0.9,
                },
            ],
        };
        handler.handle_event(event, &mut comp_state);
        assert_eq!(handler.active_touch_count(), 2);

        // Clear all touches
        handler.clear_touches();
        assert_eq!(handler.active_touch_count(), 0);
    }
}
