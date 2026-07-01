// Double-buffered wl_shm renderer for decoded video frames.
//
// Owns two `BufferSlot`s; the `wl_buffer` user data carries the slot index
// so `wl_buffer::Release` events can mark the right slot free again. On a
// frame/window size mismatch we blit a centered `min(frame, buf)` region
// and let the compositor scale; the server rescales on resize anyway.

use anyhow::{Context, Result};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use wayland_client::{
    protocol::{wl_buffer, wl_shm, wl_shm_pool, wl_surface},
    Dispatch, QueueHandle,
};

use crate::decode::sw::DecodedFrame;

/// Slot index passed as the `wl_buffer` user data so the
/// `wl_buffer::Release` Dispatch can find the slot again. `0` and
/// `1` only -- the renderer is hardcoded to 2 buffers.
pub(crate) type SlotId = u32;

// Three slots instead of two: with a 60 Hz compositor the Release for
// a submitted buffer arrives ~16-32 ms later, leaving only ~1-17 ms of
// slack before the next 30 fps frame (33 ms interval). A third slot
// means there is always a free slot even when two are in flight, so
// tight Release timing never starves the renderer.
const NUM_SLOTS: usize = 3;

/// One half of the double buffer. Owned by `ShmRenderer`; its
/// `wl_buffer` carries the slot's index as user data so the Dispatch
/// impl can match `Release` events back to slots.
struct BufferSlot {
    /// `wl_shm_pool` -- the underlying shared memory region. Kept
    /// alive because the spec says the pool must outlive any buffer
    /// created from it.
    _pool: wl_shm_pool::WlShmPool,
    /// The wl_buffer we attach to the surface. Drops as `wl_buffer`
    /// (releasing compositor ref).
    buffer: wl_buffer::WlBuffer,
    /// mmap of the pool, kept live until the compositor has
    /// acknowledged release of every buffer created from it.
    _fd: OwnedFd,
    /// Raw mmap pointer + size; `pixels_ptr` is what `render` writes
    /// into. `mmap_size` is the byte count we mmap'd (stride *
    /// height).
    pixels_ptr: *mut u8,
    mmap_size: usize,
    width: u32,
    height: u32,
    /// `false` from the moment we attach until the compositor sends
    /// `Release`. `true` when it's safe to write + attach again.
    released: bool,
    /// `true` when the renderer has been resized past this slot's
    /// dimensions and the slot is waiting for the compositor to release
    /// it before being dropped. Must not be written into or re-attached;
    /// it will be dropped by `release_slot` once released.
    zombie: bool,
}

impl Drop for BufferSlot {
    fn drop(&mut self) {
        // SAFETY: pixels_ptr was returned by mmap with mmap_size bytes.
        // The mmap outlives any wl_buffer reference because we only drop
        // slots after the compositor sends Release (or on process exit).
        unsafe {
            libc::munmap(self.pixels_ptr as *mut libc::c_void, self.mmap_size);
        }
    }
}

// SAFETY: BufferSlot holds a raw mmap pointer + proxies. The pointer
// is never dereferenced off the display thread (the renderer always
// lives there). `wl_*` proxies are `Send`/`Sync` per wayland-client.
unsafe impl Send for BufferSlot {}

