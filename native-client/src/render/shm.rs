// Double-buffered wl_shm renderer for decoded video frames.
//
// `ShmRenderer` is owned by the Wayland display thread and holds the
// `wl_surface` + `wl_shm` proxies the renderer needs to allocate new
// buffers. It also owns the two buffer slots; the `wl_buffer` user
// data carries the slot index so the `wl_buffer::Release` Dispatch
// impl can find the right slot to mark available again (see
// `DisplayState::release_buffer`).
//
// Resolution policy (see Q2 in Phase 5 design): on a size mismatch
// between the decoded frame and the window we do a centered memcpy of
// `min(frame, buf)` rows/cols -- the compositor scales the rest. This
// avoids a second swscale pass on the hot path. The server already
// rescales on resize so the race is short.
//
// Vsync: Phase 5 commits on every decoded frame. That can mean
// 30-60 commits/sec, well within what wl_shm handles. Phase 9 (EGL
// renderer) replaces this with `wl_callback` frame throttling.

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

const NUM_SLOTS: usize = 2;

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
}

impl ShmRenderer {
    /// Build a renderer at `width x height`. Allocates both slots
    /// immediately so the very first commit can hand the compositor a
    /// real buffer.
    pub fn new<State>(
        shm: wl_shm::WlShm,
        surface: wl_surface::WlSurface,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
    ) -> Result<Self>
    where
        State: Dispatch<wl_shm_pool::WlShmPool, u32>
            + Dispatch<wl_buffer::WlBuffer, u32>
            + 'static,
    {
        let mut renderer = Self {
            shm,
            surface,
            slots: [None, None],
            width,
            height,
            next_idx: 0,
        };
        renderer.recreate_slots(qh)?;
        Ok(renderer)
    }

    /// Drop and reallocate both slots at the current `width x height`.
    /// Called from `new` and from `resize`.
    fn recreate_slots<State>(
        &mut self,
        qh: &QueueHandle<State>,
    ) -> Result<()>
    where
        State: Dispatch<wl_shm_pool::WlShmPool, u32>
            + Dispatch<wl_buffer::WlBuffer, u32>
            + 'static,
    {
        for idx in 0..NUM_SLOTS {
            let slot = create_buffer_slot(&self.shm, qh, idx as SlotId, self.width, self.height)
                .with_context(|| format!("create buffer slot {idx}"))?;
            self.slots[idx] = Some(slot);
        }
        Ok(())
    }

