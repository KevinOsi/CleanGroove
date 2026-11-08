// Holds the loaded file in CleanGroove's engine (cr_wasm.wasm), off the page's thread.
//
// From the page:     load, settings, analyze, range, render
// From the player:   start (a new preview stream at a frame), need (more of it)
// The player (an AudioWorklet) talks to this worker over its own MessagePort, so live preview
// keeps flowing without passing through the page.

let x = null;          // wasm exports
let s = 0;             // session pointer
let total = 0;
let player = null;     // MessagePort to the AudioWorklet
let streamGen = -1;
let streamPos = 0;
let lastSettings = null;
const STREAM_CHUNK = 4096;
let wav = null;        // the render's WAV file, filled block by block (see emit)

async function engine() {
  if (x) return x;
  const imports = {
    env: {
      cg_progress: (stage, f) => self.postMessage({ type: "progress", stage, f }),
      cg_emit: (pass, ptr, len) => emit(pass, ptr >>> 0, len >>> 0),
    },
  };
  const url = new URL("cr_wasm.wasm" + self.location.search, self.location);
  let inst;
  try {
    inst = (await WebAssembly.instantiateStreaming(fetch(url), imports)).instance;
  } catch (e) {
    // Servers that do not send application/wasm: fall back to a plain download.
    inst = (await WebAssembly.instantiate(await (await fetch(url)).arrayBuffer(), imports)).instance;
  }
  x = inst.exports;
  return x;
}

const mem = () => x.memory.buffer;
// Pointers come back from the engine as signed 32-bit numbers: past 2 GB of engine memory they
// read as negative unless taken as unsigned (>>> 0).
const f32 = (ptr, n) => new Float32Array(mem(), ptr >>> 0, n);

/** Starts the render's 24-bit WAV file: the output arrives in blocks, so the engine never holds
 *  the whole of it (a long album would not fit the browser's memory). */
function wavBegin(frames, sampleRate) {
  const bytes = 44 + frames * 6;
  const buf = new ArrayBuffer(bytes), v = new DataView(buf);
  const str = (o, t) => { for (let i = 0; i < t.length; i++) v.setUint8(o + i, t.charCodeAt(i)); };
  str(0, "RIFF"); v.setUint32(4, bytes - 8, true); str(8, "WAVE");
  str(12, "fmt "); v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 2, true);
  v.setUint32(24, sampleRate, true); v.setUint32(28, sampleRate * 6, true); v.setUint16(32, 6, true); v.setUint16(34, 24, true);
  str(36, "data"); v.setUint32(40, frames * 6, true);
  wav = { buf, u8: new Uint8Array(buf), at: 44, pass: 0, clipped: 0 };
}

/** One block of render output (interleaved left, right). A new pass (the corrections again,
 *  with an album gain) writes the file over from the start. */
function emit(pass, ptr, len) {
  if (!wav) return;
  if (pass !== wav.pass) { wav.pass = pass; wav.at = 44; wav.clipped = 0; }
  const src = new Float32Array(mem(), ptr, len), u8 = wav.u8;
  let o = wav.at;
  for (let i = 0; i < len && o + 3 <= u8.length; i++) {
    let v = Math.round(src[i] * 8388608);
    if (v > 8388607) { v = 8388607; wav.clipped++; } else if (v < -8388607) { v = -8388607; wav.clipped++; }
    u8[o] = v & 255; u8[o + 1] = (v >> 8) & 255; u8[o + 2] = (v >> 16) & 255;
    o += 3;
  }
  wav.at = o;
}

function applySettings(values) {
  if (!values) return;
  lastSettings = values;
  new Float64Array(mem(), x.cg_settings_buffer() >>> 0, values.length).set(values);
  x.cg_set_settings(s);
}

function sendChunk() {
  if (!player || streamGen < 0) return;
  const n = x.cg_stream_pull(s, STREAM_CHUNK);
  if (n === 0) {
    player.postMessage({ type: "end", gen: streamGen, at: streamPos });
    return;
  }
  const bufs = [];
  for (let i = 0; i < 6; i++) bufs.push(new Float32Array(f32(x.cg_stream_buf(s, i), n)));
  player.postMessage({ type: "chunk", gen: streamGen, start: streamPos, data: bufs }, bufs.map((b) => b.buffer));
  streamPos += n;
}

function onPlayer(ev) {
  const m = ev.data;
  if (!s) return;
  if (m.type === "start") {
    applySettings(m.settings);
    streamGen = m.gen;
    streamPos = Math.max(0, Math.min(m.start, total));
    x.cg_stream_start(s, streamPos);
    sendChunk();
    sendChunk();
  } else if (m.type === "need" && m.gen === streamGen) {
    for (let i = 0; i < (m.chunks || 1); i++) sendChunk();
  }
}