pub struct ShmRenderer {
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    slots: [Option<BufferSlot>; NUM_SLOTS],
    /// Width/height of the slots in `slots`. Updated on resize; when
    /// the next render fires, slots that don't match are recreated.
    width: u32,
    height: u32,
    /// Index of the slot we'd prefer to write into next (we toggle
    /// between 0 and 1, but skip slots that aren't released yet).
    next_idx: usize,
    render_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ShmRenderer {
    pub fn new<State>(
        shm: wl_shm::WlShm,
        surface: wl_surface::WlSurface,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
        render_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Self>
    where
        State: Dispatch<wl_shm_pool::WlShmPool, u32> + Dispatch<wl_buffer::WlBuffer, u32> + 'static,
    {
        let mut renderer = Self {
            shm,
            surface,
            slots: [None, None, None],
            width,
            height,
            next_idx: 0,
            render_count,
        };
        renderer.recreate_slots(qh)?;
        Ok(renderer)
    }

    /// Allocate a new slot for every `None` entry. Skips `Some` entries
    /// (including zombies) — overwriting a zombie would drop its WlBuffer
    /// while the compositor still holds it, bypassing the zombie invariant.
    fn recreate_slots<State>(&mut self, qh: &QueueHandle<State>) -> Result<()>
    where
        State: Dispatch<wl_shm_pool::WlShmPool, u32> + Dispatch<wl_buffer::WlBuffer, u32> + 'static,
    {
        for idx in 0..NUM_SLOTS {
            if self.slots[idx].is_none() {
                let slot =
                    create_buffer_slot(&self.shm, qh, idx as SlotId, self.width, self.height)
                        .with_context(|| format!("create buffer slot {idx}"))?;
                self.slots[idx] = Some(slot);
            }
        }
        Ok(())
    }

    /// Resize the renderer to `new_w x new_h`. Released slots are
    /// freed immediately; held slots become zombies — they're kept
    /// alive until the compositor sends their `wl_buffer::Release`,
    /// then dropped by `release_slot`. Sending `wl_buffer.destroy()`
    /// to the compositor while it still holds the buffer is a
    /// protocol error that can cause the compositor to show garbage or
    /// disconnect; the zombie pattern avoids that.
    pub fn resize(&mut self, new_w: u32, new_h: u32) {
        if (new_w, new_h) == (self.width, self.height) {
            return;
        }
        self.width = new_w;
        self.height = new_h;
        for slot in &mut self.slots {
            match slot {
                Some(s) if s.released => {
                    *slot = None; // compositor is done with it; safe to drop
                }
                Some(s) => {
                    s.zombie = true; // compositor holds it; drop on release
                }
                None => {}
            }
        }
    }

    /// Resize and immediately commit a new-sized buffer so the compositor
    /// maps the window at the correct dimensions without waiting for the
    /// next decoded frame.  Returns `false` only if all slots are still
    /// held by the compositor as zombies (very unlikely with 3 slots).
    pub fn resize_and_prime<State>(
        &mut self,
        qh: &QueueHandle<State>,
        new_w: u32,
        new_h: u32,
    ) -> bool
    where
        State: Dispatch<wl_shm_pool::WlShmPool, u32> + Dispatch<wl_buffer::WlBuffer, u32> + 'static,
    {
        self.resize(new_w, new_h);
        // Fill any freed (None) slots at the new size.
        if let Err(e) = self.recreate_slots(qh) {
            tracing::warn!("resize_and_prime recreate_slots: {e:#}");
            return false;
        }
        self.prime()
    }

    /// Attach the first available slot to the surface and commit.
    /// Use once after construction so the compositor has a buffer
    /// to map -- before the first decoded frame arrives. Returns
    /// `false` if no slot is available (shouldn't happen on a
    /// fresh renderer where both slots are `released = true`).
    pub fn prime(&mut self) -> bool {
        let picked = (0..NUM_SLOTS).find(|i| {
            self.slots[*i]
                .as_ref()
                .map(|s| s.released && !s.zombie)
                .unwrap_or(false)
        });
        let Some(idx) = picked else {
            return false;
        };
        let slot = self.slots[idx].as_mut().unwrap();
        slot.released = false;
        self.surface.attach(Some(&slot.buffer), 0, 0);
        self.surface
            .damage_buffer(0, 0, slot.width as i32, slot.height as i32);
        self.surface.commit();
        self.next_idx = (idx + 1) % NUM_SLOTS;
        true
    }

    /// Drain every pending `DecodedFrame` from `frame_rx`, rendering
    /// the latest one (and dropping earlier ones -- there's no value
    /// in showing them after a newer frame is ready, and the channel
    /// is bounded to 1 so at most one frame is usually waiting).
    /// Returns the number of frames consumed.
    pub fn try_drain<State>(
        &mut self,
        qh: &QueueHandle<State>,
        frame_rx: &mpsc::Receiver<DecodedFrame>,
    ) -> Result<usize>
    where
        State: Dispatch<wl_shm_pool::WlShmPool, u32> + Dispatch<wl_buffer::WlBuffer, u32> + 'static,
    {
        let mut latest: Option<DecodedFrame> = None;
        let mut count = 0usize;
        loop {
            match frame_rx.try_recv() {
                Ok(frame) => {
                    count += 1;
                    latest = Some(frame);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Display thread is exiting (or has exited); stop
                    // pulling frames.
                    return Ok(count);
                }
            }
        }
        if let Some(frame) = latest {
            self.render(qh, &frame)?;
        }
        Ok(count)
    }

    /// Mark `slot_idx` as released. Called from the
    /// `wl_buffer::Release` Dispatch impl. If the slot was already
    /// dropped via `resize`, this is a no-op.
    pub fn release_slot(&mut self, slot_idx: SlotId) {
        if let Some(slot) = self.slots.get_mut(slot_idx as usize) {
            if let Some(s) = slot.as_mut() {
                s.released = true;
                if s.zombie {
                    // Compositor done with the stale buffer; safe to drop now.
                    *slot = None;
                }
            }
        }
    }

    /// Attach + commit one decoded frame. Returns `Ok(false)` if both
    /// slots are still held by the compositor and we had to drop the
    /// frame (caller may log a warning; this is rare on a healthy
    /// session and self-heals on the next keyframe).
    pub fn render<State>(&mut self, qh: &QueueHandle<State>, frame: &DecodedFrame) -> Result<bool>
    where
        State: Dispatch<wl_shm_pool::WlShmPool, u32> + Dispatch<wl_buffer::WlBuffer, u32> + 'static,
    {
        // Recreate any slot that is missing (None). Zombie slots still
        // occupy their index and will be cleaned up when released; we
        // cannot allocate a new slot at that index until then.
        if self.slots.iter().any(|s| s.is_none()) {
            self.recreate_slots(qh)?;
        }

        // Pick the next non-zombie, released slot. Zombie slots have
        // the right index but stale dimensions; writing into them would
        // corrupt memory and violate the protocol.
        let mut released = [false; NUM_SLOTS];
        for (i, slot) in self.slots.iter().enumerate() {
            released[i] = slot
                .as_ref()
                .map(|s| s.released && !s.zombie)
                .unwrap_or(false);
        }
        let Some(idx) = pick_next_released(&released, self.next_idx) else {
            // Both slots held — compositor hasn't released the previous
            // buffer yet (usually <1 vsync). Sustained spamming means
            // wl_buffer::Release events aren't reaching dispatch_pending.
            tracing::debug!(
                "no released slot; dropping frame (slot_state={})",
                self.slot_state()
            );
            return Ok(false);
        };

        let slot = self.slots[idx].as_mut().unwrap();

        // Skip frames significantly different from the current slot.  After a
        // resize, old-size frames arrive until the server processes our Resize
        // message; blitting them produces a visible crop/letterbox glitch.
        // A tolerance of 2 px handles the server's ÷2 alignment rounding
        // (at most 1 px per dimension).
        if frame.width.abs_diff(slot.width) > 2 || frame.height.abs_diff(slot.height) > 2 {
            tracing::debug!(
                "skipping frame {}x{} (slot {}x{})",
                frame.width,
                frame.height,
                slot.width,
                slot.height
            );
            return Ok(false);
        }

        let frame_w = frame.width.min(slot.width);
        let frame_h = frame.height.min(slot.height);
        blit_into(
            &frame.pixels,
            frame.width,
            frame.height,
            slot.pixels_ptr,
            slot.mmap_size,
            slot.width,
            slot.height,
            frame_w,
            frame_h,
        );

        slot.released = false;
        self.surface.attach(Some(&slot.buffer), 0, 0);
        self.surface
            .damage_buffer(0, 0, slot.width as i32, slot.height as i32);
        self.surface.commit();

        self.next_idx = (idx + 1) % NUM_SLOTS;
        self.render_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
    }

    /// Diagnostic: report which slots are currently released. Useful
    /// when the renderer appears stuck (no released slot -> no render
    /// possible). Returns a string like `[released,held]`.
    pub fn slot_state(&self) -> String {
        let fmt = |s: &Option<BufferSlot>| match s {
            None => "none",
            Some(b) if b.zombie && b.released => "zombie-released",
            Some(b) if b.zombie => "zombie-held",
            Some(b) if b.released => "released",
            Some(_) => "held",
        };
        let parts: Vec<&str> = self.slots.iter().map(fmt).collect();
        format!("[{}]", parts.join(","))
    }
}

/// Pick the next slot to render into given which slots are currently
/// released and which one we'd *prefer* to use (for cache-locality).
/// Returns `None` if every slot is held by the compositor.
///
/// Pure function -- extracted from `render` so it can be unit-tested
/// without a real Wayland connection.
fn pick_next_released(released: &[bool; NUM_SLOTS], preferred: usize) -> Option<usize> {
    (0..NUM_SLOTS)
        .map(|i| (preferred + i) % NUM_SLOTS)
        .find(|i| released[*i])
}

fn create_buffer_slot<State>(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    slot_id: SlotId,
    width: u32,
    height: u32,
) -> Result<BufferSlot>
where
    State: Dispatch<wl_shm_pool::WlShmPool, u32> + Dispatch<wl_buffer::WlBuffer, u32> + 'static,
{
    const BYTES_PER_PIXEL: usize = 4;
    let stride = width as usize * BYTES_PER_PIXEL;
    let size = stride * height as usize;

    let name = CString::new("wws-client-shm").unwrap();
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        anyhow::bail!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let ret = unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) };
    if ret < 0 {
        anyhow::bail!(
            "ftruncate({size}) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        anyhow::bail!("mmap failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: we just mapped `size` bytes at `ptr`; zero them so the
    // first commit before any blit shows black, not whatever the
    // memfd was initialized with.
    unsafe {
        std::ptr::write_bytes(ptr as *mut u8, 0, size);
    }

    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, slot_id);
    // Xrgb8888 — compositor ignores the alpha byte, treating the surface as
    // fully opaque.  Argb8888 would alpha-blend the zero-initialised bytes
    // (alpha=0) making the window transparent until a frame fills the slot.
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Xrgb8888,
        qh,
        slot_id,
    );

