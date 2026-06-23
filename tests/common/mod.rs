//! Shared helpers for picking a Wayland socket name and TCP port that won't
//! collide with another test -- whether that's another `#[test]` in the same
//! binary (these run concurrently on separate threads but share a PID) or a
//! completely different test binary running in parallel.
//!
//! Lives under `tests/common/` rather than directly in `tests/` so cargo's
//! test auto-discovery doesn't treat this as its own test binary.

use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// A Wayland display name unique to this call: the PID disambiguates across
/// test binaries/processes, the counter disambiguates multiple calls within
/// the same process (e.g. two `#[test]`s in one file).
#[allow(dead_code)]
pub fn unique_display_name(prefix: &str) -> String {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{}", std::process::id(), id)
}

/// Asks the OS for a currently-free TCP port by binding to port 0 and
/// reading back what it chose, then releasing it immediately so the caller
/// (typically about to hand it to a spawned subprocess as `--port`) can use
/// it. The bind-then-drop has a theoretical race if something else grabs the
/// port in between, but that window is microseconds -- far more robust than
/// a hardcoded port shared across test files.
#[allow(dead_code)]
pub fn unique_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("failed to bind an ephemeral port")
        .local_addr()
        .expect("failed to read bound port")
        .port()
}
