pub mod keyboard;
pub mod mouse;
pub mod touch;

/// Clamp a normalized browser coordinate into the documented `[0.0, 1.0]`
/// range. Browser input is untrusted: a buggy or malicious client can send
/// out-of-range or non-finite values, which would otherwise map to off-screen
/// (or NaN) compositor coordinates when scaled. Non-finite inputs collapse to
/// `0.0`.
pub(crate) fn normalize_unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}
