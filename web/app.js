// CleanGroove web demo: decode a file, restore it in a worker, then A/B it in sync.
"use strict";

const $ = (id) => document.getElementById(id);
// Samples that differ by more than wavelet round-off (two steps of the engine's 25-bit scale).
const ROUNDOFF = 2.5 / 16777216;

const state = {
  file: null, sampleRate: 0, orig: null, rest: null, diff: null, repairs: [],
  ctx: null, buffers: null, playing: false, startedAt: 0, offset: 0, sources: [], gains: null,
  mode: "rest", peaks: null,
};

// ---------- file loading ----------

/** Sample rate from a WAV or FLAC header, so decoding keeps the file's own rate. */
function sniffRate(bytes) {
  const v = new DataView(bytes);
  const tag = (o) => String.fromCharCode(v.getUint8(o), v.getUint8(o + 1), v.getUint8(o + 2), v.getUint8(o + 3));
  try {
    if (tag(0) === "RIFF" && tag(8) === "WAVE") {
      let o = 12;
      while (o + 8 <= v.byteLength) {
        const id = tag(o), size = v.getUint32(o + 4, true);
        if (id === "fmt ") return v.getUint32(o + 12, true);
        o += 8 + size + (size & 1);
      }
    }
    if (tag(0) === "fLaC") {
      // STREAMINFO follows the 4-byte block header: rate is 20 bits at byte 10.
      const b = (i) => v.getUint8(8 + i);
      return (b(10) << 12) | (b(11) << 4) | (b(12) >> 4);
    }
  } catch (e) { /* fall through */ }
  return 0;
}

function makeContext(rate) {
  if (state.ctx && state.ctx.sampleRate === rate) return state.ctx;
  if (state.ctx) state.ctx.close();
  try {
    state.ctx = rate ? new AudioContext({ sampleRate: rate }) : new AudioContext();
  } catch (e) {
    state.ctx = new AudioContext();
  }
  return state.ctx;
}

async function loadFile(file) {
  stop();
  $("result").classList.add("hidden");
  $("run").disabled = true;
  $("fileinfo").textContent = `Reading ${file.name}...`;
  const bytes = await file.arrayBuffer();
  const rate = sniffRate(bytes);
  const ctx = makeContext(rate);
  let buf;
  try {
    buf = await ctx.decodeAudioData(bytes);
  } catch (e) {
    $("fileinfo").textContent = `This browser could not decode ${file.name}. Try a WAV or FLAC file.`;
    return;
  }
  // Playback and the engine both want two channels: a mono file is played as two.
  if (buf.numberOfChannels !== 2) {
    const two = ctx.createBuffer(2, buf.length, buf.sampleRate);
    two.copyToChannel(buf.getChannelData(0), 0);
    two.copyToChannel(buf.getChannelData(buf.numberOfChannels > 1 ? 1 : 0), 1);
    if (buf.numberOfChannels > 2) {
      $("fileinfo").textContent = "Only the first two channels are used.";
    }
    state.origChannels = buf.numberOfChannels;
    buf = two;
  } else {
    state.origChannels = 2;
  }
  state.file = file;
  state.sampleRate = buf.sampleRate;
  // The decoded buffer is kept as is and played directly: no copies of the original.
  state.origBuf = buf;
  state.orig = [buf.getChannelData(0), buf.getChannelData(1)];
  state.rest = state.diff = state.buffers = null;
  const resampled = rate && rate !== buf.sampleRate;
  $("fileinfo").textContent =
    `${file.name}: ${fmtTime(buf.duration)}, ${buf.sampleRate} Hz, ` +
    `${state.origChannels === 1 ? "mono (played as two channels)" : "stereo"}` +
    (resampled ? `. Note: the browser resampled it from ${rate} Hz.` : "");
  $("run").disabled = false;
  $("status").textContent = "";
}

// ---------- restoring ----------

let worker = null;

function settings() {
  return {
    preset: +$("preset").value, declick: +$("declick").value, decrackle: +$("decrackle").value,
    method: +$("method").value, pitch: $("pitch").checked, reverse: $("reverse").checked,
    engine: $("enhanced").checked ? 1 : 0,
  };
}