self.onmessage = async (ev) => {
  const m = ev.data;
  try {
    await engine();
    if (m.type === "port") {
      player = m.port;
      player.onmessage = onPlayer;
    } else if (m.type === "load") {
      if (s) x.cg_session_free(s);
      streamGen = -1;
      total = m.left.length;
      s = x.cg_session_new(m.sampleRate, total);
      f32(x.cg_session_input_l(s), total).set(m.left);
      f32(x.cg_session_input_r(s), total).set(m.right);
      x.cg_session_ready(s);
      applySettings(m.settings);
      self.postMessage({ type: "loaded", total });
    } else if (m.type === "settings") {
      applySettings(m.settings);
    } else if (m.type === "analyze") {
      const t0 = performance.now();
      const ptr = x.cg_analyze(s) >>> 0;
      const json = new TextDecoder().decode(new Uint8Array(mem(), ptr, x.cg_text_len(s)));
      self.postMessage({ type: "analysis", analysis: JSON.parse(json), seconds: (performance.now() - t0) / 1000 });
      // The analysis feeds balance and azimuth: settings are applied again with it known.
      applySettings(lastSettings);
    } else if (m.type === "slice") {
      // The view worker: a stretch of the file (with warm-up and context around the view) in a
      // session of its own, so the waveform preview never holds up playback.
      if (s) x.cg_session_free(s);
      total = m.left.length;
      s = x.cg_session_new(m.sampleRate, total);
      f32(x.cg_session_input_l(s), total).set(m.left);
      f32(x.cg_session_input_r(s), total).set(m.right);
      x.cg_session_ready(s);
      applySettings(m.settings);
      if (m.thumps.length) {
        const ptr = x.cg_thumps_buffer(s, m.thumps.length / 2) >>> 0;
        new Uint32Array(mem(), ptr, m.thumps.length).set(m.thumps);
        x.cg_set_thumps(s);
      }
      const t0 = performance.now();
      const count = x.cg_range(s, m.start - m.offset, m.len);
      const n = x.cg_range_len(s);
      const l = new Float32Array(f32(x.cg_range_out(s, 0), n));
      const r = new Float32Array(f32(x.cg_range_out(s, 1), n));
      const markers = new Uint32Array(new Uint32Array(mem(), x.cg_range_markers(s) >>> 0, count * 3));
      for (let i = 0; i < markers.length; i += 3) markers[i] += m.offset;
      self.postMessage({ type: "range", id: m.id, start: m.start, l, r, markers, seconds: (performance.now() - t0) / 1000 },
        [l.buffer, r.buffer, markers.buffer]);
    } else if (m.type === "range") {
      applySettings(m.settings);
      const t0 = performance.now();
      const count = x.cg_range(s, m.start, m.len);
      const n = x.cg_range_len(s);
      const l = new Float32Array(f32(x.cg_range_out(s, 0), n));
      const r = new Float32Array(f32(x.cg_range_out(s, 1), n));
      const markers = new Uint32Array(new Uint32Array(mem(), x.cg_range_markers(s) >>> 0, count * 3));
      self.postMessage({ type: "range", id: m.id, start: m.start, l, r, markers, seconds: (performance.now() - t0) / 1000 },
        [l.buffer, r.buffer, markers.buffer]);
    } else if (m.type === "render") {
      applySettings(m.settings);
      const t0 = performance.now();
      x.cg_render_begin(s);
      const CHUNK = 65536;
      let done = 0, last = 0;
      while (done < total) {
        done = x.cg_render_step(s, CHUNK);
        const now = performance.now();
        if (now - last > 100) {
          last = now;
          self.postMessage({ type: "progress", stage: 2, f: 0.95 * done / total, seconds: (now - t0) / 1000 });
        }
      }
      self.postMessage({ type: "progress", stage: 2, f: 0.96, seconds: (performance.now() - t0) / 1000 });
      wavBegin(total, m.sampleRate);
      const n = x.cg_render_finish(s);
      const levels = Array.from(new Float64Array(mem(), x.cg_render_levels(s) >>> 0, 4));
      const { buf, clipped } = wav;
      wav = null;
      self.postMessage({ type: "rendered", wav: buf, frames: n, clipped, levels, seconds: (performance.now() - t0) / 1000 }, [buf]);
    }
  } catch (e) {
    self.postMessage({ type: "error", message: String((e && e.message) || e), during: m.type });
  }
};
