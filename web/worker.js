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

async function engine() {
  if (x) return x;
  const imports = { env: { cg_progress: (stage, f) => self.postMessage({ type: "progress", stage, f }) } };
  const url = new URL("cr_wasm.wasm", self.location);
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
const f32 = (ptr, n) => new Float32Array(mem(), ptr, n);

function applySettings(values) {
  if (!values) return;
  lastSettings = values;
  new Float64Array(mem(), x.cg_settings_buffer(), values.length).set(values);
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
      const ptr = x.cg_analyze(s);
      const json = new TextDecoder().decode(new Uint8Array(mem(), ptr, x.cg_text_len(s)));
      self.postMessage({ type: "analysis", analysis: JSON.parse(json), seconds: (performance.now() - t0) / 1000 });
      // The analysis feeds balance and azimuth: settings are applied again with it known.
      applySettings(lastSettings);
    } else if (m.type === "range") {
      applySettings(m.settings);
      const t0 = performance.now();
      const count = x.cg_range(s, m.start, m.len);
      const n = x.cg_range_len(s);
      const l = new Float32Array(f32(x.cg_range_out(s, 0), n));
      const r = new Float32Array(f32(x.cg_range_out(s, 1), n));
      const markers = new Uint32Array(new Uint32Array(mem(), x.cg_range_markers(s), count * 3));
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
      const n = x.cg_render_finish(s);
      const l = new Float32Array(f32(x.cg_render_out(s, 0), n));
      const r = new Float32Array(f32(x.cg_render_out(s, 1), n));
      const levels = Array.from(new Float64Array(mem(), x.cg_render_levels(s), 4));
      self.postMessage({ type: "rendered", l, r, levels, seconds: (performance.now() - t0) / 1000 }, [l.buffer, r.buffer]);
    }
  } catch (e) {
    self.postMessage({ type: "error", message: String((e && e.message) || e), during: m.type });
  }
};