function run() {
  if (!state.orig) return;
  stop();
  // A fresh worker per job: WebAssembly memory only grows, so ending the worker is what frees it.
  if (worker) worker.terminate();
  worker = new Worker("worker.js");
  // Drop the last result before the next one is made.
  state.rest = state.diff = state.buffers = null;
  const total = state.orig[0].length;
  const duration = total / state.sampleRate;
  $("run").disabled = true;
  $("progress").classList.remove("hidden");
  const fill = $("progress").firstElementChild;
  fill.style.width = "0";
  $("status").textContent = "Loading the engine...";
  worker.onmessage = (ev) => {
    const m = ev.data;
    if (m.type === "progress") {
      fill.style.width = `${(100 * m.done / m.total).toFixed(1)}%`;
      const speed = (m.done / state.sampleRate) / Math.max(m.seconds, 1e-3);
      $("status").textContent = `Restoring... ${Math.round(100 * m.done / m.total)}%, ${speed.toFixed(1)}x real time`;
    } else if (m.type === "done") {
      fill.style.width = "100%";
      $("run").disabled = false;
      worker.terminate();
      worker = null;
      const speed = duration / Math.max(m.seconds, 1e-3);
      $("status").textContent = `Done in ${m.seconds.toFixed(1)} s (${speed.toFixed(1)}x real time).`;
      finished(m.left, m.right, m.seconds);
    } else if (m.type === "error") {
      $("run").disabled = false;
      $("status").textContent = `Something went wrong: ${m.message}`;
    }
  };
  // Copies go to the worker; the originals stay here for playback.
  const l = new Float32Array(state.orig[0]), r = new Float32Array(state.orig[1]);
  worker.postMessage({ type: "run", sampleRate: state.sampleRate, left: l, right: r, settings: settings() }, [l.buffer, r.buffer]);
}

function finished(left, right, seconds) {
  const ctx = state.ctx;
  const n = left.length;
  // The result and the difference live only in their playback buffers.
  const restBuf = ctx.createBuffer(2, n, state.sampleRate);
  restBuf.copyToChannel(left, 0);
  restBuf.copyToChannel(right, 1);
  left = restBuf.getChannelData(0);
  right = restBuf.getChannelData(1);
  state.rest = [left, right];
  const diffBuf = ctx.createBuffer(2, n, state.sampleRate);
  const dl = diffBuf.getChannelData(0), dr = diffBuf.getChannelData(1);
  // Repairs: runs of changed samples in either channel, merged when under 1 ms apart.
  const gap = Math.round(state.sampleRate / 1000);
  const repairs = [];
  let changed = 0, runStart = -1, lastHit = -1e9;
  const [ol, or] = state.orig;
  for (let i = 0; i < n; i++) {
    const a = ol[i] - left[i], b = or[i] - right[i];
    dl[i] = a; dr[i] = b;
    const hitL = Math.abs(a) > ROUNDOFF, hitR = Math.abs(b) > ROUNDOFF;
    if (hitL) changed++;
    if (hitR) changed++;
    if (hitL || hitR) {
      if (i - lastHit > gap) {
        if (runStart >= 0) repairs.push([runStart, lastHit + 1]);
        runStart = i;
      }
      lastHit = i;
    }
  }
  if (runStart >= 0) repairs.push([runStart, lastHit + 1]);
  state.diff = [dl, dr];
  state.repairs = repairs;

  state.buffers = { orig: state.origBuf, rest: restBuf, diff: diffBuf };
  state.offset = 0;
  state.peaks = null;

  const dur = n / state.sampleRate;
  const longest = repairs.reduce((m, [a, b]) => Math.max(m, b - a), 0);
  $("stats").innerHTML = [
    ["Repairs", repairs.length.toLocaleString()],
    ["Per second", (repairs.length / dur).toFixed(1)],
    ["Samples changed", `1 in ${changed ? Math.round(2 * n / changed).toLocaleString() : "none"}`],
    ["Longest repair", `${(1000 * longest / state.sampleRate).toFixed(1)} ms`],
    ["Engine time", `${seconds.toFixed(1)} s`],
  ].map(([k, v]) => `<div class="stat"><span class="muted">${k}</span><b>${v}</b></div>`).join("");
  $("result").classList.remove("hidden");
  draw();
}

// ---------- playback ----------

function position() {
  if (!state.buffers) return 0;
  const dur = state.buffers.orig.duration;
  const t = state.playing ? state.offset + state.ctx.currentTime - state.startedAt : state.offset;
  return Math.min(Math.max(t, 0), dur);
}

