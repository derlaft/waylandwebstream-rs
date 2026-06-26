// WebSocket transport. Connects to the server's `/client` endpoint and
// exposes the framed binary protocol as `Transport` + `Frame`.
// See docs/native-client-plan.md Part 3.3 (`transport/websocket.rs`).

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream,
};

use super::{Frame, FrameError, Transport};
use crate::proto;
use crate::types::ServerMessage;

pub struct WsTransport {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsTransport {
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _resp) = connect_async(url)
            .await
            .with_context(|| format!("WebSocket connect to {} failed", url))?;
        Ok(Self { ws })
    }
}

impl Transport for WsTransport {
    async fn recv(&mut self) -> Result<Frame> {
        loop {
            let msg = match self.ws.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => return Err(anyhow!(e)),
                None => return Err(anyhow!(FrameError::Closed)),
            };
            // Only binary frames carry framed messages. Text / Ping / Pong /
            // Close from the server are ignored on the receive side -- the
            // server doesn't send any of those today and if it ever did,
            // they'd be control-plane noise that doesn't affect decode.
            if let Message::Binary(data) = msg {
                return parse_frame(&data).map_err(|e| anyhow!(e));
            }
        }
    }

    async fn send(&mut self, json: &str) -> Result<()> {
        let frame = proto::encode_msg(proto::MSG_CLIENT_MSG, 0, json.as_bytes());
        self.ws
            .send(Message::Binary(frame.into()))
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }
}

