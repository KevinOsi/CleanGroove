# The offline restoration engine

How CleanGroove's File Mode engine (`crates/cr-dsp`) finds and repairs clicks and crackle. This
describes CleanGroove's own implementation; the code is the reference, and section numbers here
are what code comments point to (for example "engine doc 5.2").

CleanGroove reimplements the methods of Brian Davies' ClickRepair (see the README's
Provenance section). The Classic profile described here is pinned to exact output by
`crates/cr-cli/tests/golden_classic.rs`.

Formulas are written in plain text: `x^2` is x squared, `sum(...)` a sum, `|x|` an absolute
value.

---

## 1. Samples and signal domain

Audio is processed as signed integers in a 25-bit range held in `i32`: 24-bit PCM shifted left
by one, so full scale is 2^24 = 16,777,216. 16-bit input is shifted left by 9, 32-bit input
right by 7, float input multiplied by 2^24 and rounded half away from zero. Output reverses
this, clamping to the 24-bit range. The helpers are in `params.rs` (`scale_*_to_internal`,
`internal_to_*`).

Each channel is processed independently by a `Channel` (`channel.rs`), in Stereo mode one per
side. Internally a channel keeps power-of-two ring buffers of the current samples, the
original samples, per-sample flags (section 11), and prediction errors (section 4).

## 2. From settings to detection parameters

The user settings are DeClick `dcl` and DeCrackle `dcr` (0 to 100, 0 = off), the method, the
channel mode, pitch protection and reverse scanning (`params.rs`).

**Threshold.** With `u = 100 - setting`:

    T(setting) = 2.8 + 0.012 u + 2.5e-4 u^2 + 3.5e-8 u^4

T falls from 10.0 at setting 0 to 2.8 at 100: a higher setting means a lower threshold
and more repairs. (Examples: T(30) = 5.705, T(50) = 4.244, T(100) = 2.8.)

**Windows.** The detector compares a short *peak window* of `peak_len` samples with *shoulder*
windows of `shoulder_len` samples on either side:

| Method | peak_len | shoulder_len |
|---|---|---|
| Wavelet | 4 + dcl / 25 | 80 + dcl / 5 |
| Simple | 6 + dcl / 15 | 100 + dcl / 3 |

(integer division). Reverse scanning adds 1 to `peak_len`.

**Factors.** From the threshold `T = T(dcl)`:

    onset_factor = T / shoulder_len
    detect_factor = onset_factor^2 * peak_len

A second, *lower* set uses `T - 0.25 * n` where n is the number of subchannels (section 8),
for samples a subchannel has marked.

**DeCrackle.** `lambda` = 0.996 (Wavelet) or 0.998 (Simple), and

    crackle_factor = 4 * (T(dcr) * (1 - lambda))^2

## 3. Signal path

`Processor` (`process.rs`) handles sample rates, channel modes and alignment.

**Sample rates.** The channels work at 48 kHz or below:

- Above 96 kHz, each pair of samples is first split into a half-rate sum and difference
  (`(a + b) / 2`, `(a - b) / 2`); the difference band is kept aside and recombined on output,
  set to zero over repaired spans (it is not reconstructed: a known limitation).
- Above 48 kHz (after that), a D4 wavelet split (section 4.1) halves the rate again; the high
  band is kept aside. When a channel repairs a span, the matching high-band span is replaced
  by an AR extrapolation of the high band (orders 50 to 512), so the repair reaches the full
  bandwidth.

**Channel modes.**

- *Stereo*: left and right each go through their own channel, in parallel.
- *Mono*: one channel; the output is copied to both sides.
- *Merge* (mono records captured in stereo): left + right is declicked as one signal. The
  left and right levels are tracked over a 4000-sample window centred on each sample, and
  the change the declicker made is shared between the sides in proportion to their levels:
  `out_left = left + w * delta`, `out_right = right + (1 - w) * delta`, with
  `w = level_left / (level_left + level_right)`.

**Alignment.** A file's length is trimmed to a multiple of the processing block (rate splits
and wavelet levels); the few samples left over at the end are passed through unchanged.
`Processor::process_slice_aligned` returns output aligned sample for sample with the input.

**Latency.** Each channel looks ahead far enough for detection and repair (section 5), plus
192 samples with DeCrackle. With Wavelet x2 / x3, Classic adds the subchannel delay wrapped
to a signed 8-bit value (`compat_byte_delay`, kept for output compatibility); switching it off
adds four times the subchannel delay instead.

### 3.1 The wavelet split

In the Wavelet methods each channel splits its input into a low and a high band at half the
rate, with the 4-tap Daubechies D4 filter pair (`wavelet.rs`). The low-pass taps are

    h = ((1 - sqrt 3), (3 - sqrt 3), (3 + sqrt 3), (1 + sqrt 3)) / (4 sqrt 2)

