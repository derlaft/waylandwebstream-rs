# Native Wayland Client — Implementation Plan

Goal: a native Wayland client that speaks the same stream-server protocol, usable as a diagnostic
tool to isolate whether observed video-delivery problems are on the client (browser/WebCodecs) or
the server. Supports SW and VAAPI decode, zero-copy when doing GL+VAAPI, Opus/PipeWire audio,
full input forwarding, and latency reporting back to the server.

Secondary goal (prerequisite): unify the current three-endpoint protocol (`/ws` + `/stream` +
`/audio`) into one byte-stream channel that works identically over WebSocket (in the future: also
stdio and unix sockets). Note: this plan was initially created with unix socket and stdio support as one of the first steps.
If you need some stubs, please feel free to add them, otherwise you can remove them.

Implement the phases in order. Each phase ends with a verifiable milestone.

---

## Part 1 — Unified Binary Protocol

### 1.1 Message framing

Every message in both directions uses an 8-byte header followed by a payload:

```
byte  0   : msg_type (u8)
byte  1   : flags    (u8, meaning depends on msg_type; 0 for control messages)
bytes 2-3 : reserved (u16, always 0)
bytes 4-7 : payload_len (u32, little-endian)
bytes 8.. : payload  (payload_len bytes)
```

Over a WebSocket transport each WS binary message carries exactly one framed message (the
8-byte header plus its payload). In the future over a byte-stream transport (Unix socket, TCP, stdio) the
receiver reads the 8-byte header first, then reads `payload_len` more bytes.

The header is always sent, even over WebSocket. This keeps the framing code identical across
all transports.

### 1.2 Message type table

**Server → client (0x01–0x0F)**

| Byte | Name        | flags bits | Payload layout |
|------|-------------|------------|----------------|
| 0x01 | VIDEO_FRAME | bit 0 = is_keyframe, bit 1 = has_ping_echo | see §1.3 |
| 0x02 | AUDIO_FRAME | 0 | see §1.4 |
| 0x03 | CONTROL     | 0 | UTF-8 JSON (existing `ServerMessage` enum unchanged) |

**Client → server (0x10–0x1F)**

| Byte | Name        | flags | Payload layout |
|------|-------------|-------|----------------|
| 0x10 | CLIENT_MSG  | 0     | UTF-8 JSON (existing `SignalingMessage` enum unchanged) |

### 1.3 VIDEO_FRAME payload layout

```
bytes 0-3  : frame_id (u32, big-endian, matches existing wire format)
bytes 4-11 : ping_echo_client_ts (f64, big-endian; 0.0 when flags bit 1 is clear)
bytes 12-19: capture_to_encode_ms (f64, big-endian)
bytes 20.. : raw Annex-B H.264 NAL data
```

The `is_keyframe` and `has_ping_echo` values come from the header `flags` byte rather than
embedding them in the payload bytes, saving the two inline flag bytes the current `/stream`
format uses. Everything else is identical to the existing `encode_video_frame` function in
`src/server.rs` (bytes 1-4 frame_id BE, bytes 6-13 ping_echo BE).

### 1.4 AUDIO_FRAME payload layout

```
bytes 0-7 : pts_us (u64, big-endian)
bytes 8.. : raw Opus packet
```

This is byte-for-byte identical to the existing `/audio` wire format. It is simply wrapped
in the 8-byte header.

### 1.5 Where to put these definitions

Add `src/proto.rs` to the main crate with:

```rust
pub const MSG_VIDEO_FRAME: u8 = 0x01;
pub const MSG_AUDIO_FRAME: u8 = 0x02;
pub const MSG_CONTROL:     u8 = 0x03;
pub const MSG_CLIENT_MSG:  u8 = 0x10;

pub const FLAG_KEYFRAME:    u8 = 0b0000_0001;
pub const FLAG_HAS_PING:    u8 = 0b0000_0010;

pub const HEADER_LEN: usize = 8;

/// Encodes a complete framed message into a Vec<u8>.
pub fn encode_msg(msg_type: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.push(msg_type);
    buf.push(flags);
    buf.push(0); buf.push(0);             // reserved
    let len = payload.len() as u32;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decodes the header from exactly 8 bytes. Returns (msg_type, flags, payload_len).
pub fn decode_header(h: &[u8; 8]) -> (u8, u8, u32) {
    let payload_len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
    (h[0], h[1], payload_len)
}
```

Add `pub mod proto;` to `src/lib.rs`.

The native client will have its own copy of this file at
`native-client/src/proto.rs` (exact duplicate; no shared workspace crate needed yet).

---

## Part 2 — Server-Side Changes

### 2.1 New `/client` WebSocket endpoint

Add a new route `/client` in `src/server.rs` alongside the existing `/ws`, `/stream`, `/audio`.
Do **not** remove or modify the old endpoints — the browser client continues to use them.

The `/client` handler does everything the current `/ws` + `/stream` + `/audio` handlers do,
combined into one connection:

**On connect:**
1. Call `state.session.ensure_started().await`
2. Send `CONTROL(ServerMessage::Codec { codec })` (current codec string)
3. Send `CONTROL(ServerMessage::Bitrate { bps })` (current bitrate)
4. Send `CONTROL(ServerMessage::Cursor { cursor })` (current cursor)
5. Request a fresh keyframe (`EncoderControl::ForceKeyframe`) and set `force_render`
6. Subscribe to `video_tx`, `audio_tx`, `bitrate_rx`, `codec_rx`, `cursor_rx`, `shutdown_rx`

**Receive loop (tokio::select!):**
- `video_rx.recv()` → encode with `encode_unified_video_frame(packet)` → send as WS binary
- `audio_rx.recv()` → encode with `encode_unified_audio_frame(packet)` → send as WS binary
- `bitrate_rx.changed()` → send `CONTROL(ServerMessage::Bitrate { bps })`
- `codec_rx.changed()` → send `CONTROL(ServerMessage::Codec { codec })`
- `cursor_rx.changed()` → send `CONTROL(ServerMessage::Cursor { cursor })`
- `receiver.next()` → parse as `CLIENT_MSG` JSON → dispatch exactly like `websocket_handler`
  (resize, pointer, key, touch, ping, latency, request_keyframe, ready)
- `shutdown_rx.changed()` → send WebSocket Close frame, break

Helper functions to add to `src/server.rs`:

