# Contributing to CleanGroove

Thank you for helping. Bug reports, descriptions of records CleanGroove handles badly,
documentation fixes and code are all welcome.

## Ground rules

**No further analysis of the originals.** CleanGroove reimplements ClickRepair's methods from
functional specifications produced during development research, which included decompiling
those programs. That research is finished and is not part of this project. Do not decompile, disassemble or copy from ClickRepair, ClickRepairRT or any other
proprietary program, and do not contribute anything derived from doing so. Work from
CleanGroove's own code and documentation, published research, and your own measurements.

**No copyrighted audio.** Never commit or attach commercial recordings, even short clips.
Describe the problem (record, time, what you hear) instead. Test signals you generate, and
recordings you made and own the rights to, are fine. The repository ignores audio files by
default.

**The engines' output is locked.** The offline and real-time declickers are fixed to the
behaviour of their specifications. A change that alters their output, however small, needs
discussion in an issue first. A DSP bug fix starts with a failing test that reproduces it,
then the fix. Regression tests for past fixes are in
`crates/cr-dsp/tests/audit_regressions.rs`, and `crates/cr-cli/tests/golden_classic.rs`
pins the Classic engine's exact output.

## Building and testing

Rust 1.80 or newer. On Linux, first:

    sudo apt install pkg-config libasound2-dev libgtk-3-dev

Build and test with optimisations; the DSP is far too slow without them:

    cargo build --release
    cargo test --workspace --release --no-fail-fast

`test_live_stream_monitor` is ignored by design: it needs real audio hardware. The
`test_screenshot_*` GUI tests open a hidden window and run on Windows only.

The code lives in crates named `cr-*` (`crates/cr-gui`, `crates/cr-cli`, `crates/cr-rt`), and
`cargo -p` uses those names. They build the programs `cleangroove`, `cleangroove-cli` and
`cleangroove-rt`.

## Your own regression clips

To check how a DSP change sounds and measures, build a small gold set from your own records:
short clips chosen for specific problems (quiet intros, brass, sibilance, scratches, heavy
damage). Keep them in a local folder such as `scratch/audio`, which is ignored by git. They stay
on your machine.

    cargo run --release -p cr-cli --example goldset -- scratch/audio --save my_baseline.tsv
    cargo run --release -p cr-cli --example goldset -- scratch/audio --baseline my_baseline.tsv

The second command flags every clip whose output changed since you saved the baseline.

## Code

- Keep changes small and focused, one topic per pull request.
- Match the surrounding code: naming, comment density, idiom.
- Comments explain why, in plain words. No LaTeX in comments or docs.
- Internal samples are 25-bit signed integers in `i32` (24-bit PCM shifted left by one).
  Convert with the helpers in `cr_dsp::params` (`scale_*_to_internal`, `internal_to_*`),
  never with ad-hoc shifts.
- Corrections (`cr_dsp::post`) always run after declicking, never before.
- When you change menus, shortcuts or workflows, update `docs/USER_GUIDE.md` (shown in the
  app) and `docs/MANUAL.md`.

## Pull requests

- Describe what changed and why, and how you tested it (which tests, which hardware).
- For GUI changes, a screenshot helps.
- For anything touching the declickers, say whether their output changed.

## License of contributions

Unless you say otherwise, any contribution you submit for inclusion in CleanGroove is licensed
under the MIT license and the Apache License 2.0, as the rest of the project, without any
additional terms.
