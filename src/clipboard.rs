//! Clipboard bridge between the browser and the *nested* compositor.
//!
//! Our headless compositor hosts a second compositor (the DE the user runs);
//! apps talk to that nest, not to us, so we can't see their clipboard through
//! our own `wl_data_device`. Instead we act as a **data-control client of the
//! nested compositor** -- the focus-independent clipboard access that clipboard
//! managers use -- and plumb it to the browser over the `/client` WebSocket.
//!
//! `data-control` is event-driven (the device emits `selection` on every
//! change), so remote->device needs no polling. We support both the
//! standardized `ext-data-control-v1` (KDE 6, GNOME >= 49, recent wlroots) and
//! the legacy `zwlr-data-control` (wlroots 0.18 / labwc 0.8, sway, hyprland),
//! preferring ext. (cage exposes no data-control at all and can't be bridged.)
//!
//! The whole thing runs on one dedicated thread with a calloop event loop:
//! the wayland queue (inbound clipboard) and a calloop channel (outbound text
//! to set on the nest) are both sources, so it stays single-threaded.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use os_pipe::pipe;
use rustix::fs::{fcntl_setfl, OFlags};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self as ext_device, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self as ext_offer, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self as ext_source, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self as wlr_device, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self as wlr_offer, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self as wlr_source, ZwlrDataControlSourceV1},
};

/// Text MIME types we offer (device->remote) and look for (remote->device), in
/// preference order. The `STRING`/`UTF8_STRING`/`TEXT` aliases keep X11 apps
/// (XWayland inside the nest) happy.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// Image MIME types we look for, in preference order. `image/png` is the only
/// type browsers reliably read *and* write, so it leads; any other `image/*`
/// is read through but rarely needed.
const IMAGE_MIMES: &[&str] = &["image/png"];

/// Don't bridge clipboard payloads larger than this. Keeps a single WebSocket
/// message under tungstenite's 16 MiB frame limit and bounds memory/abuse.
pub const MAX_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;

/// A clipboard payload flowing through the bridge in either direction. Text
/// rides the JSON control channel; images ride a binary frame (see src/proto).
#[derive(Clone)]
pub enum ClipboardData {
    Text(String),
    Image { mime: String, bytes: Vec<u8> },
}

fn text_mimes() -> Vec<String> {
    TEXT_MIMES.iter().map(|s| (*s).to_string()).collect()
}

/// Picks which MIME type to read from an offer: image first (so a copied
/// picture comes through as an image rather than its text/html fallback), else
/// text. Returns `(mime, is_image)`.
fn choose_read_mime(mimes: &[String]) -> Option<(String, bool)> {
    if let Some(m) = IMAGE_MIMES.iter().find(|m| mimes.iter().any(|x| x == *m)) {
        return Some(((*m).to_string(), true));
    }
    if let Some(m) = mimes.iter().find(|x| x.starts_with("image/")) {
        return Some((m.clone(), true));
    }
    if let Some(m) = TEXT_MIMES.iter().find(|m| mimes.iter().any(|x| x == *m)) {
        return Some(((*m).to_string(), false));
    }
    if let Some(m) = mimes.iter().find(|x| x.starts_with("text/")) {
        return Some((m.clone(), false));
    }
    None
}

// ─── ext/wlr abstraction ──────────────────────────────────────────────────────
// The two data-control protocols are byte-for-byte identical in shape; these
// enums let the handler logic stay protocol-agnostic.

