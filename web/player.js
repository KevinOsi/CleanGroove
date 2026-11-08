// Live preview player, on the browser's audio thread (AudioWorklet).
//
// The worker streams the engine's output a little ahead of the playhead: original, declicked
// and final audio for each frame. This plays it: Out (Clean) is the final audio, In (Bypass) the
// original, Noise (Delta) what the declicker took out. The context runs at the file's own sample
// rate, so one output frame is one file frame.
//
// From the page: play {start, settings}, pause, seek {frame, settings}, restart {settings},
// mode {mode}, port {port} (to the worker). To the page: pos {frame}, ended.

const LEAD_SECONDS = 0.12;   // a settings change takes effect this far ahead of the playhead
const AHEAD_SECONDS = 1.5;   // how much audio to keep queued
const FADE_FRAMES = 256;     // Out / In / Noise crossfade

class Player extends AudioWorkletProcessor {
  constructor() {
    super();
    this.worker = null;
    this.chunks = [];        // {start, len, d: [origL, origR, declL, declR, outL, outR]}
    this.gen = 0;
    this.pos = 0;
    this.playing = false;
    this.waiting = false;    // a request for more is out
    this.ended = false;
    this.mode = 0;
    this.gain = [1, 0, 0];
    this.lastReport = 0;
    this.port.onmessage = (ev) => this.onPage(ev.data);
  }

  onPage(m) {
    if (m.type === "port") {
      this.worker = m.port;
      this.worker.onmessage = (ev) => this.onWorker(ev.data);
    } else if (m.type === "play" || m.type === "seek") {
      this.pos = m.start;
      this.startStream(m.start, m.settings, true);
      if (m.type === "play") this.playing = true;
    } else if (m.type === "pause") {
      this.playing = false;
    } else if (m.type === "restart") {
      // New settings: keep what is queued up to a point just ahead, then splice in the new stream.
      // Never past the end of what is queued, or the playhead would wait on a gap.
      const at = Math.min(this.pos + Math.round(LEAD_SECONDS * sampleRate), this.queuedEnd());
      this.startStream(at, m.settings, false);
    } else if (m.type === "mode") {
      this.mode = m.mode;
    }
  }

  startStream(at, settings, clear) {
    this.gen++;
    this.ended = false;
    if (clear) {
      this.chunks = [];
    } else {
      // Drop queued audio from `at` on.
      this.chunks = this.chunks.filter((c) => c.start < at);
      const last = this.chunks[this.chunks.length - 1];
      if (last && last.start + last.len > at) last.len = at - last.start;
    }
    this.waiting = true;
    if (this.worker) this.worker.postMessage({ type: "start", gen: this.gen, start: at, settings });
  }

  onWorker(m) {
    if (m.gen !== this.gen) return;   // a stream the page has since replaced
    this.waiting = false;
    if (m.type === "chunk") {
      this.chunks.push({ start: m.start, len: m.data[0].length, d: m.data });
    } else if (m.type === "end") {
      this.ended = true;
    }
  }

  queuedEnd() {
    const last = this.chunks[this.chunks.length - 1];
    return last ? last.start + last.len : this.pos;
  }

  process(inputs, outputs) {
    const out = outputs[0];
    const L = out[0], R = out[1] || out[0];
    const n = L.length;
    if (!this.playing) {
      L.fill(0); R.fill(0);
      return true;
    }
    for (let i = 0; i < n; i++) {
      // Move the gains a step toward the chosen mode (a short crossfade).
      for (let k = 0; k < 3; k++) {
        const target = k === this.mode ? 1 : 0;
        const g = this.gain[k];
        this.gain[k] = g + Math.sign(target - g) * Math.min(Math.abs(target - g), 1 / FADE_FRAMES);
      }
      while (this.chunks.length && this.chunks[0].start + this.chunks[0].len <= this.pos) this.chunks.shift();
      const c = this.chunks[0];
      if (!c || c.start > this.pos) {
        // Not streamed yet (just after a seek): silence, and hold the position.
        L[i] = 0; R[i] = 0;
        continue;
      }
      const j = this.pos - c.start, d = c.d, [g0, g1, g2] = this.gain;
      const ol = d[0][j], or = d[1][j];
      L[i] = g0 * d[4][j] + g1 * ol + g2 * (ol - d[2][j]);
      R[i] = g0 * d[5][j] + g1 * or + g2 * (or - d[3][j]);
      this.pos++;
    }
    if (this.ended && !this.chunks.length) {
      this.playing = false;
      this.port.postMessage({ type: "ended", frame: this.pos });
    }
    // Ask for more when less than AHEAD_SECONDS is queued.
    if (!this.waiting && !this.ended && this.worker && this.queuedEnd() - this.pos < AHEAD_SECONDS * sampleRate) {
      this.waiting = true;
      this.worker.postMessage({ type: "need", gen: this.gen, chunks: 4 });
    }
    if (currentFrame - this.lastReport > sampleRate / 30) {
      this.lastReport = currentFrame;
      this.port.postMessage({ type: "pos", frame: this.pos });
    }
    return true;
  }
}

registerProcessor("cleangroove-player", Player);
