// RTP packetization for H.264
//
// Note: The webrtc-rs crate's TrackLocalStaticRTP handles RTP packetization
// automatically, including fragmentation of H.264 NAL units according to RFC 6184.
// This module can be extended in the future if we need custom RTP handling.