#[derive(Clone)]
enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}
#[derive(Clone)]
enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}
#[derive(Clone)]
enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}
#[derive(Clone)]
enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl Manager {
    fn get_data_device(&self, seat: &WlSeat, qh: &QueueHandle<ClipState>) -> Device {
        match self {
            Manager::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
            Manager::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
        }
    }
    fn create_data_source(&self, qh: &QueueHandle<ClipState>) -> Source {
        match self {
            Manager::Ext(m) => Source::Ext(m.create_data_source(qh, ())),
            Manager::Wlr(m) => Source::Wlr(m.create_data_source(qh, ())),
        }
    }
}
impl Device {
    fn set_selection(&self, source: Option<&Source>) {
        match self {
            Device::Ext(d) => d.set_selection(source.map(Source::ext)),
            Device::Wlr(d) => d.set_selection(source.map(Source::wlr)),
        }
    }
}
impl Source {
    fn ext(&self) -> &ExtDataControlSourceV1 {
        match self {
            Source::Ext(s) => s,
            _ => unreachable!("ext source on wlr device"),
        }
    }
    fn wlr(&self) -> &ZwlrDataControlSourceV1 {
        match self {
            Source::Wlr(s) => s,
            _ => unreachable!("wlr source on ext device"),
        }
    }
    fn offer(&self, mime: String) {
        match self {
            Source::Ext(s) => s.offer(mime),
            Source::Wlr(s) => s.offer(mime),
        }
    }
    fn destroy(&self) {
        match self {
            Source::Ext(s) => s.destroy(),
            Source::Wlr(s) => s.destroy(),
        }
    }
    fn id(&self) -> ObjectId {
        match self {
            Source::Ext(s) => s.id(),
            Source::Wlr(s) => s.id(),
        }
    }
}
impl Offer {
    fn receive(&self, mime: String, fd: BorrowedFd) {
        match self {
            Offer::Ext(o) => o.receive(mime, fd),
            Offer::Wlr(o) => o.receive(mime, fd),
        }
    }
    fn destroy(&self) {
        match self {
            Offer::Ext(o) => o.destroy(),
            Offer::Wlr(o) => o.destroy(),
        }
    }
    fn id(&self) -> ObjectId {
        match self {
            Offer::Ext(o) => o.id(),
            Offer::Wlr(o) => o.id(),
        }
    }
}

// ─── bridge ────────────────────────────────────────────────────────────────────

/// Spawns the clipboard bridge on a dedicated OS thread.
///
/// - `nested_display`: the `WAYLAND_DISPLAY` name of the nested compositor.
/// - `to_remote`: text the browser put on the device clipboard, to set as the
///   nest selection (device -> remote).
/// - `from_remote`: latest nest selection text, pushed to browsers (remote ->
///   device). A `watch` so a newly connected client sees the current value.
pub fn spawn(
    nested_display: String,
    to_remote: calloop::channel::Channel<ClipboardData>,
    from_remote: watch::Sender<ClipboardData>,
) {
    std::thread::Builder::new()
        .name("clipboard-bridge".into())
        .spawn(move || {
            if let Err(e) = run(&nested_display, to_remote, from_remote) {
                warn!("Clipboard bridge stopped: {e:#}");
            }
        })
        .expect("spawn clipboard-bridge thread");
}

fn socket_path(display: &str) -> PathBuf {
    if display.starts_with('/') {
        return PathBuf::from(display);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    PathBuf::from(dir).join(display)
}

fn run(
    nested_display: &str,
    to_remote: calloop::channel::Channel<ClipboardData>,
    from_remote: watch::Sender<ClipboardData>,
) -> anyhow::Result<()> {
    let path = socket_path(nested_display);
    let stream = UnixStream::connect(&path)
        .map_err(|e| anyhow::anyhow!("connect to nested compositor {path:?}: {e}"))?;
    let conn = Connection::from_socket(stream)?;

    let (globals, event_queue) = registry_queue_init::<ClipState>(&conn)?;
    let qh = event_queue.handle();

    let seat: WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| anyhow::anyhow!("nested compositor exposes no wl_seat: {e}"))?;

    // Prefer ext-data-control; fall back to the legacy wlr protocol.
    let manager = if let Ok(m) = globals.bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
        info!("Clipboard bridge using ext-data-control");
        Manager::Ext(m)
    } else if let Ok(m) = globals.bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ()) {
        info!("Clipboard bridge using zwlr-data-control");
        Manager::Wlr(m)
    } else {
        warn!(
            "Nested compositor '{nested_display}' has no data-control; clipboard \
             sync disabled (need KDE>=6 / GNOME>=49 / wlroots / labwc; cage is \
             unsupported)"
        );
        return Ok(());
    };
    let device = manager.get_data_device(&seat, &qh);

    let mut state = ClipState {
        conn: conn.clone(),
        qh: qh.clone(),
        manager,
        device,
        offer_mimes: HashMap::new(),
        source: None,
        serve_bytes: Vec::new(),
        serve_mimes: Vec::new(),
        owning: false,
        last_value: None,
        from_remote,
    };

    let mut event_loop: EventLoop<ClipState> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;
    handle
        .insert_source(to_remote, |event, _, state| {
            if let calloop::channel::Event::Msg(text) = event {
                state.set_selection(text);
            }
        })
        .map_err(|e| anyhow::anyhow!("insert clipboard channel: {e}"))?;

    info!("Clipboard bridge connected to nested compositor '{nested_display}'");
    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}

