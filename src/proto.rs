pub const MSG_VIDEO_FRAME: u8 = 0x01;
pub const MSG_AUDIO_FRAME: u8 = 0x02;
pub const MSG_CONTROL: u8 = 0x03;
pub const MSG_CLIENT_MSG: u8 = 0x10;

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