# Changelog

All notable changes to CleanGroove are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [0.1.0] - unreleased

First public release.

### Restoration engines (reimplemented from ClickRepair / ClickRepairRT)

- Offline DeClick (Simple, Wavelet, Wavelet x2 / x3) and DeCrackle, pitch protection,
  reverse scanning, Stereo / Merge / Mono modes and presets.
- Real-time declicker (Simple and Wavelet) with Sound Out / Sound In / Noise monitoring, from
  32 to 192 kHz.

### Built around them (new in CleanGroove)

- Record analysis: click grade, levels and clipping, channel balance and azimuth, problem
  spots, suggested settings.
- Corrections after declicking: polarity, balance, azimuth, subsonic filter, elliptical EQ,
  mono sum, level check with album gain.
- Needle-thump finder and repair.
- GUI with File Mode and Live Monitor sharing one layout; menus with shortcuts; built-in user
  guide; app icon.
- Live Monitor: device selection (with a short, readable device list on Linux), monitoring
  through the real-time declicker, raw recorder with Rec / Pause / Stop, Audacity labels at
  pause joins and a settings sidecar, input and output meters with peak hold and clip
  lights, a mono-input warning, a scrolling waveform with repair marks and a take timeline.
- `cleangroove-cli` for batch restoration and analysis; `cleangroove-rt` for monitoring and recording without
  the GUI.
- Windows and Linux builds.

### Known issues

- Settings are not saved between runs.
- File Mode playback uses the system default output device.
- `cleangroove-rt` cannot switch pitch protection off.
- Opening a large file briefly pauses the window.
- macOS is untested.