struct ClipState {
    conn: Connection,
    qh: QueueHandle<ClipState>,
    manager: Manager,
    device: Device,
    /// MIME types collected per data offer, keyed by the offer's object id.
    offer_mimes: HashMap<ObjectId, Vec<String>>,
    /// The source we currently own the selection with (device -> remote).
    source: Option<Source>,
    /// Bytes that source serves on `send`, and the MIME types it advertises.
    serve_bytes: Vec<u8>,
    serve_mimes: Vec<String>,
    /// True while we own the selection, so the compositor's echo `selection`
    /// event for our own source isn't read back as a remote change (which
    /// would also self-deadlock, since we'd be both writer and reader).
    owning: bool,
    /// Last payload bytes seen in either direction -- dedupes so identical
    /// content doesn't loop between device and remote. Bytes-only (not keyed by
    /// MIME) so a text/plain vs text/plain;charset=utf-8 label can't defeat it.
    last_value: Option<Vec<u8>>,
    from_remote: watch::Sender<ClipboardData>,
}

impl ClipState {
    /// device -> remote: take ownership of the nest selection and serve `data`.
    fn set_selection(&mut self, data: ClipboardData) {
        let (bytes, mimes) = match data {
            ClipboardData::Text(text) => (text.into_bytes(), text_mimes()),
            ClipboardData::Image { mime, bytes } => (bytes, vec![mime]),
        };
        if bytes.len() > MAX_CLIPBOARD_BYTES {
            warn!("clipboard: device payload too large ({} bytes), dropping", bytes.len());
            return;
        }
        if self.last_value.as_deref() == Some(bytes.as_slice()) {
            return; // already the current clipboard value; nothing to do
        }
        let source = self.manager.create_data_source(&self.qh);
        for mime in &mimes {
            source.offer(mime.clone());
        }
        self.device.set_selection(Some(&source));
        self.serve_bytes = bytes.clone();
        self.serve_mimes = mimes;
        self.source = Some(source);
        self.owning = true;
        self.last_value = Some(bytes);
        let _ = self.conn.flush();
        debug!("clipboard: set nested selection ({} bytes)", self.serve_bytes.len());
    }

    /// remote -> device: read the offered selection (image preferred, else
    /// text) and push it to browsers.
    fn read_offer(&mut self, off: &Offer) {
        let mimes = self.offer_mimes.get(&off.id()).cloned().unwrap_or_default();
        let Some((mime, is_image)) = choose_read_mime(&mimes) else {
            debug!("clipboard: nested selection has no text/image mime, ignoring");
            return;
        };

        let (mut reader, writer) = match pipe() {
            Ok(p) => p,
            Err(e) => {
                warn!("clipboard: pipe creation failed: {e}");
                return;
            }
        };
        off.receive(mime.clone(), writer.as_fd());
        drop(writer); // so the read sees EOF once the source finishes writing
        if self.conn.flush().is_err() {
            return;
        }
        // Bounded read so a huge selection can't blow up memory or exceed the
        // frame limit; if it overruns the cap we drop it.
        let mut buf = Vec::new();
        if let Err(e) = (&mut reader)
            .take((MAX_CLIPBOARD_BYTES + 1) as u64)
            .read_to_end(&mut buf)
        {
            warn!("clipboard: reading nested selection failed: {e}");
            return;
        }
        if buf.len() > MAX_CLIPBOARD_BYTES {
            warn!("clipboard: nested selection too large (> {MAX_CLIPBOARD_BYTES} bytes), dropping");
            return;
        }
        if self.last_value.as_deref() == Some(buf.as_slice()) {
            return; // unchanged / our own value coming back
        }
        debug!("clipboard: nested selection -> device ({} bytes, {mime})", buf.len());
        self.last_value = Some(buf.clone());
        let data = if is_image {
            ClipboardData::Image { mime, bytes: buf }
        } else {
            ClipboardData::Text(String::from_utf8_lossy(&buf).into_owned())
        };
        let _ = self.from_remote.send(data);
    }