    /// Resize the renderer to `new_w x new_h`. Slots are dropped and
    /// rebuilt on the next `render` or `prime` call (we don't
    /// allocate here so callers can call resize at any point without
    /// holding the surface flush).
    pub fn resize(&mut self, new_w: u32, new_h: u32) {
        if (new_w, new_h) == (self.width, self.height) {
            return;
        }
        self.width = new_w;
        self.height = new_h;
        // Drop slots now -- the renderer doesn't allocate yet, so we
        // can defer the actual mmap + pool creation until render
        // fires. (Allocating here would also be fine; deferring keeps
        // resize() non-fallible.)
        for slot in &mut self.slots {
            *slot = None;
        }
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
                .map(|s| s.released)
                .unwrap_or(false)
        });
        let Some(idx) = picked else {
            return false;
        };
        let slot = self.slots[idx].as_mut().unwrap();
        slot.released = false;
        self.surface.attach(Some(&slot.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, slot.width as i32, slot.height as i32);
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
        State: Dispatch<wl_shm_pool::WlShmPool, u32>
            + Dispatch<wl_buffer::WlBuffer, u32>
            + 'static,
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
            }
        }
    }

    /// Attach + commit one decoded frame. Returns `Ok(false)` if both
    /// slots are still held by the compositor and we had to drop the
    /// frame (caller may log a warning; this is rare on a healthy
    /// session and self-heals on the next keyframe).
    pub fn render<State>(
        &mut self,
        qh: &QueueHandle<State>,
        frame: &DecodedFrame,
    ) -> Result<bool>
    where
        State: Dispatch<wl_shm_pool::WlShmPool, u32>
            + Dispatch<wl_buffer::WlBuffer, u32>
            + 'static,
    {
        if self.slots[0].is_none() || self.slots[1].is_none() {
            self.recreate_slots(qh)?;
        }

        // Pick the next *released* slot; toggle `next_idx` so we
        // ping-pong between slots for cache friendliness.
        let released: [bool; NUM_SLOTS] = [
            self.slots[0].as_ref().map(|s| s.released).unwrap_or(false),
            self.slots[1].as_ref().map(|s| s.released).unwrap_or(false),
        ];
        let Some(idx) = pick_next_released(&released, self.next_idx) else {
            // Both slots are held by the compositor. This happens
            // briefly after a commit (the compositor hasn't released
            // the previous buffer yet, usually <1 vsync). If this
            // log line is spammed, the compositor is *not* sending
            // wl_buffer::Release for our buffers -- which is the bug
            // we hit before fixing dispatch_pending -> blocking_dispatch.
            tracing::debug!(
                "no released slot; dropping frame (slot_state={})",
                self.slot_state()
            );
            return Ok(false);
        };

        // SAFETY: we just checked the slot exists.
        let slot = self.slots[idx].as_mut().unwrap();
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
        self.surface.damage_buffer(0, 0, slot.width as i32, slot.height as i32);
        self.surface.commit();

        self.next_idx = (idx + 1) % NUM_SLOTS;
        tracing::info!(
            "rendered slot={idx} {}x{} ({}x{} source)",
            slot.width, slot.height, frame.width, frame.height
        );
        Ok(true)
    }

    /// Diagnostic: report which slots are currently released. Useful
    /// when the renderer appears stuck (no released slot -> no render
    /// possible). Returns a string like `[released,held]`.
    pub fn slot_state(&self) -> String {
        format!(
            "[{},{}]",
            self.slots[0].as_ref().map(|s| s.released).unwrap_or(false),
            self.slots[1].as_ref().map(|s| s.released).unwrap_or(false),
        )
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

/// Allocate one wl_shm memfd + pool + buffer pair. Mirrors
/// `attach_placeholder_buffer` in `display/mod.rs` but returns the
/// components (the placeholder path stays because Phase 4 needs it
/// before any decoded frame arrives -- the renderer takes over on the
/// first frame).
fn create_buffer_slot<State>(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    slot_id: SlotId,
    width: u32,
    height: u32,
) -> Result<BufferSlot>
where
    State: Dispatch<wl_shm_pool::WlShmPool, u32>
        + Dispatch<wl_buffer::WlBuffer, u32>
        + 'static,
{
    const BYTES_PER_PIXEL: usize = 4;
    let stride = width as usize * BYTES_PER_PIXEL;
    let size = stride * height as usize;

    let name = CString::new("wws-client-shm").unwrap();
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        anyhow::bail!(
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let ret = unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) };
    if ret < 0 {
        anyhow::bail!("ftruncate({size}) failed: {}", std::io::Error::last_os_error());
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
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
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
            std::ptr::copy_nonoverlapping(src.as_ptr().add(src_off), dst_ptr.add(dst_off), row_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_next_released_prefers_preferred_when_released() {
        assert_eq!(pick_next_released(&[true, true], 0), Some(0));
        assert_eq!(pick_next_released(&[true, true], 1), Some(1));
    }

    #[test]
    fn pick_next_released_falls_back_when_preferred_held() {
        // preferred=0 held, 1 free -> use 1
        assert_eq!(pick_next_released(&[false, true], 0), Some(1));
        // preferred=1 held, 0 free -> use 0
        assert_eq!(pick_next_released(&[true, false], 1), Some(0));
    }

    #[test]
    fn pick_next_released_returns_none_when_all_held() {
        // The pathological state the original bug produced: both
        // slots marked unreleased because the compositor's
        // wl_buffer::Release events were never read off the socket.
        // (See run_display_loop's blocking_dispatch comment.)
        assert_eq!(pick_next_released(&[false, false], 0), None);
        assert_eq!(pick_next_released(&[false, false], 1), None);
    }

    #[test]
    fn pick_next_released_rotates_correctly() {
        // preferred=1 but slot 1 held -> try 0 -> free
        assert_eq!(pick_next_released(&[true, false], 1), Some(0));
        // preferred=0 but slot 0 held -> try 1 -> free
        assert_eq!(pick_next_released(&[false, true], 0), Some(1));
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
