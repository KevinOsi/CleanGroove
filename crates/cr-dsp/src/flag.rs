//! Per-sample marks the engines keep alongside the audio (one byte per sample).
//!
//! See docs/engine/OFFLINE_ENGINE.md, section 11.

/// Replaced by a DeClick repair (in Wavelet mode: the low band).
pub const REPAIRED: u8 = 1 << 0;
/// Replaced by a DeClick repair, high band (Wavelet mode sets it together with `REPAIRED`).
pub const REPAIRED_HIGH: u8 = 1 << 1;
/// Changed by DeCrackle.
pub const DECRACKLED: u8 = 1 << 2;
/// A repair here was undone by pitch protection.
pub const PROTECTED: u8 = 1 << 3;
/// A subchannel found something here: detect with the lower threshold.
pub const HINT: u8 = 1 << 4;
/// Repaired inside a subchannel (its own buffer, not the output).
pub const SUB_REPAIRED: u8 = 1 << 5;

/// Changed by either restoration stage.
pub const CHANGED: u8 = REPAIRED | DECRACKLED;
/// Changed by either restoration stage, in either band.
pub const ANY_CHANGE: u8 = REPAIRED | REPAIRED_HIGH | DECRACKLED;
/// What pitch protection looks for offline: changes, and repairs it has already undone.
pub const PITCH_CANDIDATE: u8 = REPAIRED | DECRACKLED | PROTECTED;
/// What pitch protection looks for in the real-time engine (no DeCrackle there).
pub const REPAIRED_OR_PROTECTED: u8 = REPAIRED | PROTECTED;
/// Cleared when pitch protection restores a sample.
pub const UNDONE_BY_PROTECTION: u8 = REPAIRED | DECRACKLED | HINT;

/// The repair marks of a channel: both bands in Wavelet mode.
#[inline]
pub fn repaired_mask(is_wavelet: bool) -> u8 {
    if is_wavelet {
        REPAIRED | REPAIRED_HIGH
    } else {
        REPAIRED
    }
}
