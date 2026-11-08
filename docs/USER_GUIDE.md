# CleanGroove User Guide

CleanGroove removes clicks and crackle from vinyl (and other disc) recordings. File Mode restores a recording you already have, Live Monitor records a record as it plays and lets you hear it declicked while it does, and Tracks splits a finished album into its tracks and fills in the album details.

The usual workflow: record each side raw in Live Monitor, then open the take in File Mode, analyse it, and render the restored copy. Open the restored copy in Tracks to place the track splits. The raw recording is your archive; the declicker never touches it.

## Three modes

Switch with the buttons at the top right, or View > File Mode / Live Monitor / Tracks. The transport bar, the Out / In / Noise buttons and the DeClick controls work on the file in File Mode and on the live input in Live Monitor.

- File Mode: open a WAV, FLAC or AIFF file, find and preview the repairs, render a restored copy.
- Live Monitor: pick your input and output, monitor the record through the real-time declicker, and record the raw input.
- Tracks: look up the album on Discogs, place the start and end of every track on the waveform, and edit the titles and album details. It plays the file as it is, without the declicker.

You cannot leave Live Monitor while a take is recording, so a take is never cut short by accident.

File Mode also runs in a web browser, with nothing to install: https://kevinosi.github.io/CleanGroove/ . It is the same engine, running on your computer (the file is never uploaded), with live preview, Record Analysis, the corrections and a 24-bit WAV download. Live Monitor and Tracks are desktop only. See the manual, section 12, for what else differs.

## File Mode: restoring a recording

1. File > Open Audio File (Ctrl+O).
2. Press Analyze Album in the bottom panel (or Tools > Analyze Album). It grades the record, measures its levels and channels, and lists problem spots.
3. Apply the suggested settings, or set DeClick yourself. Start low (around 30) and raise it only while you still hear clicks. The suggestion normally turns Reverse on: scanning backwards leaves drum hits and other sharp attacks alone and still finds the clicks (see the manual for when it is left off).
4. Listen. Zoom into 30 seconds or less to see the repairs drawn on the waveform. Press N to jump to the next problem spot.
5. Switch between Out, In and Noise (keys 1, 2, 3) to compare. Noise plays only what is being removed: if you hear music in it, DeClick is too high.
6. File > Render Album writes the restored file. The original is never changed.

Corrections (bottom panel) run after the declicker: polarity, balance, azimuth, subsonic filter, elliptical EQ and Stereo>Mono. The analysis suggests which ones a record needs. Needle thumps (a low thud once per turn) can be repaired on request when the analysis finds them.

## Tracks: splitting an album

1. Open the restored album (Ctrl+O) and press Tracks. One file holding the whole album works best, side A then side B.
2. Press Search... in the bottom panel and find the record: by artist and album, or by catalogue number or barcode under Other. It looks in your own Discogs collection first (marked Mine), then on Discogs and MusicBrainz, records before CDs. Press Use on the right release. If you already know it, type its Discogs release number in the Release box, or paste a discogs.com or musicbrainz.org release link, and press Look Up instead.
   CleanGroove fetches the album details and the tracklist and places every track from the release's lengths, fine-tuned to a quiet gap where one is close by. It is a first guess: listen to each split and move it.
