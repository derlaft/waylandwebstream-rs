# Rendering Bug: Buffer Access Issue

## Status: IN PROGRESS

## Summary
The compositor successfully manages windows but cannot access their buffers for rendering. Windows are created, mapped, and committed, but the render loop cannot retrieve pixel data.

## What Works ✅

1. **Compositor Infrastructure**
   - Wayland socket created: `/run/user/{uid}/wayland-test-0`
   - XDG shell state initialized correctly
   - Clients connect successfully

2. **Window Management**
   - `new_toplevel()` is called when clients create windows
   - Windows are mapped to Space: `Window mapped to space. Total windows: 1`
   - XdgShellHandler is working correctly

3. **Surface Commits**
   - `commit()` handler is called
   - `on_commit_buffer_handler::<Self>(surface)` processes buffers
   - Log shows: "Window surface committed"

4. **Render Loop**
   - Detects windows: "Rendering 1 windows"
   - Iterates through space.elements()
   - Gets window surfaces successfully

## The Problem ❌

**Buffer retrieval fails** in `src/compositor/state.rs` around line 171-179:

```rust
let buffer_opt = with_states(&surface, |states| {
    let mut cached = states.cached_state.get::<SurfaceAttributes>();
    let current_attrs = cached.current();
    match &current_attrs.buffer {
        Some(smithay::wayland::compositor::BufferAssignment::NewBuffer(buf)) => Some(buf.clone()),
        _ => None,
    }
});
// buffer_opt is always None!
```

### Investigation Results

**Test 1: Check buffer in commit handler**
```rust
fn commit(&mut self, surface: &WlSurface) {
    on_commit_buffer_handler::<Self>(surface);
    
    let has_buffer = with_states(surface, |states| {
        let mut cached = states.cached_state.get::<SurfaceAttributes>();
        cached.current().buffer.is_some()
    });
    // Result: has_buffer = false ❌
}
```

**Finding:** Immediately after `on_commit_buffer_handler()`, the buffer is NOT in `cached_state.current().buffer`.

**Test 2: Check both pending and current**
```rust
let (has_pending, has_current) = with_states(surface, |states| {
    let mut cached = states.cached_state.get::<SurfaceAttributes>();
    (cached.pending().buffer.is_some(), cached.current().buffer.is_some())
});
// Result: has_pending = false, has_current = false ❌
```

**Finding:** Buffer is in neither pending nor current state after commit.

## Root Cause Analysis

`on_commit_buffer_handler::<Self>(surface)` does NOT store the buffer in `SurfaceAttributes`. Instead, it:

1. Takes the buffer from pending `SurfaceAttributes`
2. Processes it for the **renderer**
3. Stores it in `RendererSurfaceState` (a different data structure)
4. Leaves `SurfaceAttributes.buffer` as `None`

This is by design - Smithay's buffer handling is renderer-centric. The buffer is meant to be accessed through the renderer's state, not through SurfaceAttributes.

## Solution Options

### Option 1: Use Renderer Surface State (RECOMMENDED)
Access buffers through `with_renderer_surface_state`:

```rust
use smithay::backend::renderer::utils::with_renderer_surface_state;

let buffer_data = with_renderer_surface_state(&surface, |state| {
    // state.buffer() returns Option<&Buffer>
    // Need to extract WlBuffer from Buffer wrapper
    state.buffer().map(|buf| {
        // TODO: Find correct method to get WlBuffer from Buffer
        // Tried: buf.as_wl_buffer() - doesn't exist
        // Tried: buf.wl_buffer() - doesn't exist  
        // Need to check Buffer struct methods
    })
});
```

**Blocker:** Need to find the correct API to extract `WlBuffer` from Smithay's `Buffer` type for SHM access.

### Option 2: Don't Use on_commit_buffer_handler
Handle buffer commits manually without the renderer infrastructure:

```rust
fn commit(&mut self, surface: &WlSurface) {
    // Manually apply pending to current
    with_states(surface, |states| {
        let mut cached = states.cached_state.get::<SurfaceAttributes>();
        // Apply state transition somehow
    });
}
```

**Problem:** Need to find the correct method to apply cached state. Tried:
- `cached.commit()` - doesn't exist
- `states.cached_state.apply_state::<SurfaceAttributes>()` - wrong signature

### Option 3: Store Buffer Reference Ourselves
In the commit handler, extract and store the buffer before calling `on_commit_buffer_handler`:

```rust
fn commit(&mut self, surface: &WlSurface) {
    // Get buffer BEFORE on_commit_buffer_handler processes it
    let buffer = with_states(surface, |states| {
        states.cached_state.get::<SurfaceAttributes>()
            .pending().buffer.clone()
    });
    
    // Store in our own data structure
    // self.surface_buffers.insert(surface.id(), buffer);
    
    on_commit_buffer_handler::<Self>(surface);
}
```

**Issue:** Requires adding a HashMap to track buffers, more complex state management.

### Option 4: Use Pixman Renderer Directly
Instead of manual pixel copying, use the Pixman renderer to render to our framebuffer:

```rust
if let Some(renderer) = &mut self.renderer {
    // Use renderer.render() to draw windows to framebuffer
    // Pixman renderer can render to memory buffer
}
```

**Status:** We have `renderer: Option<PixmanRenderer>` but it's not initialized.
**Advantage:** Proper Smithay rendering pipeline, handles all buffer types.

## Recommended Next Steps

### Immediate: Option 4 (Use Pixman Renderer)

1. **Initialize Pixman Renderer**
   ```rust
   // In WaylandWebStreamState::new()
   let renderer = PixmanRenderer::new()?;
   ```

2. **Create Pixman Image for Output**
   ```rust
   let output_buffer = PixmanImage::new(...);
   ```

3. **Render Space to Buffer**
   ```rust
   renderer.render(
       output_buffer,
       &mut damage,
       &[&self.space],
       &[],
       age,
   )?;
   ```

4. **Extract Pixels from Pixman Image**
   Access the rendered pixels from the Pixman image

### Alternative: Research Smithay Buffer API

Check Smithay source code or examples for:
- How to get `WlBuffer` from `Buffer` 
- Whether there's a `buffer.wl_buffer()` or similar method
- How other software renderers access SHM buffers

### Files to Modify

- `src/compositor/state.rs:140-244` - `render()` method
- `src/compositor/state.rs:49` - Initialize `renderer: Option<PixmanRenderer>`
- `src/compositor/state.rs:318-333` - `commit()` handler (if needed)

## Testing

Once fixed, the integration test will automatically validate:

```bash
cd tests && npm install && cd ..
cargo build --release
cargo test --release -- --nocapture
```

Expected result:
- Test client shows red window
- Compositor logs: "Rendering buffer: 800x600"  
- Screenshot validation: finds red pixels
- WebRTC stream shows actual client content (not test pattern)

## References

- Smithay buffer handling: https://github.com/Smithay/smithay
- Current code: `src/compositor/state.rs:169-225`
- Test client: `wayland-test-client/src/main.rs` (draws 800x600 red rectangle)
- Integration test: `tests/integration_test.rs`