and the high-pass taps are the same reversed with alternating signs. Analysis rounds half
away from zero, and synthesis compensates for integer truncation. Both bands are scanned
(section 5), a repair is made in both together, and the output is synthesised from the two
bands.

## 4. Prediction error

Clicks are found in the *prediction error* (excitation) of a short autoregressive (AR) model,
not in the audio itself: music is predictable from its recent past, a click is not.

Every 256 samples, for each 256-sample block (`burg.rs`, `update_excitation`):

1. Fit an order-10 AR model with Burg's method to 512 samples: the block plus 128 samples on
   each side.
2. Store `|forward prediction error|` and `|backward prediction error|` for each of the
   block's samples, and keep the 10 reflection coefficients (DeCrackle uses them).

Burg's method chooses each reflection coefficient to minimise the sum of forward and
backward prediction errors: `k_m = 2 sum(f * b) / sum(f^2 + b^2)`. The Levinson-Durbin step-up
recursion turns reflection coefficients into predictor coefficients (`reflection_to_predictor`).

After every repair the excitation over the repaired span is recomputed, so the detector sees
the repaired audio.

## 5. Click detection

Every 128 samples the channel scans the stretch of audio that has newly entered its
look-ahead (`scan_forward`, or `scan_backward` in reverse). The scan moves
one sample at a time through these windows, kept as running sums:

    pre-shoulder  [pre_start, peak_start)   shoulder_len samples  sum of forward error
    peak          [peak_start, peak_end)    peak_len samples      sum of squared error
    post-shoulder [peak_end, post_end)      shoulder_len samples  sum of backward error

Scanning forward the peak window ends at the scan position; scanning in reverse it starts
there, and the roles of forward and backward error swap.

**Detection.** A click starts where

    peak > detect_factor * pre^2

that is, where the error in the peak window stands out from the average error before it (the
post-shoulder in reverse). The detector then searches up to `peak_len` samples either side for the
first sample whose error exceeds `onset_factor * pre`: the onset.

**End.** The click ends when the condition clears, or after 256 samples (the click window).
Its end is refined by searching back from the end for the last sample whose backward error
exceeds `onset_factor * post`.

**Both bands.** In Wavelet mode each band has its own click state. When both bands are idle
again, each band's click goes to edge location (section 6), and the two bands' repair spans
are unified so that both cover the same span of output samples.

**Rescans.** After a repair, the scan backs up to rescan from just before it (up to the
look-ahead limit), so a click next to a repair is seen against repaired audio. A repair is
made only if its span contains samples not already repaired, and never within 128 samples
(64 in Wavelet mode) of the file's start or end.

## 6. Click edges and the false-repair gate

`find_defect_edges` (`detector.rs`) turns the detected span `[span_start, span_end)` into
the exact samples to replace:

1. Widen the search by a margin of a third of the span, kept between `peak_len` and `3 peak_len`.
2. Fit an AR model to the audio before the span (order = span width, kept between 20 and 512;
   window 2 * order + 256 samples). Step forward with one-step predictions; the first bad
   sample is where the prediction error crosses `T * rms`, with a two-sample test
   (`|e[i-1]| + |e[i]| > 1.7 T rms`) so a single noisy sample does not decide it. A look-ahead
   of 1 to 4 samples (by span width) keeps the model out of the click.
3. Do the same backwards from the audio after the span for the first good sample.
   (Reverse scanning finds the end first, then the start.)
4. **False-repair gate.** Build the repair (section 7). If no repaired sample differs from
   the original by more than `T * rms`, the "click" was within the music's own prediction
   error, and no repair is made.

Repairs longer than the automatic limit (128 samples in Wavelet mode, 256 in Simple) are left
alone and counted.

A new repair within one or two samples of an existing one is merged with it, and the merged
span is rebuilt. Where a subchannel has marked the area, earlier repairs up to one span-width
back are merged too.

## 7. Repair

`fill_gap_bounded` (`reconstruct.rs`) fills the span `[start, end)` of length `n`:

1. AR order `p = 2n`, kept between 50 and 512. Fit one AR model to the `2p + 256` samples before
   the span and another to the same number after it.
2. **Forward pass.** Predict the span sample by sample from the samples before it. Each
   prediction keeps a little of the original sample's deviation, softly limited:

       out = prediction + residual / (1 + r^8),   r = d * residual / rms

   with `d = 0.7 + (n / 125)^2`. Small deviations (part of the music) pass; large ones (the
   click) are removed.
3. **Backward pass.** The same from the samples after the span, running backwards.
4. **Crossfade.** Combine the two with weights that favour the nearer side, a piecewise
   quadratic: with `t = (k + 1) / (n + 1)`, the forward weight is `1 - 2 t^2` for `t < 0.5`,
   and `2 (1 - t)^2` after.

The result is rounded half away from zero and written over the span, and the span's samples
are flagged as repaired.

## 8. Wavelet x2 and x3: subchannels

