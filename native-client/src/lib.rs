// Library entry for `native-client`. The binary in `src/main.rs`
// also has its own `mod decode; ...` tree (Rust requires each
// compilation unit to declare what it uses), but for the
// integration tests under `tests/` we re-export the same modules
// here so they can `use native_client::decode::...`.

pub mod audio;
pub mod decode;
pub mod display;
pub mod input;
pub mod latency;
pub mod proto;
pub mod render;
pub mod transport;
pub mod types;