function setMode(mode) {
  state.mode = mode;
  document.querySelectorAll(".seg button").forEach((b) => b.classList.toggle("on", b.dataset.mode === mode));
  if (state.gains) {
    const t = state.ctx.currentTime;
    // A 5 ms crossfade, so switching never clicks by itself.
    for (const [k, g] of Object.entries(state.gains)) g.gain.setTargetAtTime(k === mode ? 1 : 0, t, 0.005);
  }
}

function play(from) {
  if (!state.buffers) return;
  stop(true);
  const ctx = state.ctx;
  if (ctx.state === "suspended") ctx.resume();
  state.offset = from ?? state.offset;
  if (state.offset >= state.buffers.orig.duration - 0.01) state.offset = 0;
  const when = ctx.currentTime + 0.05;
  state.gains = {};
  state.sources = [];
  for (const k of ["orig", "rest", "diff"]) {
    const src = ctx.createBufferSource();
    src.buffer = state.buffers[k];
    const g = ctx.createGain();
    g.gain.value = k === state.mode ? 1 : 0;
    src.connect(g).connect(ctx.destination);
    src.start(when, state.offset);
    state.sources.push(src);
    state.gains[k] = g;
  }
  state.sources[0].onended = () => { if (state.playing && position() >= state.buffers.orig.duration - 0.05) { stop(); state.offset = 0; } };
  state.startedAt = when;
  state.playing = true;
  $("play").textContent = "Pause";
  tick();
}

function stop(keepPosition) {
  if (state.playing) state.offset = position();
  for (const s of state.sources) { try { s.stop(); } catch (e) {} }
  state.sources = [];
  state.gains = null;
  state.playing = false;
  if (!keepPosition) $("play") && ($("play").textContent = "Play");
}

function tick() {
  draw();
  if (state.playing) requestAnimationFrame(tick);
  else $("play").textContent = "Play";
}

// ---------- drawing ----------

function css(name) { return getComputedStyle(document.documentElement).getPropertyValue(name).trim(); }

function sizeCanvas(c) {
  const dpr = window.devicePixelRatio || 1;
  const w = Math.round(c.clientWidth * dpr), h = Math.round(c.clientHeight * dpr);
  if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
  return [w, h];
}

function draw() {
  if (!state.orig || !state.rest) return;
  drawOverview();
  const dur = state.buffers.orig.duration;
  $("clock").textContent = `${fmtTime(position())} / ${fmtTime(dur)}`;
}

function drawOverview() {
  const c = $("wave"), g = c.getContext("2d");
  const [w, h] = sizeCanvas(c);
  const n = state.orig[0].length;
  if (!state.peaks || state.peaks.w !== w) {
    const mins = new Float32Array(w), maxs = new Float32Array(w), hits = new Uint8Array(w);
    const [l, r] = state.orig;
    for (let x = 0; x < w; x++) {
      const a = Math.floor(x * n / w), b = Math.max(a + 1, Math.floor((x + 1) * n / w));
      let lo = 0, hi = 0;
      for (let i = a; i < b; i++) { const s = (l[i] + r[i]) * 0.5; if (s < lo) lo = s; if (s > hi) hi = s; }
      mins[x] = lo; maxs[x] = hi;
    }
    // Repairs per pixel column, shaded against a busy column (the 95th percentile), so a long
    // file shows where the damage is instead of one solid bar; a lone repair stays visible.
    const counts = new Uint32Array(w);
    for (const [a] of state.repairs) counts[Math.min(w - 1, Math.floor(a * w / n))]++;
    const busy = Array.from(counts).filter((c) => c > 0).sort((x, y) => x - y);
    const ref = busy.length ? busy[Math.floor(0.95 * (busy.length - 1))] : 1;
    for (let x = 0; x < w; x++) hits[x] = counts[x] ? Math.round(255 * Math.min(1, 0.2 + 0.8 * Math.sqrt(counts[x] / ref))) : 0;
    state.peaks = { w, mins, maxs, hits };
  }
  const p = state.peaks;
  g.clearRect(0, 0, w, h);
  g.fillStyle = css("--wave");
  const mid = h * 0.55, amp = h * 0.42;
  for (let x = 0; x < w; x++) g.fillRect(x, mid - p.maxs[x] * amp, 1, Math.max(1, (p.maxs[x] - p.mins[x]) * amp));
  g.fillStyle = css("--repair");
  for (let x = 0; x < w; x++) {
    if (!p.hits[x]) continue;
    g.globalAlpha = p.hits[x] / 255;
    g.fillRect(x, 0, 1, h * 0.12);
  }
  g.globalAlpha = 1;
  const px = position() / state.buffers.orig.duration * w;
  g.fillStyle = css("--head");
  g.fillRect(Math.round(px), 0, Math.max(1, Math.round(window.devicePixelRatio || 1)), h);
}