The x2 / x3 methods add one or two *subchannels* (`subchannel.rs`) that look for longer
clicks at lower rates. A subchannel never changes the output: it marks where it found
something, and the main channel then detects there with the lower threshold (section 2).

- **Input.** The average of every 4 original samples (and of both bands in Wavelet mode); x3
  chains a second subchannel at a sixteenth of the rate. Each subchannel also runs a
  second-order Butterworth low-pass over its input.
- **Detection.** As section 5, with `peak_len` = 3 and `shoulder_len` = 50 (35 in reverse), its own
  excitation (order 10, with 1 added to every error as a floor), and a repair limit of
  64 samples at the first level and 16 at the next.
- **Repair.** Up to 64 samples: the exact least-squares AR interpolation. Fit AR models
  (order `2n`, 8 to 128) before and after the gap and average their reflection coefficients
  into one model, then solve for
  the gap samples that minimise the total prediction error across the gap and `p` samples
  either side. The system matrix is the autocorrelation of the prediction-error filter, a
  symmetric Toeplitz matrix, solved by Cholesky factorisation. Longer gaps fall back to
  section 7.
- **Low-frequency gate.** The repair counts only if, over 64 samples around it, the
  subchannel's signal is genuinely low-frequency (its variance more than 10 times the
  difference from its low-passed version) and the repair changed some sample by more than
  the lower threshold times that difference. Then the matching span in the parent, four times
  as long and widened by 8 samples, is marked.

## 9. DeCrackle

DeCrackle (`kalman.rs`) is a fixed-lag Kalman smoother over a 64-sample state, run on each
band, using the order-10 AR model from section 4 (reloaded every 256 samples).

- A running level of the prediction error is kept: `average = lambda * average + |error|`.
- A sample is crackle when its local error energy
  `e[i-1]^2 + 2 e[i]^2 + e[i+1]^2` exceeds `crackle_factor * average^2`.
- For crackle samples the smoother treats the observation as missing and predicts it from the
  AR model; for the next samples it updates the state and covariance from the observations.
  After 11 steps without crackle the smoothing window closes.
- The smoothed value is written 64 samples later, flagged as DeCrackle, and the excitation
  there is recomputed for the click detector.

## 10. Pitch protection

Strongly pitched sounds (brass, reeds, voices) have sharp, periodic edges that can look like
clicks. Pitch protection (`pitch.rs`) undoes repairs that recur at the music's period.

When there are more than four repairs waiting in the look-ahead:

1. Fit an order-256 AR model to 768 samples of the *original* audio at the repair. The lag
   (32 to 255) with the largest reflection coefficient is taken as the period.
2. Repairs much wider than a pulse (more than `min(period / 3, period / 6 + 10)` samples) are
   left alone.
3. Look for repairs at one period before and after, up to 11 periods each way, tracking a
   drifting period by one sample per step and allowing one miss.
4. If at least five repairs line up, restore the original audio over all of them, flag them as
   protected, and recompute the excitation.

## 11. Flags

Each sample has a byte of marks (`flag.rs`):

| Mark | Meaning |
|---|---|
| `REPAIRED` | repaired by DeClick (low band in Wavelet mode) |
| `REPAIRED_HIGH` | repaired by DeClick, high band |
| `DECRACKLED` | changed by DeCrackle |
| `PROTECTED` | repair undone by pitch protection |
| `HINT` | marked by a subchannel: detect with the lower threshold |
| `SUB_REPAIRED` | repaired inside a subchannel |

The live view's repair pins and held-back pins read `REPAIRED` and `PROTECTED`.

## 12. Presets

| Preset | DeClick | DeCrackle | Pitch | Reverse | Method | Mode |
|---|---|---|---|---|---|---|
| Stereo LP | 50 | 0 | off | off | Wavelet | Stereo |
| Stereo brass | 50 | 0 | on | on | Wavelet | Stereo |
| Stereo percussion | 20 | 0 | off | on | Simple | Stereo |
| Mono LP | 50 | 0 | off | off | Wavelet | Merge |
| 78 RPM shellac | 50 | 50 | off | off | Wavelet | Merge |

## 13. Engine profiles

Classic is the behaviour described above. Enhanced extends it for
sustained defects (gouges of 4 to 20 ms): when a click ends but the error stays high, the
whole stretch is analysed with a longer repair limit and repaired only if it stands clear of
the music on both sides. It is available in the measuring tools, not yet in the app.

## 14. Deliberate corrections

Where the reference behaviour was internally inconsistent, CleanGroove follows the mathematics;
each such place is commented `Audit fix:` in the code and has a regression test in
`crates/cr-dsp/tests/audit_regressions.rs`. Examples: the reverse scanner's running window
sums, the Merge mode level window (centred, and its starting value sliding out), the
least-squares interpolation using the samples on both sides of the gap, and the sample format
conversions.