```rust
fn encode_unified_video_frame(packet: &EncodedPacket) -> Vec<u8> {
    let mut flags = 0u8;
    if packet.is_keyframe { flags |= proto::FLAG_KEYFRAME; }
    let has_ping = packet.ping_echo_client_ts.is_some();
    if has_ping { flags |= proto::FLAG_HAS_PING; }

    let mut payload = Vec::with_capacity(20 + packet.data.len());
    payload.extend_from_slice(&packet.frame_id.to_be_bytes());
    let ping_val = packet.ping_echo_client_ts.unwrap_or(0.0);
    payload.extend_from_slice(&ping_val.to_be_bytes());
    payload.extend_from_slice(&packet.capture_to_encode_ms.to_be_bytes());
    payload.extend_from_slice(&packet.data);

    proto::encode_msg(proto::MSG_VIDEO_FRAME, flags, &payload)
}

fn encode_unified_audio_frame(packet: &AudioPacket) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8 + packet.data.len());
    payload.extend_from_slice(&packet.pts_us.to_be_bytes());
    payload.extend_from_slice(&packet.data);
    proto::encode_msg(proto::MSG_AUDIO_FRAME, 0, &payload)
}

fn encode_unified_control(msg: &ServerMessage) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("ServerMessage always serializes");
    proto::encode_msg(proto::MSG_CONTROL, 0, &json)
}
```

For the incoming `CLIENT_MSG`, read each WS binary message, parse the 8-byte header, verify
`msg_type == MSG_CLIENT_MSG`, then `serde_json::from_slice` the payload as `SignalingMessage`
and dispatch it with the same match block as `websocket_handler`.


## Part 3 — Native Client Crate

### 3.0 Crate setup

File: `native-client/Cargo.toml`

```toml
[package]
name = "native-client"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "wws-client"
path = "src/main.rs"

[dependencies]
# Wayland window + input
wayland-client = "0.31"
wayland-protocols = { version = "0.32", features = ["client"] }
wayland-egl = "0.32"           # WlEglWindow for EGL surface

# EGL + OpenGL ES
khronos-egl = { version = "6", features = ["dynamic"] }
gl = "0.14"

# Video decode
ffmpeg-next = "8.1"

# Audio decode + playback
opus = "0.3"
pipewire = "0.10"

# Keyboard input (evdev keycode → KeyboardEvent.code)
# No extra crate needed; we write the reverse lookup table ourselves.

# Transport / async
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = "0.26"
futures-util = "0.3"

# Serialization (reuse server's message types via JSON)
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Utilities
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap = { version = "4", features = ["derive"] }
bytes = "1"
```

Add `"native-client"` to the `[workspace] members` list in the root `Cargo.toml`.

### 3.1 Directory structure

```
native-client/src/
  main.rs          CLI, top-level wiring
  proto.rs         copy of src/proto.rs from main crate
  types.rs         re-export SignalingMessage / ServerMessage / CursorUpdate (copy from server)
  transport/
    mod.rs         Transport trait + Frame / ClientMsg enums
    websocket.rs   WebSocket transport
    unix.rs        Unix socket transport
    tcp.rs         TCP stream transport (for SSH port forwarding)
    stdio.rs       Stdin/stdout transport
  decode/
    mod.rs         Decoder trait + DecodedFrame enum
    sw.rs          Software H.264 decode (ffmpeg libx264)
    vaapi.rs       VAAPI H.264 decode (ffmpeg h264_vaapi) + DMA-buf export
  render/
    mod.rs         Renderer trait
    shm.rs         Wayland wl_shm software blit
    egl.rs         EGL + OpenGL ES renderer (both SW-upload and zero-copy DMA-buf paths)
  audio/
    mod.rs         Opus decode + PipeWire playback
  input/
    mod.rs         Wayland input event capture + translation to SignalingMessage JSON
    keymap.rs      evdev keycode → KeyboardEvent.code string table
  latency/
    mod.rs         Ping/pong tracking + latency report assembly
  display/
    mod.rs         Wayland registry, surface, xdg_toplevel
```

### 3.2 CLI (`main.rs`)

```
Usage: wws-client [OPTIONS] <TRANSPORT>

Transports:
  ws <URL>          WebSocket URL, e.g. ws://localhost:8080/client

Options:
  --decoder <sw|vaapi>     Video decoder [default: sw]
  --renderer <shm|egl>     Renderer [default: egl]
  --vaapi-device <PATH>    VAAPI device [default: /dev/dri/renderD128]
  --no-audio               Disable audio playback
  --no-input               View-only (no input events sent)
  --size <WxH>             Initial window size [default: 1280x720]
```

Main entry point:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Parse CLI args
    // 2. Init tracing
    // 3. Connect transport (returns Box<dyn Transport>)
    // 4. Init Wayland display (returns DisplayHandle that owns the wl_display)
    // 5. Init decoder (sw or vaapi)
    // 6. Init renderer (shm or egl, needs the display handle)
    // 7. Init audio if not disabled
    // 8. Create channels:
    //      decoded_video_tx / decoded_video_rx
    //      client_msg_tx / client_msg_rx   (outgoing messages to server)
    //      input_event_tx / input_event_rx (from display thread → send loop)
    // 9. Send SignalingMessage::Ready, then SignalingMessage::Resize
    // 10. tokio::try_join!(recv_task, send_task, render_task, display_task, latency_task)
}
```

### 3.3 Transport layer (`transport/mod.rs`)

```rust
pub enum Frame {
    VideoFrame {
        is_keyframe: bool,
        frame_id: u32,
        ping_echo: f64,       // 0.0 = no echo
        capture_to_encode_ms: f64,
        data: Vec<u8>,        // Annex-B H.264
    },
    AudioFrame {
        pts_us: u64,
        data: Vec<u8>,        // Opus packet
    },
    Control(ServerMessage),   // codec / bitrate / cursor update
}

pub trait Transport: Send {
    async fn recv(&mut self) -> anyhow::Result<Frame>;
    async fn send(&mut self, json: &str) -> anyhow::Result<()>;
    // send wraps the JSON in a MSG_CLIENT_MSG framed message
}
```

**`transport/websocket.rs`** — Connect to the server's `/client` endpoint:

```rust
pub struct WsTransport { ws: WebSocketStream<MaybeTlsStream<TcpStream>> }

impl WsTransport {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(url).await?;
        Ok(Self { ws })
    }
}

impl Transport for WsTransport {
    async fn recv(&mut self) -> anyhow::Result<Frame> {
        loop {
            let msg = self.ws.next().await
                .ok_or_else(|| anyhow::anyhow!("WebSocket closed"))??;
            if let tungstenite::Message::Binary(data) = msg {
                return parse_frame(&data);
            }
            // ignore text / ping / pong / close
        }
    }