/// Parses a single framed message. `parse_frame` is the inverse of the
/// server-side `encode_unified_*` helpers and is exposed (not just used
/// internally) so tests can round-trip binary buffers without spinning
/// up a real WebSocket.
pub fn parse_frame(data: &[u8]) -> Result<Frame, FrameError> {
    if data.len() < proto::HEADER_LEN {
        return Err(FrameError::BadHeader(format!(
            "frame too short: {} bytes",
            data.len()
        )));
    }
    let header: [u8; proto::HEADER_LEN] = data[..proto::HEADER_LEN]
        .try_into()
        .expect("length checked above");
    let (msg_type, flags, payload_len) = proto::decode_header(&header);
    let end = proto::HEADER_LEN
        .checked_add(payload_len as usize)
        .ok_or_else(|| FrameError::BadHeader("payload_len overflow".into()))?;
    if data.len() < end {
        return Err(FrameError::BadHeader(format!(
            "payload truncated: header says {} bytes, frame has {}",
            payload_len,
            data.len() - proto::HEADER_LEN
        )));
    }
    let payload = &data[proto::HEADER_LEN..end];

    match msg_type {
        proto::MSG_VIDEO_FRAME => {
            if payload.len() < 20 {
                return Err(FrameError::BadHeader(format!(
                    "video payload too short: {} bytes (need >= 20)",
                    payload.len()
                )));
            }
            let frame_id = u32::from_be_bytes(payload[0..4].try_into().unwrap());
            // `FLAG_HAS_PING` indicates a real ping echo is present; without
            // it the server writes 0.0 in those bytes and we don't need to
            // trust whatever happens to be there. Reading both ways is safe
            // because the layout is fixed, but treating the flag as
            // authoritative matches what the server writes (see
            // `encode_unified_video_frame`).
            let ping_echo = if (flags & proto::FLAG_HAS_PING) != 0 {
                f64::from_be_bytes(payload[4..12].try_into().unwrap())
            } else {
                0.0
            };
            let capture_to_encode_ms =
                f64::from_be_bytes(payload[12..20].try_into().unwrap());
            let h264 = payload[20..].to_vec();
            Ok(Frame::VideoFrame {
                is_keyframe: (flags & proto::FLAG_KEYFRAME) != 0,
                frame_id,
                ping_echo,
                capture_to_encode_ms,
                data: h264,
            })
        }
        proto::MSG_AUDIO_FRAME => {
            if payload.len() < 8 {
                return Err(FrameError::BadHeader(format!(
                    "audio payload too short: {} bytes (need >= 8)",
                    payload.len()
                )));
            }
            let pts_us = u64::from_be_bytes(payload[0..8].try_into().unwrap());
            let opus = payload[8..].to_vec();
            Ok(Frame::AudioFrame { pts_us, data: opus })
        }
        proto::MSG_CONTROL => {
            let msg: ServerMessage = serde_json::from_slice(payload)?;
            Ok(Frame::Control(msg))
        }
        other => Err(FrameError::UnknownType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyframe_video_payload(frame_id: u32, ping: f64, capture_ms: f64, h264: &[u8]) -> Vec<u8> {
        let mut payload = Vec::with_capacity(20 + h264.len());
        payload.extend_from_slice(&frame_id.to_be_bytes());
        payload.extend_from_slice(&ping.to_be_bytes());
        payload.extend_from_slice(&capture_ms.to_be_bytes());
        payload.extend_from_slice(h264);
        payload
    }

    #[test]
    fn parses_keyframe_video() {
        let h264 = [0x67u8, 0x42, 0x00, 0x1e]; // arbitrary SPS-ish bytes
        let payload = keyframe_video_payload(42, 0.0, 1.5, &h264);
        let frame = proto::encode_msg(proto::MSG_VIDEO_FRAME, proto::FLAG_KEYFRAME, &payload);

        let parsed = parse_frame(&frame).expect("valid frame");
        match parsed {
            Frame::VideoFrame {
                is_keyframe,
                frame_id,
                ping_echo,
                capture_to_encode_ms,
                data,
            } => {
                assert!(is_keyframe);
                assert_eq!(frame_id, 42);
                assert_eq!(ping_echo, 0.0);
                assert!((capture_to_encode_ms - 1.5).abs() < 1e-9);
                assert_eq!(data, h264);
            }
            other => panic!("expected VideoFrame, got {:?}", other),
        }
    }

    #[test]
    fn parses_delta_video_with_ping_echo() {
        let h264 = [0x41, 0x9a, 0x24];
        let payload = keyframe_video_payload(7, 12345.678, 0.0, &h264);
        let frame = proto::encode_msg(
            proto::MSG_VIDEO_FRAME,
            proto::FLAG_HAS_PING, // not a keyframe, but has a ping echo
            &payload,
        );

        let parsed = parse_frame(&frame).expect("valid frame");
        match parsed {
            Frame::VideoFrame {
                is_keyframe,
                frame_id,
                ping_echo,
                ..
            } => {
                assert!(!is_keyframe, "keyframe flag must be clear");
                assert_eq!(frame_id, 7);
                assert!((ping_echo - 12345.678).abs() < 1e-9);
            }
            other => panic!("expected VideoFrame, got {:?}", other),
        }
    }

    #[test]
    fn parses_audio_frame() {
        let opus = [0xFCu8, 0xDE, 0xAD];
        let mut payload = Vec::new();
        payload.extend_from_slice(&20_000u64.to_be_bytes());
        payload.extend_from_slice(&opus);
        let frame = proto::encode_msg(proto::MSG_AUDIO_FRAME, 0, &payload);

        let parsed = parse_frame(&frame).expect("valid audio frame");
        match parsed {
            Frame::AudioFrame { pts_us, data } => {
                assert_eq!(pts_us, 20_000);
                assert_eq!(data, opus);
            }
            other => panic!("expected AudioFrame, got {:?}", other),
        }
    }

    #[test]
    fn parses_control_frame() {
        let msg = ServerMessage::Codec {
            codec: "avc1.42E028".into(),
        };
        let json = serde_json::to_vec(&msg).unwrap();
        let frame = proto::encode_msg(proto::MSG_CONTROL, 0, &json);

        let parsed = parse_frame(&frame).expect("valid control frame");
        match parsed {
            Frame::Control(ServerMessage::Codec { codec }) => {
                assert_eq!(codec, "avc1.42E028");
            }
            other => panic!("expected Control(Codec), got {:?}", other),
        }
    }

    #[test]
    fn rejects_truncated_header() {
        assert!(matches!(
            parse_frame(&[0u8; 3]),
            Err(FrameError::BadHeader(_))
        ));
    }

    #[test]
    fn rejects_truncated_payload() {
        // Header claims 100 bytes of payload but frame only carries 4.
        let mut bad = proto::encode_msg(proto::MSG_CLIENT_MSG, 0, &[1, 2, 3, 4]);
        bad[4] = 100;
        bad[5] = 0;
        bad[6] = 0;
        bad[7] = 0;
        assert!(matches!(
            parse_frame(&bad),
            Err(FrameError::BadHeader(_))
        ));
    }

    #[test]
    fn rejects_unknown_type() {
        let frame = proto::encode_msg(0xEE, 0, &[1, 2, 3]);
        assert!(matches!(parse_frame(&frame), Err(FrameError::UnknownType(0xEE))));
    }

    #[test]
    fn rejects_short_video_payload() {
        let payload = vec![0u8; 10]; // < 20 bytes required for video
        let frame = proto::encode_msg(proto::MSG_VIDEO_FRAME, 0, &payload);
        assert!(matches!(
            parse_frame(&frame),
            Err(FrameError::BadHeader(_))
        ));
    }

    #[test]
    fn rejects_short_audio_payload() {
        let payload = vec![0u8; 4]; // < 8 bytes required for audio
        let frame = proto::encode_msg(proto::MSG_AUDIO_FRAME, 0, &payload);
        assert!(matches!(
            parse_frame(&frame),
            Err(FrameError::BadHeader(_))
        ));
    }
}