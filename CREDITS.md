# Credits

## ClickRepair and ClickRepairRT

CleanGroove is an independent reimplementation of **ClickRepair** and
**ClickRepairRT**, the click and crackle removal programs written by **Brian Davies**. The
restoration methods at the heart of CleanGroove (how clicks and crackle are detected, how
damaged samples are replaced, how pitched music is protected, and how the real-time
declicker works) are his work. CleanGroove would not exist without them.

CleanGroove's code was written from functional specifications produced by analysing ClickRepair
4.0.7 (build 46j) and ClickRepairRT 2.0.5 (build 5b); it contains no ClickRepair source code, decompiled code or bytecode. See
[Provenance](README.md#provenance-how-cleangroove-was-made) in the README.

CleanGroove is not affiliated with, endorsed by or supported by Brian Davies or any holder of
rights in ClickRepair. Please do not contact them about CleanGroove.

## Background reading

The methods CleanGroove implements draw on well-established signal processing. These references
are listed for readers who want the mathematics; CleanGroove's code was written from the
specifications above, not from these works.

- J. P. Burg, *Maximum Entropy Spectral Analysis*, PhD thesis, Stanford University, 1975.
  Burg's method for fitting autoregressive models.
- I. Daubechies, "Orthonormal bases of compactly supported wavelets", *Communications on
  Pure and Applied Mathematics* 41 (7), 1988. The D4 wavelet used to split the signal into
  bands.
- A. J. E. M. Janssen, R. N. J. Veldhuis and L. B. Vries, "Adaptive interpolation of
  discrete-time signals that can be modeled as autoregressive processes", *IEEE
  Transactions on Acoustics, Speech, and Signal Processing* 34 (2), 1986. Autoregressive
  interpolation of missing samples.
- R. E. Kalman, "A new approach to linear filtering and prediction problems", *Journal of
  Basic Engineering* 82 (1), 1960. The Kalman filter.
- S. J. Godsill and P. J. W. Rayner, *Digital Audio Restoration: A Statistical Model Based
  Approach*, Springer, 1998. A survey of click and noise removal.

## Libraries

CleanGroove is built on these open-source Rust libraries (direct dependencies; each has its own
dependencies). Their licenses apply to them.

| Library | Used for | License |
|---|---|---|
| [egui / eframe](https://github.com/emilk/egui) | the GUI | MIT OR Apache-2.0 |
| [cpal](https://github.com/rustaudio/cpal) | audio input and output | Apache-2.0 |
| [hound](https://github.com/ruuda/hound) | writing WAV | Apache-2.0 |
| [Symphonia](https://github.com/pdeljanov/Symphonia) | reading WAV, FLAC and AIFF | MPL-2.0 |
| [rayon](https://github.com/rayon-rs/rayon) | processing channels in parallel | MIT OR Apache-2.0 |
| [rtrb](https://github.com/mgeier/rtrb) | lock-free audio queues | MIT OR Apache-2.0 |
| [crossbeam-channel](https://github.com/crossbeam-rs/crossbeam) | messages between threads | MIT OR Apache-2.0 |
| [rfd](https://github.com/PolyMeilex/rfd) | file and folder dialogs | MIT |
| [image](https://github.com/image-rs/image) | the app icon | MIT OR Apache-2.0 |
| [clap](https://github.com/clap-rs/clap) | command-line options | MIT OR Apache-2.0 |
| [indicatif](https://github.com/console-rs/indicatif) | command-line progress bars | MIT |
| [ctrlc](https://github.com/Detegr/rust-ctrlc) | Ctrl+C handling | MIT OR Apache-2.0 |
| [serde](https://github.com/serde-rs/serde) / [serde_json](https://github.com/serde-rs/json) | recording sidecar files | MIT OR Apache-2.0 |
| [anyhow](https://github.com/dtolnay/anyhow), [thiserror](https://github.com/dtolnay/thiserror) | error handling | MIT OR Apache-2.0 |
| [tracing](https://github.com/tokio-rs/tracing) | logging | MIT |
| [libc](https://github.com/rust-lang/libc) | quieting ALSA's device probe on Linux | MIT OR Apache-2.0 |
| [ureq](https://github.com/algesten/ureq) | Discogs, MusicBrainz and cover art requests | MIT OR Apache-2.0 |
| [metaflac](https://github.com/jameshurst/rust-metaflac) | FLAC tags and embedded covers | MIT |
| [md-5](https://github.com/RustCrypto/hashes) | the audio checksum in exported FLAC files | MIT OR Apache-2.0 |
| [keyring](https://github.com/hwchen/keyring-rs) | keeping the Discogs token in the system keychain | MIT OR Apache-2.0 |
| [arboard](https://github.com/1Password/arboard) | pasting cover images | MIT OR Apache-2.0 |
| [raw-window-handle](https://github.com/rust-windowing/raw-window-handle) | the cover drop target | MIT OR Apache-2.0 OR Zlib |

Symphonia is licensed under the Mozilla Public License 2.0: its source is available from the
link above, and it is used unmodified.

## CleanGroove

Written by the CleanGroove contributors.