// ---------- export ----------

function wav24(left, right, rate) {
  const n = left.length, bytes = 44 + n * 6;
  const buf = new ArrayBuffer(bytes), v = new DataView(buf);
  const str = (o, s) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
  str(0, "RIFF"); v.setUint32(4, bytes - 8, true); str(8, "WAVE");
  str(12, "fmt "); v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 2, true);
  v.setUint32(24, rate, true); v.setUint32(28, rate * 6, true); v.setUint16(32, 6, true); v.setUint16(34, 24, true);
  str(36, "data"); v.setUint32(40, n * 6, true);
  let o = 44;
  for (let i = 0; i < n; i++) {
    for (const ch of [left, right]) {
      let s = Math.round(ch[i] * 8388608);
      s = Math.max(-8388607, Math.min(8388607, s));
      v.setUint8(o, s & 255); v.setUint8(o + 1, (s >> 8) & 255); v.setUint8(o + 2, (s >> 16) & 255);
      o += 3;
    }
  }
  return new Blob([buf], { type: "audio/wav" });
}

function download() {
  if (!state.rest) return;
  const url = URL.createObjectURL(wav24(state.rest[0], state.rest[1], state.sampleRate));
  const a = document.createElement("a");
  a.href = url;
  a.download = state.file.name.replace(/\.[^.]+$/, "") + "_restored.wav";
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 10000);
}

// ---------- helpers and wiring ----------

function fmtTime(s, ms) {
  const m = Math.floor(s / 60), r = s - 60 * m;
  return ms ? `${m}:${r.toFixed(3).padStart(6, "0")}` : `${m}:${String(Math.floor(r)).padStart(2, "0")}`;
}

const drop = $("drop");
drop.onclick = () => $("file").click();
drop.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); $("file").click(); } };
$("file").onchange = (e) => e.target.files[0] && loadFile(e.target.files[0]);
drop.ondragover = (e) => { e.preventDefault(); drop.classList.add("over"); };
drop.ondragleave = () => drop.classList.remove("over");
drop.ondrop = (e) => { e.preventDefault(); drop.classList.remove("over"); e.dataTransfer.files[0] && loadFile(e.dataTransfer.files[0]); };

for (const id of ["declick", "decrackle"]) $(id).oninput = () => ($(id + "v").textContent = $(id).value);
$("preset").onchange = () => {
  // Starting points per record type; adjust by ear.
  const p = { 0: [30, 0, true], 1: [30, 0, true], 2: [50, 50, false] }[$("preset").value];
  $("declick").value = p[0]; $("decrackle").value = p[1]; $("pitch").checked = p[2];
  $("declickv").textContent = p[0]; $("decracklev").textContent = p[1];
};
$("run").onclick = run;
$("play").onclick = () => (state.playing ? stop() : play());
document.querySelectorAll(".seg button").forEach((b) => (b.onclick = () => setMode(b.dataset.mode)));
$("download").onclick = download;
$("nextrepair").onclick = () => {
  const now = Math.round(position() * state.sampleRate) + Math.round(state.sampleRate * 0.03);
  const next = state.repairs.find(([a]) => a > now) || state.repairs[0];
  if (!next) return;
  const t = Math.max(0, next[0] / state.sampleRate - 0.75);
  if (state.playing) play(t); else { state.offset = next[0] / state.sampleRate; draw(); }
};
$("wave").onpointerdown = (e) => {
  if (!state.buffers) return;
  const r = e.currentTarget.getBoundingClientRect();
  const t = (e.clientX - r.left) / r.width * state.buffers.orig.duration;
  if (state.playing) play(t); else { state.offset = t; draw(); }
};
window.addEventListener("resize", () => { state.peaks = null; draw(); });

// ?src=URL loads a file from the same site (for example a sample recording placed next to the page).
const src = new URLSearchParams(location.search).get("src");
if (src) {
  fetch(src).then((r) => {
    if (!r.ok) throw new Error(r.status);
    return r.blob();
  }).then((b) => loadFile(new File([b], decodeURIComponent(src.split("/").pop())))).catch(() => {
    $("fileinfo").textContent = `Could not load ${src}.`;
  });
}