    Ok(BufferSlot {
        _pool: pool,
        buffer,
        _fd: fd,
        pixels_ptr: ptr as *mut u8,
        mmap_size: size,
        width,
        height,
        released: true, // fresh buffer hasn't been attached yet
        zombie: false,
    })
}

/// Copy `blit_w x blit_h` pixels from `src` (row stride
/// `src_stride_bytes = src_w * 4`) into the destination buffer at
/// `dst_ptr` (row stride `dst_w * 4`, total size `dst_size` bytes).
/// Centers the source inside the destination so a size mismatch is a
/// centered letterbox rather than a top-left clip.
///
/// Takes raw pointers + dimensions rather than a `BufferSlot` so the
/// logic is testable without a real Wayland connection.
fn blit_into(
    src: &[u8],
    src_w: u32,
    _src_h: u32,
    dst_ptr: *mut u8,
    dst_size: usize,
    dst_w: u32,
    dst_h: u32,
    blit_w: u32,
    blit_h: u32,
) {
    let src_stride = src_w as usize * 4;
    let dst_stride = dst_w as usize * 4;

    if blit_w == dst_w && blit_h == dst_h && src_stride == dst_stride {
        // Fast path: identical layout, single memcpy.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst_ptr, dst_size);
        }
        return;
    }

    // Centered letterbox.
    let x_off = ((dst_w.saturating_sub(blit_w)) / 2) as usize * 4;
    let y_off = ((dst_h.saturating_sub(blit_h)) / 2) as usize;
    let row_bytes = blit_w as usize * 4;
    for row in 0..blit_h as usize {
        let src_off = row * src_stride;
        let dst_off = (y_off + row) * dst_stride + x_off;
        unsafe {
            std::ptr::copy_nonoverlapping(
                src.as_ptr().add(src_off),
                dst_ptr.add(dst_off),
                row_bytes,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_next_released_prefers_preferred_when_released() {
        assert_eq!(pick_next_released(&[true, true, true], 0), Some(0));
        assert_eq!(pick_next_released(&[true, true, true], 1), Some(1));
        assert_eq!(pick_next_released(&[true, true, true], 2), Some(2));
    }

    #[test]
    fn pick_next_released_falls_back_when_preferred_held() {
        // preferred=0 held, 1 free -> use 1
        assert_eq!(pick_next_released(&[false, true, true], 0), Some(1));
        // preferred=1 held, 2 free -> use 2
        assert_eq!(pick_next_released(&[true, false, true], 1), Some(2));
        // preferred=2 held, 0 free -> use 0
        assert_eq!(pick_next_released(&[true, true, false], 2), Some(0));
    }

    #[test]
    fn pick_next_released_returns_none_when_all_held() {
        assert_eq!(pick_next_released(&[false, false, false], 0), None);
        assert_eq!(pick_next_released(&[false, false, false], 1), None);
        assert_eq!(pick_next_released(&[false, false, false], 2), None);
    }

    #[test]
    fn pick_next_released_rotates_correctly() {
        // preferred=1 held, 2 held -> falls through to 0
        assert_eq!(pick_next_released(&[true, false, false], 1), Some(0));
        // preferred=0 held, 1 held -> falls through to 2
        assert_eq!(pick_next_released(&[false, false, true], 0), Some(2));
    }

    /// Run blit_into against a heap-allocated buffer. Returns the
    /// post-blit bytes for inspection. The whole exercise is just
    /// memcpy semantics -- no Wayland involved.
    fn run_blit(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
        let dst_size = (dst_w as usize) * (dst_h as usize) * 4;
        let mut dst = vec![0u8; dst_size];
        let blit_w = src_w.min(dst_w);
        let blit_h = src_h.min(dst_h);
        blit_into(
            src,
            src_w,
            src_h,
            dst.as_mut_ptr(),
            dst_size,
            dst_w,
            dst_h,
            blit_w,
            blit_h,
        );
        dst
    }

    #[test]
    fn blit_into_same_size_copies_all() {
        // 4x4 frame -> 4x4 slot: every byte should be copied verbatim.
        let mut src = Vec::with_capacity(4 * 4 * 4);
        for i in 0..(4u32 * 4 * 4) {
            src.push((i & 0xFF) as u8);
        }
        let out = run_blit(&src, 4, 4, 4, 4);
        assert_eq!(out, src);
    }

    #[test]
    fn blit_into_smaller_frame_letterboxes() {
        // 2x2 frame into 4x4 slot: y_off=1, x_off=4 bytes.
        // dst[20..28] holds src row 0; dst[36..44] holds src row 1.
        let src = vec![0xAAu8; 2 * 2 * 4]; // 16 bytes of 0xAA
        let out = run_blit(&src, 2, 2, 4, 4);
        assert_eq!(out.len(), 64);
        // Row 0 (dst bytes 0..16): all zeros.
        assert_eq!(&out[0..16], &[0u8; 16]);
        // Row 1 (dst bytes 16..32): 4 bytes 0, 8 bytes 0xAA (src row 0),
        // 4 bytes 0.
        assert_eq!(&out[16..20], &[0u8; 4]);
        assert_eq!(&out[20..28], &[0xAAu8; 8]);
        assert_eq!(&out[28..32], &[0u8; 4]);
        // Row 2 (dst bytes 32..48): same as row 1 (src row 1).
        assert_eq!(&out[32..36], &[0u8; 4]);
        assert_eq!(&out[36..44], &[0xAAu8; 8]);
        assert_eq!(&out[44..48], &[0u8; 4]);
        // Row 3 (dst bytes 48..64): all zeros.
        assert_eq!(&out[48..64], &[0u8; 16]);
    }

    #[test]
    fn blit_into_larger_frame_is_clamped() {
        // 4x4 frame into 2x2 slot: only min(4,2) x min(4,2) = 2x2 of
        // the frame is copied. blit_into reads `blit_w` bytes per row
        // (= 8 bytes) from the start of each row, so only the first 8
        // bytes of src rows 0 and 1 land in dst.
        let mut src = Vec::with_capacity(4 * 4 * 4);
        for i in 0..(4u32 * 4 * 4) {
            src.push((i & 0xFF) as u8);
        }
        let out = run_blit(&src, 4, 4, 2, 2);
        // 2x2 slot = 16 bytes
        assert_eq!(out.len(), 16);
        // Row 0: 8 bytes from src row 0 (cols 0..2)
        assert_eq!(&out[0..8], &src[0..8]);
        // Row 1: 8 bytes from src row 1 (cols 0..2)
        assert_eq!(&out[8..16], &src[16..24]);
    }
}