    async fn send(&mut self, json: &str) -> anyhow::Result<()> {
        let frame = proto::encode_msg(proto::MSG_CLIENT_MSG, 0, json.as_bytes());
        self.ws.send(tungstenite::Message::Binary(frame)).await?;
        Ok(())
    }
}
```

`parse_frame(data: &[u8]) -> anyhow::Result<Frame>`:
- Read 8-byte header via `proto::decode_header`
- Branch on `msg_type`:
  - `MSG_VIDEO_FRAME`: parse frame_id (BE u32 at offset 0), ping_echo (BE f64 at 4),
    capture_to_encode_ms (BE f64 at 12), H.264 data from offset 20; flags → is_keyframe
  - `MSG_AUDIO_FRAME`: parse pts_us (BE u64 at 0), Opus data from offset 8
  - `MSG_CONTROL`: `serde_json::from_slice::<ServerMessage>(&payload)?`
  - unknown type: skip (return an error or a `Frame::Unknown` that the caller ignores)

**`transport/unix.rs`** — `UnixStream`-backed byte-stream transport:

```rust
pub struct UnixTransport {
    reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl UnixTransport {
    pub async fn connect(path: &str) -> anyhow::Result<Self> {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (r, w) = stream.into_split();
        Ok(Self { reader: tokio::io::BufReader::new(r), writer: w })
    }
}
```

`recv` reads 8 bytes, then `payload_len` bytes, calls `parse_frame`.
`send` calls `proto::encode_msg` then `writer.write_all`.

**`transport/tcp.rs`** — identical to unix.rs but using `tokio::net::TcpStream::connect`.

**`transport/stdio.rs`** — identical to unix.rs but using `tokio::io::stdin()` / `tokio::io::stdout()`.

### 3.4 Type definitions (`types.rs`)

Copy the following structs/enums verbatim from the server codebase (they must serialize to
identical JSON). Do not share them via a workspace crate yet — a copy is simpler.

- `SignalingMessage` (from `src/server.rs`)
- `ServerMessage` (from `src/server.rs`)
- `CursorUpdate` (from `src/server.rs`)
- `MouseEvent` + `PointerPoint` (from `src/input/mouse.rs`)
- `KeyboardEvent` (from `src/input/keyboard.rs`)
- `TouchEvent` + `TouchPoint` (from `src/input/touch.rs`)

Add `#[derive(Debug, Clone, Serialize, Deserialize)]` to all of them.

Helper to send a `SignalingMessage` via a `Transport`:

```rust
pub async fn send_signaling<T: Transport>(t: &mut T, msg: &SignalingMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    t.send(&json).await
}
```

### 3.5 Wayland display (`display/mod.rs`)

Wayland's `wayland-client` 0.31 event dispatch is synchronous. Run it on a **dedicated OS
thread** to avoid blocking the tokio runtime:

```rust
pub struct DisplayHandle {
    // Sends decoded frames to the render callback on the display thread
    pub frame_tx: std::sync::mpsc::SyncSender<DecodedFrame>,
    // Sends input events from the display thread to the tokio send loop
    pub input_rx: tokio::sync::mpsc::Receiver<SignalingMessage>,
    // Notified when the window is closed (toplevel.close event)
    pub close_rx: tokio::sync::watch::Receiver<bool>,
    // Actual window size after the first configure event
    pub size_rx: tokio::sync::watch::Receiver<(u32, u32)>,
}

pub fn spawn_display_thread(
    renderer: Box<dyn Renderer + Send>,
    initial_size: (u32, u32),
) -> anyhow::Result<DisplayHandle> { ... }
```

Inside the display thread:

1. `Connection::connect_to_env()?`
2. `display.get_registry(&qh, ())`
3. `event_queue.roundtrip(&mut state)` to bind globals
4. Assert `compositor`, `wm_base`, `seat` are present
5. Create `wl_surface`, `xdg_surface`, `xdg_toplevel`
6. Set title/app_id; commit surface; roundtrip to get configure
7. Get `wl_pointer`, `wl_keyboard`, `wl_touch` from seat
8. Call `renderer.surface_ready(wl_surface, width, height)` so the renderer can create its
   backing buffer/EGL window
9. Loop:
   ```rust
   loop {
       // Non-blocking: drain any pending decoded frames and render them
       while let Ok(frame) = frame_rx.try_recv() {
           renderer.render(&frame)?;
       }
       // Dispatch Wayland events (this is where input callbacks fire)
       event_queue.dispatch_pending(&mut state)?;
       event_queue.flush()?;
       // Park briefly so we don't spin at 100% CPU between frames
       std::thread::sleep(Duration::from_millis(1));
   }
   ```

State struct holds all the Wayland object handles plus channels for outgoing input events.

Wayland `Dispatch` implementations:

- `xdg_surface::Event::Configure { serial }` → `xdg_surface.ack_configure(serial)`;
  `surface.commit()`; notify `size_rx` with new dimensions
- `xdg_toplevel::Event::Configure { width, height, states }` → store new size
- `xdg_toplevel::Event::Close` → send `true` on `close_tx`
- `wl_pointer::Event::Enter/Motion` → map x/y to 0..1 floats, send
  `SignalingMessage::Pointer { event: MouseEvent::Move { pointer: PointerPoint { x, y, .. } } }`
- `wl_pointer::Event::Button { button, state }` → translate Linux button code (0x110/0x111/0x112)
  to browser button index (0/2/1); send `SignalingMessage::Pointer { event: MouseEvent::Down/Up }`
- `wl_pointer::Event::Axis { axis, value }` → send `SignalingMessage::Pointer { event: MouseEvent::Wheel }`
- `wl_keyboard::Event::Keymap { fd, size }` → store (for reference; our translation uses a
  static table, not xkb)
- `wl_keyboard::Event::Key { key, state }` → translate `key` (Linux evdev keycode) to
  `KeyboardEvent.code` string via `input::keymap::evdev_to_code(key)`; send
  `SignalingMessage::Key { event: KeyboardEvent::Down/Up { code } }`
- `wl_touch::Event::Down/Up/Motion` → accumulate and send `SignalingMessage::Touch`
- `wl_shm::Event::Format` → record available SHM pixel formats

**Hide the native cursor** (we display the remote app's cursor via the cursor update):
In the `wl_pointer::Event::Enter` handler, call `pointer.set_cursor(serial, None, 0, 0)`.

### 3.6 Software decoder (`decode/sw.rs`)

```rust
pub struct SwDecoder {
    decoder: ffmpeg::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,
}

pub struct DecodedFrame {
    pub data: Vec<u8>,    // RGBA, row-major
    pub width: u32,
    pub height: u32,
    pub frame_id: u32,
    pub ping_echo: f64,
}

impl SwDecoder {
    pub fn new() -> anyhow::Result<Self> {
        ffmpeg::init()?;
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
            .ok_or_else(|| anyhow::anyhow!("H264 codec not found"))?;
        let context = ffmpeg::codec::context::Context::new_with_codec(codec);
        let mut decoder = context.decoder().video()?;
        decoder.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Auto,
            count: 0,
            safe: true,
        });
        // Scaler is created on first frame when we know the dimensions.
        Ok(Self { decoder, scaler: /* placeholder */ ... })
    }

    pub fn decode(&mut self, nal_data: &[u8], frame_id: u32, ping_echo: f64)
        -> anyhow::Result<Option<DecodedFrame>>
    {
        let mut packet = ffmpeg::Packet::copy(nal_data);
        self.decoder.send_packet(&packet)?;
        let mut yuv = ffmpeg::frame::Video::empty();
        match self.decoder.receive_frame(&mut yuv) {
            Ok(()) => {},
            Err(ffmpeg::Error::Other { errno: ffmpeg_next::ffi::EAGAIN }) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        // Recreate scaler if dimensions changed
        if needs_rescaler(&self.scaler, yuv.width(), yuv.height()) {
            self.scaler = ffmpeg::software::scaling::Context::get(
                yuv.format(), yuv.width(), yuv.height(),
                ffmpeg::format::Pixel::RGBA, yuv.width(), yuv.height(),
                ffmpeg::software::scaling::Flags::BILINEAR,
            )?;
        }
        let mut rgba = ffmpeg::frame::Video::empty();
        self.scaler.run(&yuv, &mut rgba)?;
        let width = rgba.width();
        let height = rgba.height();
        let stride = rgba.stride(0);
        // Copy row by row to strip padding
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height as usize {
            let start = row * stride;
            data.extend_from_slice(&rgba.data(0)[start..start + width as usize * 4]);
        }
        Ok(Some(DecodedFrame { data, width, height, frame_id, ping_echo }))
    }
}
```

### 3.7 VAAPI decoder (`decode/vaapi.rs`)

This is more complex. Uses the same raw FFI approach as the server's VAAPI encoder.

```rust
use ffmpeg_next::ffi;

pub struct VaapiDecoder {
    hw_device_ctx: *mut ffi::AVBufferRef,
    decoder: ffmpeg::decoder::Video,
    // VaDisplay for vaExportSurfaceHandle
    va_display: libva::VADisplay,   // or raw *mut c_void
}

pub struct DmaBufFrame {
    pub fd: std::os::unix::io::OwnedFd,
    pub width: u32,
    pub height: u32,
    pub drm_format: u32,            // e.g. DRM_FORMAT_NV12
    pub offsets: [u32; 4],
    pub strides: [u32; 4],
    pub n_planes: u32,
    pub frame_id: u32,
    pub ping_echo: f64,
}
```

**Setup** (in `VaapiDecoder::new(device_path: &str)`):

1. `ffmpeg::init()?`
2. Allocate VAAPI hardware device:
   ```rust
   let device_path_c = CString::new(device_path)?;
   let mut hw_device_ctx: *mut ffi::AVBufferRef = std::ptr::null_mut();
   let ret = unsafe {
       ffi::av_hwdevice_ctx_create(
           &mut hw_device_ctx,
           ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
           device_path_c.as_ptr(),
           std::ptr::null_mut(),
           0,
       )
   };
   if ret < 0 { return Err(anyhow::anyhow!("av_hwdevice_ctx_create failed: {ret}")); }
   ```
3. Find H.264 decoder and create codec context:
   ```rust
   let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
       .ok_or_else(|| anyhow::anyhow!("H264 codec not found"))?;
   let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
   ```
4. Set `hw_device_ctx` and `get_format` callback on the raw `AVCodecContext`:
   ```rust
   let raw_ctx = ctx.as_mut_ptr();
   unsafe {
       (*raw_ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device_ctx);
       (*raw_ctx).get_format = Some(get_format_vaapi);
   }
   ```
   Where `get_format_vaapi` is:
   ```rust
   unsafe extern "C" fn get_format_vaapi(
       _ctx: *mut ffi::AVCodecContext,
       pix_fmts: *const ffi::AVPixelFormat,
   ) -> ffi::AVPixelFormat {
       let mut p = pix_fmts;
       while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
           if *p == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
               return *p;
           }
           p = p.add(1);
       }
       ffi::AVPixelFormat::AV_PIX_FMT_NONE
   }
   ```
5. Open the decoder: `ctx.decoder().video()?`
6. Extract the `VADisplay` from the hardware device context:
   ```rust
   let hw_frames_ctx = unsafe { (*hw_device_ctx).data as *mut ffi::AVHWDeviceContext };
   let vaapi_ctx = unsafe { (*hw_frames_ctx).hwctx as *mut ffi::AVVAAPIDeviceContext };
   let va_display = unsafe { (*vaapi_ctx).display };
   ```

**Per-packet decode** (in `VaapiDecoder::decode`):

1. Send packet to decoder (same as SW path)
2. Receive `AVFrame` with `format == AV_PIX_FMT_VAAPI`
3. Get the `VASurfaceID` from `frame.data[3]`:
   ```rust
   let surface_id = unsafe { (*frame.as_ptr()).data[3] as libva_sys::VASurfaceID };
   ```
4. Export as DMA-buf using `vaExportSurfaceHandle`:
   ```rust
   let mut desc: libva_sys::VADRMPRIMESurfaceDescriptor = std::mem::zeroed();
   let status = unsafe {
       libva_sys::vaExportSurfaceHandle(
           va_display,
           surface_id,
           libva_sys::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
           libva_sys::VA_EXPORT_SURFACE_READ_ONLY,
           &mut desc as *mut _ as *mut std::ffi::c_void,
       )
   };
   if status != libva_sys::VA_STATUS_SUCCESS { return Err(...); }
   ```
5. Return `DmaBufFrame` built from `desc.objects[0].fd`, `desc.layers[0].offset/pitch`, etc.

**Note on libva bindings:** Add `libva-sys = "0.2"` (or similar) to Cargo.toml. If no suitable
crate exists, declare the minimal FFI needed inline using `extern "C"` blocks.

**Fallback:** If `vaExportSurfaceHandle` is unavailable, use `av_hwframe_map` to map the
VAAPI frame to a `DRM_PRIME` frame and get the fd from `frame->data[0]`.

### 3.8 SHM renderer (`render/shm.rs`)

```rust
pub struct ShmRenderer {
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    width: u32,
    height: u32,
    // Double buffer: two slots, alternate between them
    slots: [ShmSlot; 2],
    current: usize,
}

struct ShmSlot {
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    mmap: memmap2::MmapMut,
    in_use: Arc<AtomicBool>,  // set true on attach, cleared in wl_buffer::release
}
```

**`render(frame: &DecodedFrame)`:**
1. Select the slot that is not currently in use (wait for `wl_buffer::release` if both are busy)
2. Copy `frame.data` (RGBA) into `slot.mmap`
3. `surface.attach(Some(&slot.buffer), 0, 0)`
4. `surface.damage_buffer(0, 0, width as i32, height as i32)`
5. `surface.commit()`
6. Mark slot as in-use; rotate `current`

**Resize:** When the display thread gets a new size from `xdg_toplevel::Configure`, recreate
the shm pools at the new dimensions and re-create the wl_buffers. Use `XRGB8888` pixel format
(alpha ignored) or `ARGB8888` depending on what formats the compositor advertises.

### 3.9 EGL renderer (`render/egl.rs`)

This renderer handles two sub-paths:

**Sub-path A: SW decode → GL texture upload (no zero-copy)**

**Sub-path B: VAAPI decode → DMA-buf → EGL image → zero-copy GL texture**

Setup (called once, on the display thread or from `surface_ready`):

1. Load EGL dynamically: `khronos_egl::DynamicInstance::<khronos_egl::EGL1_4>::load_required()?`
2. Get Wayland EGL display:
   ```rust
   let egl_display = egl.get_platform_display(
       EGL_PLATFORM_WAYLAND_KHR,
       wl_display.id().as_ptr() as *mut _,
       &[],
   )?;
   egl.initialize(egl_display)?;
   ```
3. Choose config: RGBA8888, no depth/stencil, `EGL_SURFACE_TYPE = EGL_WINDOW_BIT`
4. `egl.bind_api(EGL_OPENGL_ES_API)`
5. Create context: `EGL_CONTEXT_CLIENT_VERSION = 2`
6. Create `WlEglWindow::new(&surface, width, height)?` (from `wayland-egl` crate)
7. `egl.create_window_surface(display, config, wl_egl_window.ptr(), &[])?`
8. `egl.make_current(display, egl_surface, egl_surface, context)?`
9. Compile shaders (see below)
10. Generate GL texture: `gl::GenTextures(1, &mut tex_id)`

**Shaders for sub-path A** (RGBA texture):

```glsl
// vertex
attribute vec2 a_pos;
attribute vec2 a_tex;
varying vec2 v_tex;
void main() { gl_Position = vec4(a_pos, 0.0, 1.0); v_tex = a_tex; }

// fragment
precision mediump float;
uniform sampler2D u_tex;
varying vec2 v_tex;
void main() { gl_FragColor = texture2D(u_tex, v_tex); }
```

**Shaders for sub-path B** (external OES texture, YUV→RGB on GPU):

```glsl
// fragment — requires #extension GL_OES_EGL_image_external : require
#extension GL_OES_EGL_image_external : require
precision mediump float;
uniform samplerExternalOES u_tex;
varying vec2 v_tex;
void main() { gl_FragColor = texture2D(u_tex, v_tex); }
```

**Render (sub-path A — `DecodedFrame` with RGBA data):**
1. `gl::BindTexture(GL_TEXTURE_2D, tex_id)`
2. `gl::TexImage2D(..., GL_RGBA, GL_UNSIGNED_BYTE, frame.data.as_ptr())`
3. Draw fullscreen quad (two triangles, NDC coords, tex coords flipped Y)
4. `egl.swap_buffers(egl_display, egl_surface)?`

**Render (sub-path B — `DmaBufFrame`):**
1. Build `EGLImage` from the DMA-buf fd:
   ```rust
   let attribs: Vec<EGLAttrib> = vec![
       EGL_WIDTH as EGLAttrib, frame.width as EGLAttrib,
       EGL_HEIGHT as EGLAttrib, frame.height as EGLAttrib,
       EGL_LINUX_DRM_FOURCC_EXT as EGLAttrib, frame.drm_format as EGLAttrib,
       EGL_DMA_BUF_PLANE0_FD_EXT as EGLAttrib, frame.fd.as_raw_fd() as EGLAttrib,
       EGL_DMA_BUF_PLANE0_OFFSET_EXT as EGLAttrib, frame.offsets[0] as EGLAttrib,
       EGL_DMA_BUF_PLANE0_PITCH_EXT as EGLAttrib, frame.strides[0] as EGLAttrib,
       // add plane 1 for NV12 (U+V interleaved)
       EGL_DMA_BUF_PLANE1_FD_EXT as EGLAttrib, frame.fd.as_raw_fd() as EGLAttrib,
       EGL_DMA_BUF_PLANE1_OFFSET_EXT as EGLAttrib, frame.offsets[1] as EGLAttrib,
       EGL_DMA_BUF_PLANE1_PITCH_EXT as EGLAttrib, frame.strides[1] as EGLAttrib,
       EGL_NONE as EGLAttrib,
   ];
   let image = egl.create_image(
       egl_display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, null, &attribs)?;
   ```
2. Bind texture and attach EGL image:
   ```rust
   gl::BindTexture(GL_TEXTURE_EXTERNAL_OES, tex_id);
   gl_egl_image_target_texture_2d_oes(GL_TEXTURE_EXTERNAL_OES, image);
   ```
   `glEGLImageTargetTexture2DOES` must be loaded via `egl.get_proc_address`.
3. Draw quad with the external-OES shader
4. `egl.swap_buffers(...)`
5. `egl.destroy_image(egl_display, image)?` (destroys the EGL image; close the DMA-buf fd)

**Extension checks at startup:**
Query `egl.query_string(display, EGL_EXTENSIONS)` and check for:
- `EGL_EXT_image_dma_buf_import` — required for sub-path B
- `EGL_KHR_image_base`

Query GL extensions string for `GL_OES_EGL_image_external`.

If any extension is missing, log a warning and fall back to sub-path A (SW decode + texture
upload). The renderer selects the sub-path based on which `DecodedFrame` variant it receives.

### 3.10 Audio (`audio/mod.rs`)

Structure mirrors the server's audio capture but in reverse: Opus decode → PipeWire playback.

```rust
pub struct AudioPlayer {
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
}

impl AudioPlayer {
    pub fn spawn() -> anyhow::Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(8);
        std::thread::Builder::new()
            .name("pw-audio-playback".into())
            .spawn(move || run_playback_loop(rx))?;
        Ok(Self { tx })
    }

    pub fn push_opus(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut decoder = // lazily created; store in struct
        let mut pcm = vec![0f32; 960 * 2];
        decoder.decode_float(data, &mut pcm, false)?;
        let _ = self.tx.try_send(pcm);  // drop if backlogged rather than stall
        Ok(())
    }
}
```

`run_playback_loop`:
1. `pw::init()` (not needed in newer versions)
2. Create `MainLoop`, `Context`, `Core`
3. Create `Stream` in `Direction::Output`:
   - name: "wws-client-audio"
   - format: `AudioFormat::F32LE`, rate: 48000, channels: 2
4. Register `process` callback that dequeues from the sync_channel ring and writes PCM into
   the PipeWire buffer
5. `stream.connect(Direction::Output, ...)` with `StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS`
6. `main_loop.run()`

Keep audio playback simple for now — no A/V sync. The latency introduced is small and not the
focus of this debugging tool.

### 3.11 Input keymap (`input/keymap.rs`)

The server accepts `KeyboardEvent.code` strings (browser API physical key identifiers).
Wayland delivers raw Linux evdev keycodes (e.g. `KEY_A = 30`). The mapping from evdev
keycode to `KeyboardEvent.code` string is the **inverse** of the table already in
`src/input/keyboard.rs`.

Build a lookup function:

```rust
pub fn evdev_to_code(key: u32) -> Option<&'static str> {
    match key {
        41 => Some("Backquote"),
        43 => Some("Backslash"),
        // ... copy every entry from keyboard.rs, swap key/value
        _ => None,
    }
}
```

For keys not in the table, log a debug warning and skip. This is fine for a debug tool.

For modifier keys (Shift, Ctrl, Alt, Super), add the missing entries:
- `42` → `"ShiftLeft"`, `54` → `"ShiftRight"`
- `29` → `"ControlLeft"`, `97` → `"ControlRight"`
- `56` → `"AltLeft"`, `100` → `"AltRight"`
- `125` → `"MetaLeft"`, `126` → `"MetaRight"`

### 3.12 Latency tracking (`latency/mod.rs`)

```rust
pub struct LatencyTracker {
    last_frame_arrival: Option<Instant>,
    burst_count: u32,
    window_start: Instant,
    pending_ping_ts: Option<f64>,  // client_ts of the last unanswered ping
    last_ping_sent: Instant,
    // Measurements within the current 5s window
    decode_times_ms: Vec<f64>,
}
```

**`record_arrival(frame_id, ping_echo, now: Instant)`:**
- If `now - last_frame_arrival < 3ms`, increment `burst_count`
- Update `last_frame_arrival = now`
- If `ping_echo != 0.0` and matches `pending_ping_ts`, compute `rtt_ms = now_ms() - ping_echo`;
  store as `network_ms = rtt_ms / 2.0`

**`record_decode_time(ms: f64)`:** append to `decode_times_ms`

**`maybe_send_ping(transport)` — called in the send loop every iteration:**
If 5 seconds have elapsed since `last_ping_sent`:
```rust
let client_ts = system_time_ms(); // millis since Unix epoch as f64
send_signaling(transport, &SignalingMessage::Ping { client_ts }).await?;
self.pending_ping_ts = Some(client_ts);
self.last_ping_sent = Instant::now();
```

**`maybe_flush_report(transport)` — called every 5 seconds:**
Assemble and send:
```rust
SignalingMessage::Latency {
    encoding_ms: None,          // we don't know server-side encoding time
    network_ms: self.last_network_ms,
    jitter_buffer_ms: None,
    decoding_ms: self.decode_times_ms.iter().copied().reduce(f64::max),
    total_ms: self.last_network_ms.unwrap_or(0.0)
             + self.decode_times_ms.iter().copied().reduce(f64::max).unwrap_or(0.0),
    burst_count: self.burst_count,
    blit_ms: None,
}
```
Then reset `burst_count = 0`, `decode_times_ms.clear()`, `window_start = Instant::now()`.

### 3.13 Main event loop (`main.rs`)

```rust
// Channel capacities
const VIDEO_QUEUE: usize = 4;   // small: prefer dropping old frames to backing up
const INPUT_QUEUE: usize = 64;

let (decoded_video_tx, mut decoded_video_rx) = tokio::sync::mpsc::channel(VIDEO_QUEUE);
let (input_event_tx, mut input_event_rx) = tokio::sync::mpsc::channel(INPUT_QUEUE);
let (client_msg_tx, mut client_msg_rx) = tokio::sync::mpsc::channel::<String>(32);

// Display thread runs Wayland events + rendering
let display = display::spawn_display_thread(renderer, initial_size, decoded_video_rx, input_event_tx)?;

// Send initial handshake
send_signaling(&mut transport, &SignalingMessage::Ready).await?;
// Wait for the display thread to give us the actual window size
let (w, h) = *display.size_rx.borrow();
send_signaling(&mut transport, &SignalingMessage::Resize { width: w, height: h }).await?;

let mut tracker = LatencyTracker::new();

// recv_task: network → decode → display queue + audio
let recv_handle = tokio::spawn(async move {
    loop {
        let frame = transport_rx.recv().await?;
        match frame {
            Frame::VideoFrame { is_keyframe, frame_id, ping_echo, data, .. } => {
                tracker.record_arrival(frame_id, ping_echo, Instant::now());
                let t0 = Instant::now();
                let decoded = decoder.decode(&data, frame_id, ping_echo)?;
                let decode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                tracker.record_decode_time(decode_ms);
                if let Some(d) = decoded {
                    // Drop oldest frame if queue is full (non-blocking send)
                    let _ = decoded_video_tx.try_send(d);
                }
            }
            Frame::AudioFrame { data, .. } => {
                if let Some(ref audio) = audio_player {
                    audio.push_opus(&data)?;
                }
            }
            Frame::Control(msg) => {
                match msg {
                    ServerMessage::Codec { codec } => info!("Codec: {codec}"),
                    ServerMessage::Bitrate { bps } => info!("Bitrate: {} kbps", bps / 1000),
                    ServerMessage::Cursor { .. } => { /* display thread handles cursor */ }
                }
            }
        }
    }
});

// send_task: input events + ping/latency → server
let send_handle = tokio::spawn(async move {
    let mut ping_interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            msg = input_event_rx.recv() => {
                let json = serde_json::to_string(&msg.unwrap())?;
                transport_tx.send(&json).await?;
            }
            _ = ping_interval.tick() => {
                tracker.maybe_send_ping(&mut transport_tx).await?;
                tracker.maybe_flush_report(&mut transport_tx).await?;
            }
            _ = display.close_rx.changed() => break,
        }
    }
});

tokio::try_join!(recv_handle, send_handle)?;
```

---

## Part 4 — Implementation Order (Phase by Phase)

Implement in this exact order. Do not skip ahead — later phases depend on earlier ones
compiling cleanly.

### Phase 1 — Protocol definition
1. Add `src/proto.rs` to the server crate with `encode_msg` / `decode_header` / constants
2. Add `pub mod proto;` to `src/lib.rs`
3. `cargo build` — must compile with no warnings
4. **Milestone:** `src/proto.rs` exists and compiles

### Phase 2 — Server `/client` endpoint
1. Add `encode_unified_video_frame`, `encode_unified_audio_frame`, `encode_unified_control` to
   `src/server.rs`
2. Add the `/client` route and `unified_websocket_handler` function
3. `cargo build` — no warnings
4. **Milestone:** `curl -i http://localhost:PORT/client` returns `101 Switching Protocols`
   (or equivalent WebSocket upgrade response), server does not panic

### Phase 3 — Native client skeleton + transport
1. Create `native-client/Cargo.toml` and add to workspace
2. Create `native-client/src/proto.rs` (copy from server)
3. Create `native-client/src/types.rs` (copy message types)
4. Implement `transport/websocket.rs` — connect, recv frames, print them, do not decode
5. `main.rs`: connect ws transport, send Ready, loop printing frame type/size/keyframe flag
6. `cargo run --bin wws-client -- ws ws://localhost:PORT/client`
7. **Milestone:** client connects, prints "VideoFrame id=1 keyframe=true 12345 bytes" etc.

### Phase 4 — Wayland window
1. Add `display/mod.rs` with registry binding, surface creation, xdg_toplevel, configure loop
2. No renderer yet — just create the window and print configure events
3. **Milestone:** a window appears on screen titled "waylandwebstream"

### Phase 5 — SW decode + SHM render
1. Add `decode/sw.rs` (ffmpeg H.264 → RGBA)
2. Add `render/shm.rs` (double-buffered wl_shm blit)
3. Wire together: `recv_task` calls decoder, sends `DecodedFrame` to display thread, display
   thread calls `shm_renderer.render(frame)`, commits surface
4. `cargo run --bin wws-client -- ws ws://localhost:PORT/client`
5. **Milestone:** decoded remote video is visible in the Wayland window

### Phase 6 — Audio ✓ DONE
1. Add `audio/mod.rs` (Opus decoder + PipeWire playback thread)
2. Wire `Frame::AudioFrame` in recv_task to `AudioPlayer::push_opus`
3. **Milestone:** audio plays in sync with video (approximately)

### Phase 7 — Input forwarding ✓ DONE
1. Add `input/keymap.rs` (evdev → KeyboardEvent.code table)
2. Add input dispatch to `display/mod.rs` (pointer, keyboard, touch → SignalingMessage)
3. Wire `input_event_rx` in send_task to the transport
4. **Milestone:** clicking/typing in the native window drives the remote app

### Phase 8 — Latency tracking ✓ DONE
1. Add `latency/mod.rs`
2. Wire `record_arrival` and `record_decode_time` in recv_task
3. Wire `maybe_send_ping` and `maybe_flush_report` in send_task ping_interval tick
4. **Milestone:** server logs show `network Xms decode Xms total Xms` from native client

### Phase 9 — EGL renderer (SW path)
1. Add `render/egl.rs` — EGL init, RGBA texture upload, fullscreen quad, swap
2. Add `--renderer egl` flag
3. **Milestone:** same video visible using GL compositing (visually identical to SHM path)

### Phase 10 — VAAPI decode + zero-copy EGL render
1. Add `decode/vaapi.rs` (h264_vaapi, DMA-buf export via vaExportSurfaceHandle)
2. Add zero-copy path to `render/egl.rs` (EGL image from DMA-buf, external OES)
3. Add `--decoder vaapi` flag
4. Run on the HW-capable remote machine via Unix socket or SSH
5. **Milestone:** decoded video shows on screen with no CPU pixel copies (verify with `perf` or
   `nvidia-smi` equivalent; or just confirm `VaapiDecoder::decode` is called in logs)

### Phase 11 — Unix socket + stdio transports
1. Add `src/unix_server.rs` to the server, `--unix-socket <path>` flag
2. Add `transport/unix.rs` and `transport/stdio.rs` to native client
3. Add `--stdio` flag to server
4. Test: `native-client unix /tmp/wws.sock` while server runs with `--unix-socket /tmp/wws.sock`
5. Test SSH: `ssh -L /tmp/local.sock:/tmp/remote.sock user@host &` then
   `native-client unix /tmp/local.sock`
6. **Milestone:** video plays over Unix socket; identical latency characteristics as WS

---

## Part 5 — Notes and Gotchas

### N1: `ffmpeg-next` and raw FFI for VAAPI

`ffmpeg-next` has no safe VAAPI decode API — `hw_device_ctx` and `get_format` must be set
via `unsafe` FFI. This is the same situation as the server's VAAPI encoder. Look at
`src/encoder/vaapi.rs` for the exact FFI patterns — the decode side is a mirror.

### N2: Wayland event loop vs tokio

Do **not** call `event_queue.blocking_dispatch` from a tokio task — it blocks the async
executor. Use a dedicated OS thread (via `std::thread::spawn`) that loops on
`event_queue.dispatch_pending` + `event_queue.flush` + `thread::sleep(1ms)`. Communicate
with the tokio side via `std::sync::mpsc::sync_channel` (for the frame queue from the recv
task to the display thread) and `tokio::sync::mpsc::channel` (for input events going the
other way). The display thread sends input events using the non-async `try_send` or `blocking_send`.

### N3: Pointer coordinate normalization

The server expects pointer coordinates as 0.0..1.0 floats relative to the compositor surface.
The native client receives `surface_x / surface_y` as `wl_fixed` values (24.8 fixed-point)
via `wl_pointer::Event::Motion`. Convert with `wl_fixed_to_double(v) / window_width` for X
and `/ window_height` for Y.

### N4: Keyboard handling does not need xkbcommon

The server maps `KeyboardEvent.code` → evdev keycode and injects the raw keycode into the
compositor. Our static reverse table in `input/keymap.rs` is sufficient. We do not need to
install or use `xkbcommon`.

### N5: EGL platform extension

The wayland EGL display must be obtained via `eglGetPlatformDisplayEXT(EGL_PLATFORM_WAYLAND_KHR, ...)`.
This requires the `EGL_EXT_platform_wayland` extension. Check for it in
`eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS)` before calling.

### N6: DMA-buf format

VAAPI typically decodes to `NV12` (Y plane + interleaved UV plane). The EGL image for NV12
needs two plane descriptors in the `attribs` array:
- `EGL_DMA_BUF_PLANE0_*` for the Y plane
- `EGL_DMA_BUF_PLANE1_*` for the UV plane

`DRM_FORMAT_NV12 = 0x3231564e`. The `samplerExternalOES` fragment shader handles the YUV →
RGB conversion automatically.

### N7: Audio sync

Do not implement A/V sync in the first version. Just push decoded PCM to PipeWire as it
arrives. This is fine for a diagnostic tool where the goal is to debug video delivery, not
A/V sync.

### N8: Frame drop vs backlog

The decoded video channel capacity is intentionally small (4 slots). In `recv_task`, use
`try_send` rather than `send` — if the display thread is behind, drop the old frame rather
than accumulating a backlog. The server's periodic keyframe will resync after a drop. This
matches the browser client's behavior exactly and is important for comparable diagnostic output.

### N9: `wl_buffer::release` handling

The SHM renderer reuses buffers. The compositor sends a `wl_buffer::release` event when it
is done reading the buffer. The `in_use: Arc<AtomicBool>` flag in `ShmSlot` must be cleared
in the `Dispatch<wl_buffer::WlBuffer>` implementation's `release` event handler. The display
thread must wait (or spin-check) for a released slot before writing to it.

### N10: Cursor hiding

In `wl_pointer::Event::Enter { serial, surface_x, surface_y }`, call:
```rust
pointer.set_cursor(serial, None, 0, 0);
```
This makes the native cursor invisible over the window, so only the remote app's cursor
(from `ServerMessage::Cursor`) is visible. For now, ignore cursor rendering — the native
client can show a plain default cursor or no cursor. Implementing custom cursor rendering
from `CursorUpdate::Surface` can be done after the core pipeline works.

---

## Part 6 — Known Issues and Deferred Work

Issues found during the Phase 3–5 implementation review. Fixed items are noted; open items
need future attention.

### Fixed

**`ShmRenderer::resize()` sent `wl_buffer.destroy()` to held buffers (protocol violation)**
When a resize arrived while the compositor still held a buffer (between attach and release),
dropping the `BufferSlot` sent `wl_buffer.destroy()` before the compositor released it. This
could cause the compositor to black-screen the surface or disconnect. Fixed by introducing
the *zombie* pattern: held slots are marked `zombie = true` and kept alive in `slots[idx]`;
`release_slot()` drops them when the compositor sends Release. Released slots are still freed
immediately on resize. `render()` and `prime()` skip zombie slots when selecting a write target.

**`xdg_surface::Configure` had no follow-up `surface.commit()`**
The xdg-shell spec requires a commit after `ack_configure` so the compositor considers the
configure "applied." Without it the compositor may keep sending additional Configure events or
consider the client non-responsive. Fixed by storing `wl_surface` in `DisplayState` and
calling `surface.commit()` immediately after `ack_configure` in the `xdg_surface` dispatch
handler. This re-commits the current pending state (no new attach) and is purely a protocol
acknowledgement.

**`decode/sw.rs` error message was garbled**
`anyhow::bail!("decoder emitted a 0x{} frame", "frame")` produced the message
`"decoder emitted a 0xframe frame"`. Fixed to `"decoder emitted a zero-dimension frame: {w}x{h}"`.

**`tracing::info!` on every `wl_buffer::Release`**
Every compositor buffer release was logged at INFO level, flooding output at 60 fps. Changed
to `debug!`.

**`payload-client` binary was in `native-client` instead of `wayland-test-client`**
It is a test-only helper used only by `smoke_e2e.rs`. Moved to `wayland-test-client/src/`
and updated both crates' `Cargo.toml` accordingly.

### Open — needs investigation or future work

**`try_drain()` silently drops frames when no slot is available**
`ShmRenderer::render()` returns `Ok(false)` when both slots are held (or are zombies and no
fresh slot exists yet). `try_drain()` calls `self.render(qh, &frame)?` and ignores the bool;
no log is emitted at this level. If frames are consistently dropped here the `render_counter`
never increments and the smoke test times out. Consider logging at debug level so the gap
between "decoder produced a frame" and "frame reached the surface" is observable.

**`smoke_e2e.rs` env var restore skipped on panic inside `block_on`**
`run_visual_pipeline()` restores `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` after the async
block returns, but `std::panic::catch_unwind` is only set around the entire function call in
`run_test()`. A panic originating inside the `block_on` future propagates out of
`run_visual_pipeline` without reaching the restore code, leaving the process env dirty for any
subsequent tests in the same binary. Low risk (single test file today), but worth fixing if
more tests are added.

**Smoke test window may not fill labwc's headless output**
`grim` captures the entire Wayland output. The wss-client window (1280×720) may be smaller
than labwc's headless output (wlroots default is implementation-defined), so most pixels in
the screenshot would be compositor background instead of video. The `COLOR_MATCH_THRESHOLD`
of 95% would then fail. If the test consistently fails on the screenshot assertion even when
the pipeline is healthy, adding `toplevel.set_maximized()` in `display/mod.rs` (before the
configure roundtrip) should make the window fill the output. Note: maximizing will trigger a
Configure with the output resolution; the zombie-slot fix ensures this doesn't corrupt the
surface, but the server would still encode at 1280×720. The smoke test's `grim` would then
capture a letterboxed frame (colored center, black border). Either configure the server to
match or check only the center region of the screenshot.

**`wl_seat` input forwarding is a stub (Phase 7)**
`Dispatch<wl_seat::WlSeat>` is bound but ignores all events. Pointer, keyboard, and touch
input forwarding are deferred to Phase 7.
