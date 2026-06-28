pub const MSG_VIDEO_FRAME: u8 = 0x01;
pub const MSG_AUDIO_FRAME: u8 = 0x02;
pub const MSG_CONTROL: u8 = 0x03;
/// Server->client clipboard image (remote selection -> device). Binary, since
/// base64-in-JSON would bloat multi-MB images. Payload: `mime_len` (u16 LE),
/// `mime` (utf8), then the raw image bytes. Text clipboard stays in
/// `MSG_CONTROL` JSON; see src/clipboard.rs.
pub const MSG_CLIPBOARD_IMAGE: u8 = 0x04;
pub const MSG_CLIENT_MSG: u8 = 0x10;
/// Client->server clipboard image (device -> remote selection). Same payload
/// layout as `MSG_CLIPBOARD_IMAGE`.
pub const MSG_CLIENT_CLIPBOARD_IMAGE: u8 = 0x11;

pub const FLAG_KEYFRAME: u8 = 0b0000_0001;
pub const FLAG_HAS_PING: u8 = 0b0000_0010;

pub const HEADER_LEN: usize = 8;

pub fn encode_msg(msg_type: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.push(msg_type);
    buf.push(flags);
    buf.push(0);
    buf.push(0);
    let len = payload.len() as u32;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

pub fn decode_header(h: &[u8; 8]) -> (u8, u8, u32) {
    let payload_len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
    (h[0], h[1], payload_len)
}

/// Builds a clipboard-image payload (the bytes after the 8-byte header):
/// `mime_len` (u16 LE), `mime` (utf8), then the raw image bytes. Shared by
/// `MSG_CLIPBOARD_IMAGE` and `MSG_CLIENT_CLIPBOARD_IMAGE`.
pub fn encode_clipboard_image_payload(mime: &str, image: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(2 + mime.len() + image.len());
    p.extend_from_slice(&(mime.len() as u16).to_le_bytes());
    p.extend_from_slice(mime.as_bytes());
    p.extend_from_slice(image);
    p
}

/// Parses a clipboard-image payload into `(mime, image_bytes)`. Returns `None`
/// if the payload is truncated or the mime isn't valid UTF-8.
pub fn parse_clipboard_image_payload(payload: &[u8]) -> Option<(String, Vec<u8>)> {
    if payload.len() < 2 {
        return None;
    }
    let mime_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    let rest = &payload[2..];
    if rest.len() < mime_len {
        return None;
    }
    let mime = std::str::from_utf8(&rest[..mime_len]).ok()?.to_string();
    Some((mime, rest[mime_len..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_image_payload_round_trips() {
        let mime = "image/png";
        let image = vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3, 255];
        let payload = encode_clipboard_image_payload(mime, &image);
        let (got_mime, got_image) = parse_clipboard_image_payload(&payload).unwrap();
        assert_eq!(got_mime, mime);
        assert_eq!(got_image, image);
    }

    #[test]
    fn clipboard_image_payload_rejects_truncated() {
        assert!(parse_clipboard_image_payload(&[]).is_none());
        assert!(parse_clipboard_image_payload(&[1]).is_none());
        // mime_len says 9 but no bytes follow.
        assert!(parse_clipboard_image_payload(&[9, 0]).is_none());
    }
}