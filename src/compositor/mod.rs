pub mod state;

use crate::encoder::{CapturedFrame, RawFrame};
pub use state::CompositorState;
use state::WaylandWebStreamState;

/// A pluggable rendering backend that turns the compositor's current window
/// state into a `CapturedFrame`. `state` stays owned by the caller (it's
/// also the calloop event-loop's `Data` type, used directly for Wayland
/// protocol dispatch, input injection, resize, etc.) -- a backend only ever
/// borrows it for the duration of one `render` call.
///
/// Today only `SwCompositor` exists, wrapping the manual memcpy compositor
/// in `WaylandWebStreamState::render`. A future `GlCompositor`
/// (hardware-acceleration-plan.md Phase B) replaces that render path with
/// smithay's `GlesRenderer`, behind the same trait.
pub trait Compositor {
    fn render(&mut self, state: &mut WaylandWebStreamState, reuse: Option<Vec<u8>>) -> Option<CapturedFrame>;
}

/// Software compositor -- today's only `Compositor` implementation. Wraps
/// `WaylandWebStreamState::render`'s manual memcpy compositing, which is
/// left untouched (Phase B's doc explicitly replaces that method's body in
/// place rather than this wrapper).
pub struct SwCompositor;

impl Compositor for SwCompositor {
    fn render(&mut self, state: &mut WaylandWebStreamState, reuse: Option<Vec<u8>>) -> Option<CapturedFrame> {
        let (width, height) = (state.width, state.height);
        state.render(reuse).map(|data| {
            CapturedFrame::Cpu(RawFrame {
                data,
                width,
                height,
                capture_instant: std::time::Instant::now(),
            })
        })
    }
}