3. The top strip shows the whole album with a colour per track. Click it to move the view. The waveform below is the zoomed view; the track lane under it shows the tracks in view. Shift+mouse wheel scrolls the waveform, Ctrl+mouse wheel zooms it. Click a track in the lane, or its number in the table, to put the cursor at its start; |<< Track and Track >>| on the transport bar jump between track starts.
4. Drag a boundary in the lane to move it. It snaps to quiet, the cursor and other boundaries; hold Alt to place it freely, or switch Snap off.
5. Linked tracks share one boundary (a line with a diamond): moving it moves the end of one track and the start of the next. Where there is dead space between two tracks, such as the side change, the end and the start are separate brackets. Click the chain above a boundary to link or unlink it, or drop one bracket onto the other to link them. Linking closes the gap by extending the earlier track.
6. In the table, edit the titles and artists, and type or drag the start and end times. Discogs shows the length Discogs gives; Diff is the difference, in amber when it is more than 10 seconds.
7. Without a Discogs release: put the cursor where a track starts and press S (Split at Cursor). Merge with Previous and Remove undo splits.
8. The cover: after a lookup CleanGroove fetches the front cover (from the Cover Art Archive, through MusicBrainz, or from Discogs) and shows it under the album details. Fetch Cover tries again, Choose File... takes an image of your own, Remove drops it. You can also drag an image from your browser (or a folder) onto the window, as in Mp3tag: the cover box lights up, and the image you drop replaces the cover; when the browser only hands over the image's link, CleanGroove downloads it. Or use Copy image in the browser and press Paste. JPEG, PNG, WebP and GIF are taken; the cover is kept as JPEG.

Dropping a WAV, FLAC or AIFF file on the window opens it, in any mode. It is kept next to the rip as name.wav.cover.jpg.
9. Press Export... (top right of the panel). Choose the folder albums go in, check the names in the preview and press Export.

A boundary you drop in the lane, split or set from the cursor settles on the quietest sample within 5 milliseconds, so a track played on its own starts and ends without a click; the move is too small to see or hear. Typed times are kept exactly.

### Exporting

Each track is written as a FLAC file, by default to Album Artist\Album\01 - Title.flac under the folder you choose. The Names box takes MP3tag-style fields: %albumartist% %album% %artist% %title% %track% %tracktotal% %year% %label% %catalog% %genre%, $num(%track%,2) for a two-digit track number, and \ for a folder. Characters a file name cannot hold (such as / in AC/DC) become _.

- Bit depth: as the rip (24 bit for a 24-bit rip), or 16 bit (the lowest 8 bits are dropped, without dither).
- Tags: album artist, track artist, album, title, track number (01), track total, year, label, catalogue number, genre, style and the Discogs or MusicBrainz release id. The cover is embedded in every track, and written as cover.jpg in the album folder (both can be switched off).
- The album folder also gets album.cleangroove-tracks.json (the splits and details, to export again later), copies of the rip's sidecar files (the capture settings and labels, and the render settings File Mode wrote; the originals stay where they are), and CleanGroove log.txt, a readable record of what was done.
- Every file is decoded again after writing and compared with the rip sample for sample before it gets its name. Files already there are not replaced unless you tick Replace.

The folder, names, bit depth and cover choices are remembered. Render Album in File Mode also writes name.wav.cleangroove-render.txt next to the restored file: the settings it used, which the export copies into the album folder.

Every track has a track artist as well as the album's artist. They are the same on most records; on soundtracks and compilations Discogs fills in each track's artist. Changing the album artist also changes the tracks that had the old one.

Tracks are numbered 01, 02, ... straight through the album. The splits and details are saved as you go, next to the audio file as name.wav.cleangroove-tracks.json, and come back when you open the file again.

Searching Discogs and your collection needs a Discogs token: on discogs.com go to Settings > Developers > Generate new token, then paste it in Settings > Online services and press Save. CleanGroove keeps it in the system keychain (Windows Credential Manager), not in a file. MusicBrainz needs no account; it asks for an email address or web page to contact you about your requests, which you can enter there too. Release numbers and links work without either.

## Live Monitor: recording a record

1. Choose the Input (your turntable or audio interface) and the Output (your speakers or headphones), and the Rate. Press Rescan if you plugged something in.
2. Press Start Monitoring. You hear the record through the declicker, about 0.3 seconds late.
3. Set the level (see below).
4. Type a Name and choose a Folder in the transport bar. The bar shows the file the next take will be written to.
5. Press Rec before you drop the needle. Pause skips audio and keeps the same file; Resume carries on. Stop finishes the take.
6. Open Last Take loads the finished take in File Mode.

