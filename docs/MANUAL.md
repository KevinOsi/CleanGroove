# CleanGroove User Manual

Version 0.1.0 (early release). This manual covers everything CleanGroove does so far. For a short
walk-through, see the [quick guide](USER_GUIDE.md), which the app also shows under
Help > User Guide (F1).

Contents:

1. [Concepts](#1-concepts)
2. [Installing](#2-installing)
3. [The window](#3-the-window)
4. [File Mode: restoring a recording](#4-file-mode-restoring-a-recording)
5. [Live Monitor: recording a record](#5-live-monitor-recording-a-record)
6. [Menus and shortcuts](#6-menus-and-shortcuts)
7. [Command line: cleangroove-cli](#7-command-line-cleangroove-cli)
8. [Command line: cleangroove-rt](#8-command-line-cleangroove-rt)
9. [Files CleanGroove writes](#9-files-cleangroove-writes)
10. [Troubleshooting](#10-troubleshooting)
11. [Known limitations](#11-known-limitations)
12. [CleanGroove in your browser](#12-cleangroove-in-your-browser)

---

## 1. Concepts

**DeClick** (0 to 100) finds clicks, short spikes from dust and scratches, and replaces the
damaged samples with an estimate from the music on either side (autoregressive
interpolation). Higher values find quieter clicks, and above some point start to "repair"
sharp parts of the music too. Start around 30 and raise it only while clicks remain.

**DeCrackle** (0 to 100, File Mode only) smooths the dense, low-level crackle of worn records
and shellac. Leave it at 0 for vinyl in good condition.

**Method**:

- *Wavelet*: the default; splits the signal into bands before detecting, finds clicks hidden
  under loud music.
- *Simple*: detects on the full signal; lighter on the CPU.
- *Wavelet x2 / x3* (File Mode only): add one or two lower-rate passes that catch longer
  clicks.

**Pitch protection** recognises strongly pitched sounds (brass, reeds, voices) and keeps the
declicker from mistaking their sharp edges for clicks. Leave it on.

**Reverse** (File Mode only) scans backwards in time, which protects sharp attacks such as
horn and drum onsets. The declicker judges each possible click against the audio next to it.
Scanning forwards, a drum hit is compared with the quiet moment before it and looks like a
click; scanning backwards, it is compared with its own loud decay and does not. A real click
is short either way, so it is still found. Measured on real record noise added to clean
music, Reverse found the same clicks with a third to a half of the false repairs, so the
suggested settings turn it on unless the record shows a reason not to:
- it makes *more* repairs in the music, a sign of sounds that stop suddenly (gated drums,
  cut-off notes), which look like sharp attacks when scanned backwards;
- it misses clicks between tracks that forward scanning repairs. A click just before a loud
  attack can hide behind that attack.
The analysis report says which applies.

**Channel mode**: *Stereo* treats left and right separately. *Merge* is for mono records
(mono LPs, 78s) captured in stereo. *Mono* is for one-channel files.

**Out / In / Noise**, the audition triad:

- *Out*: the repaired audio.
- *In*: the original, time-aligned with Out, so you can switch with no jump.
- *Noise*: only what is being removed. If you hear music in it, DeClick is too high.

**Raw recording.** In Live Monitor, what is recorded is always the input as it arrives,
before any processing. The live declicker only changes what you hear. Declick the raw file
afterwards in File Mode.

## 2. Installing

There are no prebuilt binaries yet; build from source with Rust 1.80 or newer
(https://rustup.rs). Always build with `--release`: the DSP is far too slow otherwise.

Windows:

    cargo build --release

Linux (Debian / Ubuntu package names):

    sudo apt install pkg-config libasound2-dev libgtk-3-dev
    cargo build --release

The programs are written to `target/release/`: `cleangroove` (the app), `cleangroove-cli` (batch
restoration) and `cleangroove-rt` (live monitoring and recording without the GUI). macOS is untested.

Start the app with `cleangroove`, `cleangroove --live` to open in Live Monitor, or `cleangroove FILE` to open
a recording.

## 3. The window

From top to bottom:

- **Menu bar**: File, Settings, View, Transport, Tools, Help ([section 6](#6-menus-and-shortcuts)).
- **Header**: in File Mode the open file (name, length, rate, bit depth) and Render Album; in
  Live Monitor the devices, rate and latency, and Open Last Take. The **File Mode / Live
  Monitor** buttons on the right switch modes.
- **Live input bar** (Live Monitor only): devices, rate, Start / Stop Monitoring.
- **Timeline**: the whole album (File Mode) or the current take (Live Monitor).
- **Waveform**: left and right channels.
- **Transport bar**: the player (File Mode) or the recorder (Live Monitor).
- **Audition triad and tuning**: Out / In / Noise; DeClick, DeCrackle, Pitch Protection,
  Reverse, Method.
- **Bottom panel**: Record Analysis (File Mode) or Levels (Live Monitor).
- **Status bar**: the latest message.

## 4. File Mode: restoring a recording

### Opening a file

File > Open Audio File (Ctrl+O) opens WAV, FLAC or AIFF. The whole file is loaded into
memory: about 420 MB for a 20-minute side at 44.1 kHz. File > Close File (Ctrl+W) closes it.

### Timeline and waveform

The **timeline** shows the whole file. After analysis, its colour shows how many exposed
clicks each part has, and triangles mark problem spots. Drag on it to move the view.

The **waveform** shows the part in view, left channel above right. Click to place the cursor;
drag to select a range, which the player loops. Zoom with the transport bar's Zoom + / Zoom -
/ Fit Track or the View menu.

When the view is 30 seconds or shorter, the waveform previews the repairs with the current
settings:

- Purple: the original. Green: the repaired output.
- Green pins on the top edge: repairs.
- Amber pins on the bottom edge: exposed clicks the current settings miss.

Wider views show the original only, so zooming out on an album stays fast.

### Playing

The transport bar plays from the cursor (Space), stops (Esc), goes to the start or end
(Home / End), and loops the selection when Loop Selection is on. Out / In / Noise (1 / 2 / 3)
and every tuning change apply while it plays.

### Record Analysis

Press **Analyze Album** in the bottom panel (or Tools > Analyze Album). It reads the whole
file and shows:

- **Click condition**: a grade for the record's clicks, measured in the silence between
  tracks, or in the quietest music when there is none (then it may under-read).
- **Suggested start**: DeClick, method and corrections to begin with, and the DeClick level
  above which repairs start to catch music. **Apply** sets them; "Current settings match"
  means they are in use.
- **Recording**: the peak level and any clipped samples, the music level, and a warning when
  the capture level is so low that bits go unused.
- **Problem spots**: the places with the most exposed clicks. Click one to jump there and
  loop it; press N for the next.
- **Needle thumps**: low thuds that repeat once per turn of the record (debris throwing the
  needle). The declicker removes only their sharp onset. Click a series to loop it and
  listen; if you hear thumps, switch on **Repair needle thumps**.
- **Corrections (after declick)**: run after the declicker, in preview and render:
  - *Flip right polarity*: when one channel is inverted (the analysis shows the L/R
    correlation).
  - *Balance*: removes a left-minus-right level difference. Suggested only for mono records;
    on stereo a level difference is usually the mix.
  - *Azimuth*: removes a timing offset between the channels (cartridge alignment).
  - *Subsonic high-pass* (18 Hz): removes DC, warp and tonearm resonance.
  - *Elliptical EQ* (100 Hz, stereo only): makes the lowest bass mono, which removes rumble
    and vertical groove noise.
  - *Stereo>Mono*: for mono records captured in stereo; both channels are declicked, aligned
    and summed.

  "Apply corrections" switches the whole chain off and on.

### Rendering

File > Render Album (or the Render Album button) asks where to save, then writes the
restored file as WAV, `name_repaired.wav` by default. When the corrections are on, it also
checks the output level and lowers the whole album evenly if it would clip; the status bar
reports the peak, true peak and loudness. The original file is never changed.

Rendering runs much faster than real time: about 30 times on a low-power two-core PC.

### Presets

Settings > Presets sets a starting point:

- *Vinyl LP 33 RPM*: DeClick 30.
- *Shellac 78 RPM*: DeClick 70, DeCrackle 45, Stereo>Mono.
- *Brass / reed protection*: DeClick 40, DeCrackle 15, Reverse.

All three keep pitch protection on.

## 5. Live Monitor: recording a record

### Choosing devices

The live input bar lists the inputs and outputs, with the sample rates both support. Press
Rescan after plugging something in. Devices are fixed while monitoring; stop to change them.

On **Windows** each device runs at the rate and channel count set in Sound settings (More
sound settings > Recording or Playback > the device > Properties > Advanced > Default
Format).

On **Linux** the list shows the system default, PipeWire, and each sound card's direct
device, for example "USB AUDIO CODEC - USB Audio (direct)":

- The **direct** device records the card bit for bit, but no other program can use the card
  while it is open.
- The **shared** devices (PipeWire, system default) work alongside other programs.

A good setup is the card's direct device as input and PipeWire as output.

CleanGroove picks the best format a device offers: 32-bit float, then 32-bit or 16-bit integer,
stereo before mono.

### Monitoring

**Start Monitoring** opens the devices. You hear the input through the live declicker, about
0.3 s late (0.1 s of buffer plus the declicker's look-ahead). The audition triad, DeClick,
Pitch Protection and Method act at once. Changing the Method restarts the declicker: the
monitor is silent for a moment; a recording carries on. DeCrackle, Reverse and Wavelet x2 /
x3 are File Mode only.

### Levels panel

- **Input level** (raw, before the declicker): left and right bars, a white peak-hold line, a
  tick at -6 dBFS, and a CLIP light per channel that stays lit until clicked. Underneath:
  the highest peak since Reset, and advice on whether to turn the gain up or down. The
  needle drop alone can clip: press Reset once the needle is in the groove.
- **Monitor output** (after the declicker): output bars and the monitor **Volume**, which
  changes only what you hear.
- **Declicker**: how busy it is, how many samples it has repaired in the last moments, and
  the latency.
- **Stream health**: frames lost from the declicker's input or from the recording (should
  stay 0), and output dropouts.

If left and right are identical sample for sample for two seconds of music, the panel says
**Input is MONO**: the device delivers one channel and the system copies it to both sides.

### Setting the level

Clipping happens in the converter, before CleanGroove sees the signal; no software volume can
undo it. Play the loudest track and adjust the gain *before* the converter (the preamp, the
turntable's line output, or the interface's gain knob) until the peaks sit near -6 dB. A
system input-level slider usually acts after the converter: turning it down hides clipping
rather than preventing it.

### Recording

In the transport bar, type a **Name** and choose a **Folder**; the bar shows the file the
next take will be written to. Then:

- **Rec** starts a take with the audio from that moment. Press it before dropping the needle.
- **Pause** writes what has arrived and then skips audio. **Resume** carries on in the same
  file, and the join is marked.
- **Stop** finishes the take.

The centre of the window shows REC or PAUSED with the take's time and file. While a take is
recording you cannot switch to File Mode; Stop Monitoring finishes the take first.

**Open Last Take** (header, or the File menu) stops monitoring and opens the take in File
Mode.

### The live waveform and take timeline

The scrolling waveform shows the last 10 seconds (View or Settings: 5 to 60 s), newest on the
right, in step with what you hear:

- Purple / green: the original and the output, as in File Mode.
- Green pin on top: a repair.
- Amber pin at the bottom: a click found but held back by pitch protection.
- Red marks: the input clipped.

Hover for the level and repair counts at that moment. **Freeze** holds the view while audio
and recording carry on; the last two minutes are kept.

The **take timeline** above it shows the whole take so far, one cell per second, coloured by
how much of that second was repaired, with clipped seconds marked red and pause joins in
amber.

### Settings

Settings > Settings holds the recordings folder, the name, the file-name template, the live
waveform length and the monitor volume. The template uses `{name}`, `{n}` (side or take
number, counting up to the first free name), `{date}` and `{time}` (UTC); the default is
`{name}_side{n}`. Settings are not saved between runs yet.

## 6. Menus and shortcuts

| Menu | Entries |
|---|---|
| File | Open Audio File (Ctrl+O), Open Last Take, Close File (Ctrl+W), Render Album, Exit (Ctrl+Q) |
| Settings | Settings window; Presets |
| View | File Mode / Live Monitor; File Mode: Zoom In / Out, Fit Entire Track; Live: waveform length, Freeze |
| Transport | File Mode: Play / Pause (Space), Stop (Esc), Go to Start / End (Home / End), Select Entire Track (Ctrl+A), Loop Selection, Next Problem Spot (N). Live: Start / Stop Monitoring, Record, Pause / Resume, Stop Take. Both: Out / In / Noise (1 / 2 / 3) |
| Tools | Analyze Album, Rescan Audio Devices |
| Help | User Guide (F1), About |

Shortcuts are off while you type in a text field. Space, Esc, Home, End, N and Ctrl+A act in
File Mode only.

## 7. Command line: cleangroove-cli

`cleangroove-cli` restores one file. Run `cleangroove-cli --help` for the full list.

    cleangroove-cli --input side_a.wav --analyze-only
    cleangroove-cli --input side_a.wav --output side_a_restored.wav --auto
    cleangroove-cli --input side_a.wav --output out.wav --preset stereo-lp --dcl 30 --method wavelet

| Option | Meaning |
|---|---|
| `-i, --input FILE` | WAV, FLAC or AIFF to restore |
| `-o, --output FILE` | where to write the restored WAV |
| `--diff-output FILE` | also write what was removed (original minus repaired) |
| `--analyze-only` | analyse, print the report and the suggested command, then exit |
| `--auto` | use the analysis's suggested settings |
| `--preset NAME` | `stereo-lp`, `stereo-brass`, `stereo-percussion`, `mono-lp`, `shellac78` |
| `--dcl N`, `--dcr N` | DeClick and DeCrackle, 0 to 100 |
| `--method M` | `simple`, `wavelet`, `wavelet-x2`, `wavelet-x3` |
| `--mode M` | `stereo`, `merge`, `mono` |
| `--pitch-protection true/false`, `--reverse true/false` | as in the GUI |
| `--no-post` | declick only: no corrections, no level check |
| `--subsonic HZ/off`, `--elliptical HZ/off` | corrections (defaults 18 and 100 Hz) |
| `--azimuth-us US`, `--balance-db DB`, `--flip-polarity true/false`, `--mono-sum true/false` | corrections |
| `--dethump` | repair needle thumps (check with `--analyze-only` first) |

## 8. Command line: cleangroove-rt

`cleangroove-rt` monitors and records without the GUI.

    cleangroove-rt --list-devices
    cleangroove-rt -i "USB" -o "Speakers" -r 48000 --record side_a.wav

| Option | Meaning |
|---|---|
| `--list-devices` | list inputs and outputs with their rates, then exit |
| `-i, --input NAME`, `-o, --output NAME` | device by name, or a unique part of it |
| `-r, --sample-rate HZ` | default 44100 |
| `-s, --sensitivity N` | 0 to 9: ClickRepairRT's stepped slider (5 = default); 10 to 100: DeClick as in File Mode |
| `-m, --method M` | `simple` or `wavelet` |
| `--mode M` | `sound-out`, `sound-in`, `noise` |
| `--record FILE` | record the raw input from the start |
| `--pipe` | read and write raw 24-bit PCM on stdin / stdout instead of audio devices |

While it runs, type a letter and press Enter: `p` pause, `r` resume, `s` stop the take, `n`
start a new take next to the `--record` file, `q` quit. The status line shows the input
meters, the declicker's activity and REC / PAUSED with the take's time.

## 9. Files CleanGroove writes

A Live Monitor take, for example `KindOfBlue_side1.wav`:

- `KindOfBlue_side1.wav`: the raw recording, 24-bit WAV at the device's rate. (A 16-bit
  converter's samples are stored exactly in 24 bits.)
- `KindOfBlue_side1.wav.cleangroove-meta.json`: the live settings, the input device, the pause points
  and any lost frames.
- `KindOfBlue_side1_labels.txt`: written when the take was paused; one label per join, for
  Audacity (File > Import > Labels).

An existing file is never overwritten.

A File Mode render: one WAV, by default `name_repaired.wav`.

## 10. Troubleshooting

- **"Input is MONO"**: the device is set to one channel. Windows: set a 2-channel Default
  Format in the device's Sound settings. Linux: use the card's direct device, or a stereo
  input profile in pavucontrol.
- **No common rate**: the input and output must run at the same rate. Choose devices that
  share one, or change a device's rate in the system's sound settings.
- **A device is busy or missing (Linux)**: another program holds it. Close that program and
  press Rescan, or use PipeWire.
- **"Unsupported input format" or "offers no sample format this program can use"**: the
  device offers only formats CleanGroove cannot read. The message lists what it offers.
- **Lost frames or dropouts**: the computer could not keep up. Close other programs, or use
  the Simple method on a slow computer. Lost *recording* frames are serious: the take has
  gaps.
- **Clicks remain**: raise DeClick in small steps and listen to Noise; try Wavelet x2 for
  longer clicks; check the problem spots. The live declicker repairs clicks up to about 3 ms
  long; longer damage needs File Mode.
- **Music is being repaired** (you hear music in Noise): lower DeClick, keep Pitch
  Protection on, try Reverse.

## 11. Known limitations

- Settings are not saved between runs.
- File Mode playback always uses the system's default output device.
- The live declicker cannot use DeCrackle, Reverse or Wavelet x2 / x3.
- `cleangroove-rt`: pitch protection cannot be switched off from the command line.
- Opening a file and drawing the repair preview run on the interface thread; the window can
  pause briefly on large files. (Analysis and rendering run in the background.)
- macOS is untested.

## 12. CleanGroove in your browser

Try it without installing anything: **https://kevinosi.github.io/CleanGroove/**

The web version is File Mode in a browser tab. It runs CleanGroove's own engine, compiled to
WebAssembly, on your computer: the file you open is never uploaded, and nothing is stored once
you close the tab.

### What it does

- **Open a recording**: drop a WAV or FLAC file on the page, or use Open. The browser decodes it at
  its own sample rate. MP3 and other lossy files open too, but they smear clicks, so they are a
  poor test.
- **Live preview**: Play, and hear the restoration as it plays. Out (Clean), In (Bypass) and
  Noise (Delta) switch mid-play (keys 1, 2, 3), and changes to DeClick, DeCrackle, the method or
  the corrections are heard within a fraction of a second. Click the waveform to jump.
- **Waveform**: the full-file timeline at the top (drag it to move the view), and the left and
  right channels below, original in violet and restored in green. At 30 s or closer, the
  repairs in view are previewed with a marker for each, as in the desktop app. The mouse wheel
  zooms; Shift and the wheel pans.
- **Record Analysis**: Analyze Album grades the record, maps its exposed clicks, measures
  levels and channels, lists problem spots (N jumps to the next) and suggests settings; Apply
  sets them. Corrections (polarity, balance, azimuth, subsonic, elliptical EQ, Stereo>Mono, and
  needle-thump repair when the analysis finds thumps) work as in section 4.
- **Render & Download**: declicks the whole file, then the thump repair and the corrections, in
  the same order as the desktop app, including its level check (one gain for the whole file if
  the corrections push peaks over full scale). The result downloads as a 24-bit WAV.
- **Presets**: the three desktop presets, from the Presets list.

### How it compares with the desktop app

Same engine, same settings, same results, to within the last bit: on test recordings the
analysis gives the same figures, and a 24-bit render with corrections matches the command-line
tool to within one 24-bit step (about -138 dBFS). Tiny differences come from the browser's own
maths functions rounding differently in the last bit; untouched audio is identical.

### Limitations

- **File Mode only.** Live Monitor (recording from a turntable) and Tracks (splitting and
  tagging) are in the desktop app.
- **One job at a time.** Analysis and rendering pause playback while they run. Expect roughly
  30-50x real time, depending on the computer: about half a minute for a 20-minute side.
- **Memory.** The browser holds the whole file several times over: allow about 1.5-2 GB for a
  full LP side, about 3 GB for a 35-minute album at 44.1 kHz. The engine itself can use at most
  4 GB, which is roughly 90 minutes at 44.1 kHz or 40 minutes at 96 kHz; split anything longer.
  Fine on a computer; long files may not open on a phone or tablet.
- **Output format.** Downloads are 24-bit WAV, stereo (a mono file is played and saved as two
  identical channels). FLAC export and tagging are in the desktop app.
- **Sample rate.** Playback runs at the file's own rate. If a browser cannot, the page says it
  resampled the file; the render then works on the resampled audio.
- **Nothing is kept.** Settings, analysis and renders are lost when the tab closes.
- **Browsers.** Tested in Chromium-based browsers (Chrome, Edge). Firefox and Safari should work
  but are not tested yet.

### Running it from a copy of the source

To try a change, or to use it without an internet connection, build it and serve the `web`
folder from your own computer (the page needs a web server; opening `index.html` from disk
does not work):

    rustup target add wasm32-unknown-unknown
    sh web/build.sh
    python3 -m http.server -d web 8000

On Windows, in PowerShell:

    rustup target add wasm32-unknown-unknown
    powershell -ExecutionPolicy Bypass -File web\build.ps1
    python -m http.server -d web 8000

Then open http://localhost:8000. The first command is needed once; run the build again after
changing the engine or pulling a new version.