    // Shared (protocol-agnostic) device/source event handling.
    fn on_data_offer(&mut self, offer_id: ObjectId) {
        // Mime types arrive as `offer` events next, before `selection`.
        self.offer_mimes.insert(offer_id, Vec::new());
    }
    fn on_selection(&mut self, off: Option<Offer>) {
        if let Some(off) = off {
            if !self.owning {
                self.read_offer(&off);
            }
            self.offer_mimes.remove(&off.id());
            off.destroy();
        }
    }
    fn on_offer_mime(&mut self, offer_id: ObjectId, mime: String) {
        self.offer_mimes.entry(offer_id).or_default().push(mime);
    }
    fn on_source_send(&mut self, fd: OwnedFd) {
        // Clear O_NONBLOCK so the blocking write below can't WouldBlock.
        let _ = fcntl_setfl(&fd, OFlags::empty());
        let mut file = std::fs::File::from(fd);
        use std::io::Write as _;
        if let Err(e) = file.write_all(&self.serve_bytes) {
            debug!("clipboard: serving selection to nest failed: {e}");
        }
    }
    fn on_source_cancelled(&mut self, src: Source) {
        // Someone else took the nest selection -- stop owning so the next
        // `selection` event is read as a genuine remote change.
        let id = src.id();
        src.destroy();
        if self.source.as_ref().map(Source::id) == Some(id) {
            self.source = None;
            self.owning = false;
        }
    }
}

// ─── dispatch: registry / seat / manager (no events we use) ─────────────────────
impl Dispatch<WlRegistry, GlobalListContents> for ClipState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlSeat, ()> for ClipState {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtDataControlManagerV1, ()> for ClipState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ─── dispatch: device (ext + wlr) ───────────────────────────────────────────────
impl Dispatch<ExtDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_device::Event::DataOffer { id } => state.on_data_offer(id.id()),
            ext_device::Event::Selection { id } => state.on_selection(id.map(Offer::Ext)),
            _ => {}
        }
    }
    event_created_child!(ClipState, ExtDataControlDeviceV1, [
        ext_device::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}
impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: wlr_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wlr_device::Event::DataOffer { id } => state.on_data_offer(id.id()),
            wlr_device::Event::Selection { id } => state.on_selection(id.map(Offer::Wlr)),
            _ => {}
        }
    }
    event_created_child!(ClipState, ZwlrDataControlDeviceV1, [
        wlr_device::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

// ─── dispatch: offer (ext + wlr) ────────────────────────────────────────────────
impl Dispatch<ExtDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        off: &ExtDataControlOfferV1,
        event: ext_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_offer::Event::Offer { mime_type } = event {
            state.on_offer_mime(off.id(), mime_type);
        }
    }
}
impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        off: &ZwlrDataControlOfferV1,
        event: wlr_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wlr_offer::Event::Offer { mime_type } = event {
            state.on_offer_mime(off.id(), mime_type);
        }
    }
}

// ─── dispatch: source (ext + wlr) ───────────────────────────────────────────────
impl Dispatch<ExtDataControlSourceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        src: &ExtDataControlSourceV1,
        event: ext_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_source::Event::Send { fd, .. } => state.on_source_send(fd),
            ext_source::Event::Cancelled => state.on_source_cancelled(Source::Ext(src.clone())),
            _ => {}
        }
    }
}
impl Dispatch<ZwlrDataControlSourceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        src: &ZwlrDataControlSourceV1,
        event: wlr_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wlr_source::Event::Send { fd, .. } => state.on_source_send(fd),
            wlr_source::Event::Cancelled => state.on_source_cancelled(Source::Wlr(src.clone())),
            _ => {}
        }
    }
}
