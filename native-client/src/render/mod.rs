// Wayland SHM renderer. Phase 5 owns a double-buffered pair of
// wl_buffers, alternating which one is attached to the surface so the
// compositor can hold one (scanning out, etc.) while we fill the
// other. This is the same pattern every wl_shm client uses; the
// alternative -- a single buffer -- stalls because `wl_surface::attach`
// to a buffer the compositor is currently reading is a protocol error.

pub mod shm;
