// Require every `unsafe` block to carry a `// SAFETY:` justification and every
// unsafe operation inside an `unsafe fn` to sit in its own `unsafe {}` block.
// Kept as `warn` (not `deny`) here; CI promotes warnings to errors.
#![warn(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod adaptive_bitrate;
pub mod compositor;
pub mod encoder;
pub mod latency;
pub mod proto;
