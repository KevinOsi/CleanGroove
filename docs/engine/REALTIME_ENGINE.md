# The real-time declicking engine

How CleanGroove's live declicker (`crates/cr-dsp/src/rt`) works. It reimplements the methods of
Brian Davies' ClickRepairRT (see the README's Provenance section). It reuses the offline
engine's building blocks, so read [the offline engine](OFFLINE_ENGINE.md) first; this page
covers what is different. The code is the reference.

The host around it (devices, queues, recorder, meters) lives in
`crates/cr-rt`.

---

## 1. What changes for live audio

The real-time engine trades reach for bounded work and a fixed delay:

- **No DeCrackle**, no subchannels (Wavelet x2 / x3), no reverse option.
- **Backward scanning only.** Each scan runs from the newest audio back into the past, so a
  click is found as soon as enough audio after it has arrived.
- **Short repairs.** At most 128 samples (about 2.9 ms at 44.1 kHz); AR models of order up
  to 256 on windows of up to 768 samples.
- **Fixed latency** (section 4) and bounded work per block.
- **Untouched audio passes through bit for bit.** Repairs are spliced into the stream;
  everything else is the input, unchanged, even in Wavelet mode.

## 2. Settings

**Sensitivity.** `RtSensitivity` is either the classic stepped slider 0 to 9, mapped to

    s = 11 * step - 15        (step 5 gives 40, step 9 gives 84)

or a fine value 0 to 100 used as is (the GUI uses this, from the DeClick slider). 0 is off.

**Windows and factors** (`RtParameters::compute_derived`), with T from the offline engine's
threshold polynomial:

| Method | peak_len | shoulder_len |
|---|---|---|
| Wavelet | 5 + s / 25 | 80 + s / 5 |
| Simple | 7 + s / 15 | 100 + s / 3 |

    onset_factor = T(s) / shoulder_len
    detect_factor = onset_factor^2 * peak_len

The window is one sample wider than the offline forward scan's because the scan always runs
backward (as the offline reverse scan does).

**Changing settings while running.** Sensitivity, pitch protection and the monitor mode take
effect at once (`set_parameters`, `set_mode`). The method changes the block length and the
latency, so the host starts a new processor for it (the monitor goes quiet for one latency
period; recording is separate and carries on).

## 3. Structure

`RtProcessor` (`process.rs`) keeps a 32768-sample ring buffer per side (the *super buffer*)
holding the current samples, the original samples and the flags. Two `RtChannel`s (left and
right, run in parallel) each keep 4096-sample buffers of their own and process the audio in
blocks:

1. Every block (`process_block_len` = 1024 samples in Wavelet mode, 512 in Simple), each
   channel reads the new samples from the super buffer. In Wavelet mode it splits them into
   low and high bands with the D4 wavelet (offline engine 3.1), so each band runs at half rate.
2. Every 256 samples it computes the order-10 prediction error (offline engine 4).
3. At the end of the block it runs one backward scan (section 5).
4. As samples leave the channel, it exports repairs into the super buffer (section 6).

The processor emits one output frame for every input frame, zeros during the start-up delay,
so audio callbacks keep an exact one-in, one-out cadence.

Every 131072 frames all position counters are moved back together, so the engine can run
indefinitely without counter overflow.

## 4. Latency

    filter_len = 8192 (Wavelet) or 4096 (Simple)
    proc_delay = filter_len + 2 * process_block_len

so 10240 samples in Wavelet mode (232 ms at 44.1 kHz, 213 ms at 48 kHz) and 5120 in Simple
(116 / 107 ms). At 88.2 / 96 kHz each processor frame is two input frames, so the delay in
samples doubles and the time stays the same; likewise again above 96 kHz. The live host adds
its output cushion on top.

## 5. The backward scan

`scan_for_clicks` (`channel.rs`) scans from 512 samples behind the newest sample back to 1408
(further, up to 2048, to rescan after a recent repair; never into the first 128 samples, 64
in Wavelet mode). It uses the offline engine's windows, mirrored for the backward direction:

    pre-shoulder  [pre_start, peak_start)   shoulder_len samples
    peak          [peak_start, peak_end)    peak_len samples
    post-shoulder [peak_end, post_end)      shoulder_len samples

with `peak_start` at the scan position. A click starts where `peak > detect_factor * post^2`
(the audio after it, already seen, is the reference). Its right edge is found by searching
for the first sample above `onset_factor * post`, and the click ends when the condition
clears or after 256 samples. In Wavelet mode each band has its own state; when both are
idle, each band's click is located, the two spans are unified, and the repair is made.

**Edges and the false-repair gate** work as offline (offline engine 6), with the real-time
limits: AR order by span width up to 256, windows up to 768, repairs up to 128 samples. The
repair itself is offline engine 7 with the same limits.

## 6. Splicing repairs into the output

The channel never rewrites the stream wholesale. As samples leave its buffer:

- A run of repaired samples (`REPAIRED`) is collected: in Simple mode the repaired samples, in
  Wavelet mode the two bands re-synthesised into output samples.
- When the run ends, it is written over the same span of the super buffer, and those samples
  are marked `REPAIRED` there.
- A run longer than 128 samples is cancelled: the audio passes through unrepaired.
- Samples whose repair pitch protection undid are marked `PROTECTED` in the super buffer.

Everything else in the super buffer is the original input, so audio without clicks comes out
exactly as it went in.

## 7. Monitor modes

The processor reads output from the super buffer `proc_delay` frames behind the input, in one
of three modes (`RtSoundMode`):

- **Sound Out**: the repaired samples.
- **Sound In**: the original samples, delayed by the same amount, so switching is seamless.
- **Noise**: repaired minus original: only what was removed.

## 8. High sample rates

- **88.2 / 96 kHz**: the processor splits each pair of input samples with the D4 wavelet. The
  channels work on the low band at half rate; the high band is kept in the processor. When a
  channel exports a repair, the matching high-band span is rebuilt by AR extrapolation
  (section 5's repair) and the output is re-synthesised from both bands. In Noise mode the
  two bands' differences are synthesised, which by linearity gives the removed signal.
- **176.4 / 192 kHz**: before that, a sum-and-difference split halves the rate once more; the
  difference band is kept aside and set to zero over repaired spans (a known limitation, as
  offline).

## 9. Pitch protection

As offline (offline engine 10), with two changes for live use (`pitch.rs`):

- **The period is kept between checks.** It is measured (order-256 Burg on 768 original
  samples, the strongest reflection coefficient from `rate / 2940` in Wavelet mode or
  `rate / 1470` in Simple, up to lag 255) only when there is none. While at least five repairs
  keep lining up with it, it is kept, following the drift; after a check where they do not,
  it is dropped and measured again next time.
- **Significance gate**: a newly measured period is used only if its coefficient stands out:
  `max_k * period^0.33 >= 13 * rms_k`, where `rms_k` is the RMS of all 256 coefficients.

It runs when more than four repairs are waiting; when five line up at the period, their
repairs are undone as offline.

## 10. Telemetry

After each block the processor publishes `RtTelemetry`:

- the input peak per side (1.0 = full scale);
- the number of repaired samples in the window still ahead of the output, per side;
- a click-activity value from 0 to 1:

      density  = (repaired L + R) / (32768 - proc_delay)
      activity = 0.16 * (ln(density + 0.0001) + 8)      clamped to 0..1

  and 0 when the input is nearly silent.

The live view's repair pins come from the flags in section 6, read by the host
(`cr_rt::live::waveform`).
