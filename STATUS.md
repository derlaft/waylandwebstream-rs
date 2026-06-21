# WaylandWebStream - Current Status

## Summary

A Wayland compositor that streams application windows to web browsers via WebRTC. Currently at intermediate phase with full protocol support but **pixel rendering is broken** (black screen).

## What Works ✅

### Wayland Protocol (Complete)
- ✅ `wl_compositor` - Core compositor protocol
- ✅ `xdg_shell` - Window management (xdg_toplevel, configure events)
- ✅ `wl_shm` - Shared memory buffer support
- ✅ `wl_seat` - Input devices (keyboard, pointer, touch initialized)
- ✅ `wl_output` - Display output management

### Compositor Functionality
- ✅ Client connections (applications can connect)
- ✅ Window creation and mapping (windows detected in space)
- ✅ Surface commits handled
- ✅ Frame callbacks sent (30fps)
- ✅ Event loop integration with encoder

### WebRTC Streaming
- ✅ H.264 encoding @ 30fps, 2Mbps
- ✅ RTP packetization
- ✅ WebSocket signaling server
- ✅ Browser client with video playback
- ✅ ICE/STUN support

### Testing
- ✅ `weston-terminal` connects successfully
- ✅ Window mapped to space (log shows "1 windows")
- ✅ Surface commits working
- ✅ No crashes or protocol errors

## What's Broken 🔴

### Pixel Rendering (CRITICAL)
**Issue**: Only black screen visible in browser instead of application content

**Symptoms**:
- Windows are detected (`Rendering 1 windows`)
- No "Rendering buffer" log messages appear
- Black screen in browser (instead of weston-terminal content)

**Root Cause** (suspected):
The `with_states()` call cannot find `SurfaceAttributes` in the `data_map`:
```rust
let buffer_opt = with_states(&surface, |states| {
    states.data_map.get::<std::cell::RefCell<SurfaceAttributes>>() // Returns None
    // ...
});
```

**Possible Reasons**:
1. SurfaceAttributes stored with different key type in data_map
2. Buffer not yet committed when we try to access it
3. Need to use compositor's internal cached state instead of data_map
4. Missing smithay trait implementation for proper state access

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Wayland Client (weston-terminal, cage, etc.)               │
│  - Connects to wayland-wws-0 socket                         │
│  - Creates surfaces, attaches SHM buffers                   │
│  - Commits frames                                           │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  Compositor State (src/compositor/state.rs)                 │
│  - CompositorState: handles surface commits   ✅            │
│  - XdgShellState: manages windows             ✅            │
│  - ShmState: manages shared memory            ✅            │
│  - Space: tracks window positions             ✅            │
│  - render(): should extract pixel data        🔴 BROKEN     │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  Main Event Loop (src/main.rs)                              │
│  - Dispatches Wayland events                  ✅            │
│  - Calls state.render() @ 30fps               ✅            │
│  - Sends frames to encoder                    ✅            │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  Encoder Thread (src/encoder/mod.rs)                        │
│  - Receives RGBA framebuffers                 ✅            │
│  - Converts to YUV420                         ✅            │
│  - H.264 encode with x264                     ✅            │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  WebRTC Session (src/webrtc/session.rs)                     │
│  - RTP packetization                          ✅            │
│  - RTCP feedback                              ✅            │
│  - ICE/STUN NAT traversal                     ✅            │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  Browser (http://localhost:8080)                            │
│  - WebRTC peer connection                     ✅            │
│  - H.264 hardware decode                      ✅            │
│  - Video playback                             ✅            │
│  - Shows: Black screen (should show app)      🔴            │
└─────────────────────────────────────────────────────────────┘
```

## Debug Steps Needed

1. **Add more logging** in `render()` function:
   ```rust
   info!("Window count: {}", window_count);
   info!("Window surface available: {}", surface.is_some());
   info!("Buffer opt: {:?}", buffer_opt);
   ```

2. **Check what's in data_map**:
   ```rust
   with_states(&surface, |states| {
       info!("data_map keys: {:?}", /* enumerate all keys */);
   });
   ```

3. **Try alternative approaches**:
   - Use `smithay::backend::renderer::utils::with_renderer_surface_state`
   - Access cached state directly instead of data_map
   - Use surface tree traversal with proper state extraction

4. **Verify buffer is attached**:
   - Log in `commit()` handler when buffer is attached
   - Check if `on_commit_buffer_handler` processes buffers correctly

## How to Test

```bash
# Start compositor
./target/release/waylandwebstream

# Open browser
# Navigate to: http://localhost:8080
# Expected: Animated gradient (test pattern when no windows)

# Launch Wayland app
WAYLAND_DISPLAY=wayland-wws-0 weston-terminal

# Expected: Terminal window content visible
# Actual: Black screen
```

## File Locations

- **Main compositor**: `src/compositor/state.rs` (lines 139-241: render function)
- **Event loop**: `src/main.rs` (lines 161-192: rendering integration)
- **Protocol handlers**: `src/compositor/state.rs` (lines 243-314)
- **Window management**: `src/compositor/state.rs` (lines 207-219: new_toplevel)

## Next Steps (Priority Order)

1. **Debug buffer access** - Add extensive logging to understand why buffer_opt is None
2. **Check smithay examples** - See how other compositors access buffer data
3. **Try renderer-based approach** - Use Pixman renderer to access surfaces
4. **Verify SHM buffer format** - Ensure ARGB8888 is correctly handled
5. **Test with simpler client** - Create minimal test that just fills a color

## Logs

Recent test showed:
```
[INFO] Rendering 1 windows          ✅ Window detected
[INFO] Surface tree traversal: data_map has attrs: false  🔴 Can't find attributes
```

This confirms: window exists, surface exists, but SurfaceAttributes not accessible via data_map.

---

**Status**: Intermediate phase complete (applications run), pixel rendering broken.
**Last Updated**: 2026-06-21
**Commits**: `1247cd6` (broken rendering), `e400020` (working detection)
