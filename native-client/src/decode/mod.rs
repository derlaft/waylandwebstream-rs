// Video decoding. Phase 5 ships only the software H.264 -> ARGB8888
// path; the GPU path (VAAPI) lands in Phase 10 alongside the
// equivalent on the encoder side.

pub mod sw;