What you record is always the raw input, before the declicker. DeClick, Pitch Protection, Method and Out / In / Noise only change what you hear, and can be changed while recording. Changing the Method makes the monitor go quiet for a moment; the recording carries on.

## Setting the input level

Clipping happens inside the audio interface, before CleanGroove sees the signal, so no software volume can undo it. Set the level once for your turntable, preamp and interface:

- Play the loudest track. Watch the INPUT LEVEL meters in the bottom panel.
- Aim for peaks near the -6 dB tick. The line under the meters says whether to turn the gain up or down.
- The red CLIP lights stay on until you click them. The needle drop alone can clip: press Reset once the needle is in the groove.
- Turn the gain down at the source: the preamp, the turntable's line output, or the interface's gain knob. A Windows or Linux input level slider usually acts after the converter and cannot prevent clipping.

The monitor Volume slider only changes what you hear, never the recording.

## The waveform

Shift+mouse wheel (or a sideways wheel or touchpad swipe) scrolls the waveform through the file; Ctrl+mouse wheel zooms in and out around the pointer. In File Mode the repairs are drawn again a moment after the view stops moving.

File Mode shows the whole album as a timeline (heat = clicks found, triangles = problem spots) and the part you are looking at as a waveform. Live Monitor shows the take so far as a timeline and the last few seconds as a scrolling waveform.

- Purple: the original audio. Green: the output after repair.
- Green pin on the top edge: a repair.
- Amber pin on the bottom edge: in File Mode, an exposed click the current settings miss; in Live Monitor, a click found but held back by Pitch Protection.
- Red marks (Live Monitor): the input clipped there.
- Live Monitor: hover for the level and repair counts, Freeze to hold the view, View to change its length.

## Files a take produces

- name.wav: the raw recording, 24-bit.
- name.wav.cleangroove-meta.json: the live settings, the input device, pause points, and any lost frames (should be 0).
- name_labels.txt: when the take was paused, one label per join. Import it in Audacity (File > Import > Labels).

Takes are named from the Name and the template (Settings), by default name_side1, name_side2 and so on. An existing file is never overwritten.

## Keyboard shortcuts

- Space: play / pause (File Mode, Tracks)
- Esc: stop (File Mode, Tracks)
- Home / End: go to start / end (File Mode, Tracks)
- 1 / 2 / 3: Out / In / Noise (File Mode, Live Monitor)
- N: next problem spot (File Mode)
- S: split the track at the cursor (Tracks)
- [ / ]: set the selected track's start / end to the cursor (Tracks)
- B: hear the selected track's start, from 3 seconds before (Tracks)
- Tab / Shift+Tab: next / previous track, heard from just before its start (Tracks)
- Page Down / Page Up: go to the start of the next / this or previous track (Tracks)
- Shift+wheel / Ctrl+wheel: scroll / zoom the waveform (File Mode, Tracks)
- Ctrl+O: open a file
- Ctrl+W: close the file
- Ctrl+A: select the whole file (File Mode)
- Ctrl+Q: quit
- F1: this guide

Shortcuts are off while you type in a text field.

## Troubleshooting

- "Input is MONO": the device delivers one channel and the system copies it to both. On Windows set the device to 2 channels (Sound settings > More sound settings > Recording > the device > Properties > Advanced > Default Format). On Linux choose the card's direct device, or a stereo input profile in pavucontrol.
- No common rate / rate warning: input and output must run at the same rate. On Windows each device runs at the rate set in Sound settings; on Linux PipeWire runs at its clock rate (48 kHz unless changed).
- A device is busy or missing: another program has it open. Close that program and press Rescan, or choose a shared device (PipeWire / system default on Linux).
- Dropouts or lost frames (Stream Health): the computer could not keep up. Close other programs; on a slow computer use the Simple method.
- Linux device list: CleanGroove shows the system default, PipeWire and each card's direct device. The direct device (for example "USB AUDIO CODEC - USB Audio (direct)") records the card bit for bit; while it is open no other program can use that card.
