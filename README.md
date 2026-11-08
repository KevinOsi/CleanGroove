<p align="center">
  <img src="crates/cr-gui/assets/icon_256.png" width="128" alt="CleanGroove icon">
</p>

# CleanGroove

CleanGroove removes clicks and crackle from vinyl and other disc recordings. It restores
recordings you already have (File Mode), records a record while you monitor it through a
real-time declicker (Live Monitor), and splits a whole-album rip into tagged tracks (Tracks).
It runs on Windows and Linux.

CleanGroove is an independent reimplementation of **ClickRepair** and
**ClickRepairRT** by **Brian Davies**, written in Rust, plus tools built around them. It is
not affiliated with or endorsed by the author or rights holders of ClickRepair. See
[Provenance](#provenance-how-cleangroove-was-made) and [Credits](#credits).

> **Status: early release (0.1.0).** The restoration engines are complete and tested; the
> GUI and the live recorder are new and still changing. Settings are not saved between runs
> yet. Expect rough edges, and keep your raw recordings.

## What it does

- **File Mode**: open a WAV, FLAC or AIFF recording, analyse it (click grade, levels, channel
  balance and timing, problem spots, needle thumps), preview the repairs on the waveform and
  by ear, and render a restored copy. The original file is never changed.
- **Live Monitor**: record the raw input from your turntable's USB output or audio interface
  while you hear it through the real-time declicker. Level meters with peak hold and clip
  lights, a scrolling waveform with every repair marked, Rec / Pause / Stop with pause marks
  written as Audacity labels, and a warning if the input is really mono.
- **Tracks**: split an album rip and tag it. Find the record on Discogs or MusicBrainz, get a
  first guess at the splits from the listed track lengths, place the exact points by ear on the
  waveform, and export one tagged FLAC per track with its cover art. Discogs is optional and
  needs your own free token. CleanGroove is not affiliated with Discogs or MusicBrainz.
- **Command line**: `cleangroove-cli` restores files in batch; `cleangroove-rt` monitors and records without the
  GUI.

**Try it in your browser:** https://kevinosi.github.io/CleanGroove/ runs the same engine on your
own computer (nothing is uploaded): live preview, record analysis with suggested settings,
corrections and a 24-bit download. Live Monitor and Tracks are in the desktop app.

## Where it is going

CleanGroove is always working to improve on these methods, and new tools and better results are
the goal. The reimplemented engines stay as they are, as a legacy option for people who know
and trust the original programs: an approximation of how those programs work, kept stable so
that a familiar setting keeps giving the same result. Improvements are added alongside them
rather than changing them underneath you.

## Provenance: how CleanGroove was made

ClickRepair (offline) and ClickRepairRT (real-time) are Brian Davies' click and crackle
removal programs, long recommended among people who digitise records. CleanGroove exists
because of them: the restoration methods are his.

How CleanGroove was made:

1. **Research.** As a development research step, ClickRepair 4.0.7 (build 46j) and
   ClickRepairRT 2.0.5 (build 5b) were analysed, including by decompiling them, and their
   behaviour and algorithms were written down as functional specifications: the detection
   and repair methods, their parameters and the numbers they produce. That research is not
   part of this project: neither the decompiled code nor the specifications are published
   here, and the project does no further analysis of those programs.
2. **Implementation.** CleanGroove's code was written from those specifications. This repository
   contains no ClickRepair source code, decompiled code or bytecode.
3. **Verification.** Where a specification turned out to be wrong (its own numbers
   contradicting it), the code follows the mathematics and says so in a comment marked
   `Audit fix:`.
4. **Review.** A separate review compared CleanGroove's code with the original programs for
   resemblance of expression (structure and names), and the code was rewritten in CleanGroove's
   own terms wherever it found any.

CleanGroove uses the same methods, but it does not claim to produce exactly the same output as
ClickRepair: the code differs in places, and its results are its own.

The specifications are not part of this repository. CleanGroove's engines are documented in their
own terms, from CleanGroove's code, in [docs/engine](docs/engine/OFFLINE_ENGINE.md).

"ClickRepair" and "ClickRepairRT" are the names of Brian Davies' software. They are used here
only to say what CleanGroove reimplements. If you hold rights to ClickRepair and have a concern
about this project, please contact the project by opening an issue; it will be taken
seriously.

## What is reimplemented, and what is new

| Reimplemented from ClickRepair / ClickRepairRT | New in CleanGroove |
|---|---|
| DeClick: click detection and repair (Simple and Wavelet methods, Wavelet x2 / x3 for longer clicks) | Record analysis: click grade, levels and clipping, channel balance and azimuth, problem spots |
| DeCrackle: crackle smoothing | Suggested settings from the analysis (auto-tune) |
| Pitch protection for brass, reeds and voices; reverse scanning | Corrections after declicking: polarity, balance, azimuth, subsonic filter, elliptical EQ, mono sum, level check |
| Stereo, Merge (mono LP / 78) and Mono channel modes; the presets | Needle-thump finder and repair (once-per-turn thuds) |
| The real-time declicker with Sound Out / Sound In / Noise monitoring | Live Monitor: device selection, raw recorder with pause labels and settings sidecar, level meters, mono-input check, scrolling waveform with repair marks |
| | Tracks: Discogs and MusicBrainz lookup, track splitting, tagged FLAC export with cover art |
| | 176.4 and 192 kHz in the real-time engine |
| | The GUI, the `cleangroove-cli` and `cleangroove-rt` command-line tools, Linux support |

CleanGroove's GUI follows its own design; it does not copy ClickRepair's interface.

## Install

There are no prebuilt binaries yet. Build from source with Rust 1.80 or newer
(https://rustup.rs).

**Windows**

    cargo build --release

**Linux** (Debian / Ubuntu package names)

    sudo apt install pkg-config libasound2-dev libgtk-3-dev
    cargo build --release

macOS is untested.

The programs are in `target/release/`: `cleangroove`, `cleangroove-cli` and `cleangroove-rt`. The DSP is slow
unoptimised, so always build with `--release`.

## Quick start

GUI:

    cleangroove                 # File Mode
    cleangroove --live          # start in Live Monitor
    cleangroove side_a.wav      # open a file

Restore a file from the command line:

    cleangroove-cli --input side_a.wav --analyze-only             # grade it, suggest settings
    cleangroove-cli --input side_a.wav --output side_a_restored.wav --auto
    cleangroove-cli --input side_a.wav --output out.wav --preset stereo-lp --dcl 30

Record from the command line:

    cleangroove-rt --list-devices
    cleangroove-rt --input "USB" --output "Speakers" --sample-rate 48000 --record side_a.wav

## Documentation

- [User manual](docs/MANUAL.md): both modes, every panel and menu, the command-line tools.
- [Quick guide](docs/USER_GUIDE.md): the same guide the app shows under Help > User Guide (F1).
- How the engines work: [offline](docs/engine/OFFLINE_ENGINE.md) and
  [real-time](docs/engine/REALTIME_ENGINE.md).
- [Changelog](CHANGELOG.md) and [contributing](CONTRIBUTING.md).

## Credits

- **Brian Davies**, author of ClickRepair and ClickRepairRT, whose restoration methods CleanGroove
  reimplements.
- The open-source Rust libraries CleanGroove is built on, and the published research behind
  them: see [CREDITS.md](CREDITS.md).

## License

CleanGroove's own code is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. This license covers CleanGroove's code only; it grants nothing in ClickRepair.
Contributions are accepted under the same terms (see [CONTRIBUTING.md](CONTRIBUTING.md)).
