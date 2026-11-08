// CleanGroove Web: the desktop app's File Mode in the browser.
"use strict";

const $ = (id) => document.getElementById(id);
const PREVIEW_MAX_SECONDS = 30;   // repairs are previewed on the waveform at this zoom or closer (as the desktop)
const PEAK_BLOCK = 256;           // frames per precomputed min/max block

const st = {
  file: null, sr: 0, total: 0, chans: 2, orig: null, peaks: null,
  view: { a: 0, b: 0 }, cursor: 0, playhead: 0, playFrom: 0, playing: false, mode: 0,
  analysis: null, range: null, rangeId: 0, rangeTimer: 0, busy: null,
  ctx: null, node: null, worker: null, ready: false,
};

// ---------- settings ----------

const SETTINGS_IDS = ["post_on", "flip", "balance_on", "azimuth_on", "subsonic_on", "elliptical_on", "mono_sum", "dethump"];

function settings() {
  const c = (id) => ($(id).checked ? 1 : 0);
  const ellHz = st.analysis ? st.analysis.post.elliptical_hz : 100;
  return [
    +$("declickn").value, +$("decracklen").value, +$("method").value, c("pitch"), c("reverse"), +$("engine").value,
    c("post_on"), c("flip"), c("balance_on"), c("azimuth_on"), c("subsonic_on"), c("elliptical_on"), ellHz, c("mono_sum"), c("dethump"),
  ];
}

/** Any setting changed: the preview hears it at once, the waveform preview follows shortly. */
function settingsChanged() {
  $("elliptical_on").disabled = $("mono_sum").checked;
  updateSuggestMatch();
  if (!st.ready) return;
  if (st.playing) st.node.port.postMessage({ type: "restart", settings: settings() });
  else st.worker.postMessage({ type: "settings", settings: settings() });
  scheduleRange();
}

function setValue(id, v) {
  $(id).value = v;
  $(id + "n").value = v;
}

function bindSlider(id) {
  $(id).oninput = () => { $(id + "n").value = $(id).value; settingsChanged(); };
  $(id + "n").onchange = () => {
    const v = Math.max(0, Math.min(100, Math.round(+$(id + "n").value || 0)));
    setValue(id, v);
    settingsChanged();
  };
}

function applyPreset(name) {
  // The desktop's presets: DeClick, DeCrackle, Reverse, Stereo>Mono; Pitch Protection on.
  const p = { lp: [30, 0, false, false], 78: [70, 45, false, true], brass: [40, 15, true, false] }[name];
  if (!p) return;
  setValue("declick", p[0]);
  setValue("decrackle", p[1]);
  $("reverse").checked = p[2];
  $("mono_sum").checked = p[3];
  $("pitch").checked = true;
  settingsChanged();
  status(`Preset: ${$("preset").selectedOptions[0].textContent}`);
}

// ---------- loading ----------

/** Sample rate and bit depth from a WAV or FLAC header, so decoding keeps the file's own rate. */
function sniffRate(bytes) {
  const v = new DataView(bytes);
  const tag = (o) => String.fromCharCode(v.getUint8(o), v.getUint8(o + 1), v.getUint8(o + 2), v.getUint8(o + 3));
  try {
    if (tag(0) === "RIFF" && tag(8) === "WAVE") {
      for (let o = 12; o + 8 <= v.byteLength;) {
        const size = v.getUint32(o + 4, true);
        if (tag(o) === "fmt ") return [v.getUint32(o + 12, true), v.getUint16(o + 22, true)];
        o += 8 + size + (size & 1);
      }
    }
    if (tag(0) === "fLaC") {
      const b = (i) => v.getUint8(8 + i);
      return [(b(10) << 12) | (b(11) << 4) | (b(12) >> 4), (((b(12) & 1) << 4) | (b(13) >> 4)) + 1];
    }
  } catch (e) { /* unknown format */ }
  return [0, 0];
}

async function ensureAudio(rate) {
  if (st.ctx && st.ctx.sampleRate === rate) return;
  if (st.ctx) st.ctx.close();
  try {
    st.ctx = new AudioContext({ sampleRate: rate, latencyHint: "playback" });
  } catch (e) {
    st.ctx = new AudioContext({ latencyHint: "playback" });
  }
  await st.ctx.audioWorklet.addModule("player.js");
  st.node = new AudioWorkletNode(st.ctx, "cleangroove-player", { outputChannelCount: [2] });
  st.node.connect(st.ctx.destination);
  st.node.port.onmessage = (ev) => onPlayer(ev.data);
  st.node.port.postMessage({ type: "mode", mode: st.mode });
  const ch = new MessageChannel();
  st.node.port.postMessage({ type: "port", port: ch.port1 }, [ch.port1]);
  st.worker.postMessage({ type: "port", port: ch.port2 }, [ch.port2]);
}

async function loadFile(file) {
  pause();
  status(`Reading ${file.name}...`);
  const bytes = await file.arrayBuffer();
  const [rate, bits] = sniffRate(bytes);
  if (!st.worker) startWorker();
  await ensureAudio(rate || 48000);
  let buf;
  try {
    buf = await st.ctx.decodeAudioData(bytes);
  } catch (e) {
    status(`This browser could not decode ${file.name}. Try a WAV or FLAC file.`, true);
    return;
  }
  if (buf.sampleRate !== st.ctx.sampleRate) await ensureAudio(buf.sampleRate);
  const left = buf.getChannelData(0);
  const right = buf.numberOfChannels > 1 ? buf.getChannelData(1) : left;
  Object.assign(st, {
    file, sr: buf.sampleRate, total: buf.length, chans: buf.numberOfChannels, bits,
    orig: [left, right], analysis: null, range: null, cursor: 0, playhead: 0, ready: false,
  });
  st.peaks = buildPeaks(st.orig);
  st.view = { a: 0, b: Math.min(st.total, st.sr * 15) };
  resetAnalysisPanel();
  $("drop").classList.add("hidden");
  const resampled = rate && rate !== buf.sampleRate;
  $("trackline").innerHTML =
    `<span class="label">Track:</span> <b>${esc(file.name)}</b> <span class="sep">|</span> <span class="mono">${clock(st.total / st.sr, true)}</span>` +
    ` <span class="sep">|</span> <span class="mono">${(st.sr / 1000).toFixed(1)} kHz</span>` +
    (bits ? ` <span class="sep">|</span> <span class="mono">${bits}-bit</span>` : "") +
    ` <span class="sep">|</span> <span class="mono">${st.chans === 1 ? "mono" : "stereo"}</span>` +
    (resampled ? ` <span class="warn small">resampled from ${rate} Hz by the browser</span>` : "");
  $("framesinfo").textContent = `| ${st.total.toLocaleString()} frames`;
  status("Loading the engine...");
  // The worker gets its own copy; this page keeps the decoded audio for drawing.
  const l = new Float32Array(left), r = new Float32Array(right);
  st.worker.postMessage({ type: "load", sampleRate: st.sr, left: l, right: r, settings: settings() }, [l.buffer, r.buffer]);
  draw(true);
}

function startWorker() {
  st.worker = new Worker("worker.js");
  st.worker.onmessage = (ev) => onWorker(ev.data);
}

function onWorker(m) {
  if (m.type === "loaded") {
    st.ready = true;
    ["render", "analyze"].forEach((id) => ($(id).disabled = false));
    status(`Loaded: ${st.file.name} (${st.total.toLocaleString()} frames, ${st.sr} Hz). Play it, or Analyze Album to grade the record.`);
    scheduleRange(0);
  } else if (m.type === "progress") {
    if (m.stage === 1) setBar($("aprogress").firstElementChild, m.f);
    else {
      setBar($("modalbar"), m.f);
      if (m.seconds) $("modalstat").textContent = `${((m.f / 0.95) * st.total / st.sr / Math.max(m.seconds, 1e-3)).toFixed(1)}x real time`;
    }
  } else if (m.type === "analysis") {
    st.busy = null;
    $("aprogress").classList.add("hidden");
    $("analyze").disabled = false;
    $("analyze").textContent = "Re-analyze";
    st.analysis = m.analysis;
    showAnalysis();
    status(`Analysis complete in ${m.seconds.toFixed(1)} s: ${m.analysis.condition}. ${m.analysis.spots.length} problem spots.`);
    scheduleRange(0);
    draw(true);
  } else if (m.type === "range") {
    if (m.id !== st.rangeId) return;
    st.range = { a: m.start, l: m.l, r: m.r, markers: m.markers };
    $("viewstat").textContent = `${m.markers.length / 3} repairs in view`;
    draw(true);
  } else if (m.type === "rendered") {
    st.busy = null;
    $("modal").classList.add("hidden");
    download(m.l, m.r, m.seconds, m.levels);
  } else if (m.type === "error") {
    st.busy = null;
    $("modal").classList.add("hidden");
    $("aprogress").classList.add("hidden");
    $("analyze").disabled = false;
    status(`Something went wrong (${m.during}): ${m.message}`, true);
  }
}

// ---------- playback ----------

function onPlayer(m) {
  if (m.type === "pos") {
    st.playhead = m.frame;
    if (st.playing) follow();
    draw(false);
  } else if (m.type === "ended") {
    st.playing = false;
    st.playhead = st.cursor = st.total;
    $("play").classList.remove("on");
    draw(false);
  }
}

async function play() {
  if (!st.ready || st.busy || st.playing) return;
  await st.ctx.resume();
  if (st.cursor >= st.total) st.cursor = 0;
  st.playFrom = st.cursor;
  st.playhead = st.cursor;
  st.playing = true;
  $("play").classList.add("on");
  st.node.port.postMessage({ type: "play", start: st.cursor, settings: settings() });
}

function pause() {
  if (!st.playing) return;
  st.playing = false;
  st.node.port.postMessage({ type: "pause" });
  st.cursor = st.playhead;
  $("play").classList.remove("on");
  draw(false);
}

function stop() {
  pause();
  st.cursor = st.playhead = st.playFrom;
  draw(false);
}

function seek(frame) {
  st.cursor = st.playhead = Math.max(0, Math.min(st.total, Math.round(frame)));
  if (st.playing) st.node.port.postMessage({ type: "seek", start: st.cursor, settings: settings() });
  draw(false);
}

function setMode(mode) {
  st.mode = mode;
  document.querySelectorAll("#modes button").forEach((b) => b.classList.toggle("on", +b.dataset.mode === mode));
  if (st.node) st.node.port.postMessage({ type: "mode", mode });
  draw(true);
}

/** While playing, page the view forward when the playhead leaves it (as the desktop does). */
function follow() {
  const { a, b } = st.view, w = b - a;
  if (st.playhead > b || st.playhead < a) {
    const na = Math.max(0, Math.min(st.total - w, st.playhead - w * 0.05));
    st.view = { a: na, b: na + w };
    scheduleRange();
    draw(true);
  }
}

// ---------- waveform preview of the view ----------

function scheduleRange(delay = 250) {
  clearTimeout(st.rangeTimer);
  if (!st.ready) return;
  const len = st.view.b - st.view.a;
  if (len > PREVIEW_MAX_SECONDS * st.sr) {
    st.range = null;
    $("viewstat").textContent = `Zoom in to ${PREVIEW_MAX_SECONDS} s or less to preview repairs`;
    return;
  }
  st.rangeTimer = setTimeout(() => {
    if (st.busy) return;
    st.rangeId++;
    st.worker.postMessage({ type: "range", id: st.rangeId, start: Math.floor(st.view.a), len: Math.ceil(len), settings: settings() });
  }, delay);
}

// ---------- drawing ----------

function css(name) { return getComputedStyle(document.documentElement).getPropertyValue(name).trim(); }

function fit(c) {
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(1, Math.round(c.clientWidth * dpr)), h = Math.max(1, Math.round(c.clientHeight * dpr));
  if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
  return [w, h, dpr];
}

/** Min/max per PEAK_BLOCK frames for each channel: fast drawing of long spans. */
function buildPeaks([l, r]) {
  const n = Math.ceil(l.length / PEAK_BLOCK);
  const p = [new Float32Array(2 * n), new Float32Array(2 * n)];
  for (const [k, ch] of [[0, l], [1, r]]) {
    for (let b = 0; b < n; b++) {
      let lo = 0, hi = 0;
      for (let i = b * PEAK_BLOCK, e = Math.min(ch.length, i + PEAK_BLOCK); i < e; i++) {
        const v = ch[i];
        if (v < lo) lo = v;
        if (v > hi) hi = v;
      }
      p[k][2 * b] = lo; p[k][2 * b + 1] = hi;
    }
  }
  return p;
}

/** Min/max of the original channel k over frames a..b, from blocks when the span is long. */
function minmaxOrig(k, a, b) {
  a = Math.max(0, Math.floor(a)); b = Math.min(st.total, Math.ceil(b));
  if (b - a > PEAK_BLOCK * 4) {
    const p = st.peaks[k];
    let lo = 0, hi = 0;
    for (let i = Math.floor(a / PEAK_BLOCK), e = Math.ceil(b / PEAK_BLOCK); i < e; i++) {
      if (p[2 * i] < lo) lo = p[2 * i];
      if (p[2 * i + 1] > hi) hi = p[2 * i + 1];
    }
    return [lo, hi];
  }
  return minmaxSlice(st.orig[k], a, b);
}

function minmaxSlice(data, a, b) {
  a = Math.max(0, Math.floor(a)); b = Math.min(data.length, Math.ceil(b));
  let lo = 0, hi = 0;
  for (let i = a; i < b; i++) { const v = data[i]; if (v < lo) lo = v; if (v > hi) hi = v; }
  return [lo, hi];
}

let waveLayer = null;

function draw(full) {
  if (!st.orig) return;
  drawOverview();
  drawRuler();
  if (full || !waveLayer) waveLayer = renderWave();
  const c = $("wave"), g = c.getContext("2d");
  const [w, h, dpr] = fit(c);
  g.clearRect(0, 0, w, h);
  g.drawImage(waveLayer, 0, 0);
  const x = (f) => ((f - st.view.a) / (st.view.b - st.view.a)) * w;
  g.fillStyle = css("--accent");
  g.fillRect(Math.round(x(st.cursor)), 0, dpr, h);
  if (st.playing) {
    g.fillStyle = "#ffffff";
    g.fillRect(Math.round(x(st.playhead)), 0, Math.max(1, dpr * 1.5), h);
  }
  const at = st.playing ? st.playhead : st.cursor;
  $("cursor").textContent = `${clock(at / st.sr, true)} (${Math.round(at).toLocaleString()} samples)`;
}

function renderWave() {
  const c = $("wave");
  const [w, h, dpr] = fit(c);
  const layer = document.createElement("canvas");
  layer.width = w; layer.height = h;
  const g = layer.getContext("2d");
  g.fillStyle = css("--wave-bg");
  g.fillRect(0, 0, w, h);
  const lane = h / 2, { a, b } = st.view, span = b - a;
  const r = st.range && st.range.a <= a + 1 && st.range.a + st.range.l.length >= b - 1 ? st.range : null;
  const origColor = css("--bypass"), outColor = css("--repaired"), deltaColor = css("--delta");
  for (let k = 0; k < 2; k++) {
    const top = k * lane, mid = top + lane / 2, amp = lane * 0.44;
    g.fillStyle = css("--grid");
    g.fillRect(0, top + lane - dpr, w, dpr);
    g.fillRect(0, mid, w, Math.max(1, dpr * 0.6));
    const out = r ? (k === 0 ? r.l : r.r) : null;
    // Original underneath, then the restored output (or the removed noise) on top, as the desktop.
    const passes = [["orig", origColor]];
    if (out && st.mode === 0) passes.push(["out", outColor]);
    if (out && st.mode === 2) passes.push(["delta", deltaColor]);
    for (const [kind, color] of passes) {
      g.fillStyle = color;
      for (let px = 0; px < w; px++) {
        const fa = a + (px / w) * span, fb = a + ((px + 1) / w) * span;
        let lo = 0, hi = 0;
        if (kind === "orig") [lo, hi] = minmaxOrig(k, fa, fb);
        else if (kind === "out") [lo, hi] = minmaxSlice(out, fa - r.a, fb - r.a);
        else {
          const o = st.orig[k];
          for (let i = Math.floor(fa); i < Math.ceil(fb); i++) {
            const d = o[i] - out[i - r.a];
            if (d < lo) lo = d;
            if (d > hi) hi = d;
          }
        }
        g.fillRect(px, mid - hi * amp, 1, Math.max(dpr * 0.6, (hi - lo) * amp));
      }
    }
    // Repair pins along the top of each lane.
    if (r) {
      g.fillStyle = outColor;
      const m = r.markers;
      for (let i = 0; i < m.length; i += 3) {
        if (m[i + 2] !== k) continue;
        const px = ((m[i] - a) / span) * w;
        g.fillRect(Math.round(px) - dpr, top + 3 * dpr, 3 * dpr, 6 * dpr);
      }
    }
    g.font = `${11 * dpr}px ${css("--mono")}`;
    g.fillStyle = "rgba(30, 41, 59, .9)";
    g.fillRect(8 * dpr, top + 12 * dpr, 50 * dpr, 18 * dpr);
    g.fillStyle = css("--text2");
    g.fillText(k === 0 ? "LEFT" : "RIGHT", 13 * dpr, top + 25 * dpr);
  }
  // Legend, top right.
  g.font = `${11 * dpr}px ${css("--mono")}`;
  const legend = !r ? [["Original", origColor]]
    : st.mode === 2 ? [["Original", origColor], ["Removed Noise", deltaColor]]
    : st.mode === 1 ? [["Original (playing)", origColor]]
    : [["Original", origColor], ["Repaired Output", outColor]];
  let lx = w - 12 * dpr;
  for (let i = legend.length - 1; i >= 0; i--) {
    const [t, col] = legend[i];
    lx -= g.measureText(t).width;
    g.fillStyle = css("--text2"); g.fillText(t, lx, 26 * dpr);
    lx -= 12 * dpr;
    g.fillStyle = col; g.beginPath(); g.arc(lx + 4 * dpr, 22 * dpr, 4 * dpr, 0, 7); g.fill();
    lx -= 14 * dpr;
  }
  return layer;
}

function drawOverview() {
  const c = $("overview"), g = c.getContext("2d");
  const [w, h, dpr] = fit(c);
  g.fillStyle = css("--wave-bg");
  g.fillRect(0, 0, w, h);
  const n = st.total, an = st.analysis;
  if (an && an.block_exposed.length) {
    // Heat: exposed clicks per second, against a busy second (the 95th percentile).
    const be = an.block_exposed, sorted = be.filter((v) => v > 0).sort((p, q) => p - q);
    const ref = sorted.length ? sorted[Math.floor(0.95 * (sorted.length - 1))] : 1;
    for (let px = 0; px < w; px++) {
      const b0 = Math.floor((px / w) * be.length), b1 = Math.max(b0 + 1, Math.floor(((px + 1) / w) * be.length));
      let v = 0;
      for (let i = b0; i < b1; i++) v = Math.max(v, be[i] || 0);
      const t = Math.min(1, Math.sqrt(v / ref));
      g.fillStyle = `rgb(${Math.round(55 + 200 * t)},${Math.round(65 + 190 * t)},${Math.round(140 + 115 * t)})`;
      g.fillRect(px, 0, 1, h);
    }
    g.fillStyle = css("--warn");
    for (const s of an.spots) {
      const px = (s.start / n) * w;
      g.beginPath(); g.moveTo(px - 4 * dpr, 0); g.lineTo(px + 4 * dpr, 0); g.lineTo(px, 7 * dpr); g.fill();
    }
  } else {
    g.fillStyle = "#3b4a7a";
    for (let px = 0; px < w; px++) {
      const [lo, hi] = minmaxOrig(0, (px / w) * n, ((px + 1) / w) * n);
      g.fillRect(px, h / 2 - hi * h * 0.45, 1, Math.max(1, (hi - lo) * h * 0.45));
    }
  }
  const x0 = (st.view.a / n) * w, x1 = (st.view.b / n) * w;
  g.fillStyle = "rgba(99, 102, 241, .28)";
  g.fillRect(x0, 0, Math.max(2, x1 - x0), h);
  g.strokeStyle = "#818cf8"; g.lineWidth = dpr;
  g.strokeRect(x0 + 0.5, 0.5, Math.max(2, x1 - x0) - 1, h - 1);
  g.fillStyle = "#ffffff";
  g.fillRect(((st.playing ? st.playhead : st.cursor) / n) * w, 0, dpr, h);
}

function drawRuler() {
  const c = $("ruler"), g = c.getContext("2d");
  const [w, h, dpr] = fit(c);
  g.fillStyle = css("--panel2");
  g.fillRect(0, 0, w, h);
  const { a, b } = st.view, secs = (b - a) / st.sr;
  const steps = [0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600];
  const step = steps.find((s) => (secs / s) * 110 * dpr <= w) || 600;
  g.fillStyle = css("--label");
  g.font = `${10.5 * dpr}px ${css("--mono")}`;
  for (let t = Math.ceil(a / st.sr / step) * step; t * st.sr <= b; t += step) {
    const px = ((t * st.sr - a) / (b - a)) * w;
    g.fillRect(px, h - 6 * dpr, dpr, 6 * dpr);
    g.fillText(clock(t, true), px + 3 * dpr, 13 * dpr);
  }
}

// ---------- view ----------

function setView(a, b) {
  const minSpan = Math.max(64, st.sr * 0.01);
  const span = Math.max(minSpan, Math.min(st.total, b - a));
  a = Math.max(0, Math.min(st.total - span, a));
  st.view = { a, b: a + span };
  scheduleRange();
  draw(true);
}

function zoom(factor, centre) {
  const { a, b } = st.view;
  const c = centre ?? (st.cursor >= a && st.cursor <= b ? st.cursor : (a + b) / 2);
  setView(c - (c - a) * factor, c + (b - c) * factor);
}

// ---------- record analysis ----------

function resetAnalysisPanel() {
  $("acol").innerHTML = `<p class="label">Analyze the album to grade the record's condition, map where its clicks are, measure its channels and suggest settings. Takes about as long as a render.</p>`;
  ["suggest", "recording", "spots"].forEach((id) => $(id).classList.add("hidden"));
  $("analyze").textContent = "Analyze Album";
  for (const id of ["balance", "azimuth"]) {
    $(id + "_on").disabled = true; $(id + "_on").checked = false; $(id + "_l").classList.add("off");
  }
  $("dethump_l").classList.add("hidden");
  $("dethump").checked = false;
}

function gradeColor(g) { return ["--good", "--good", "--warn", "--bad"][g] || "--text"; }

function showAnalysis() {
  const a = st.analysis, s = a.suggested, p = a.post, ms = a.spots;
  $("acol").innerHTML =
    `<h4>CLICK CONDITION</h4><div class="grade" style="color:var(${gradeColor(a.grade)})">${esc(a.condition)}${a.estimated ? " (estimated)" : ""}</div>` +
    `<p>${a.audible_per_sec.toFixed(1)} audible clicks/s, ${a.pops_per_sec.toFixed(1)} loud pops/s</p>` +
    `<p class="dim small">${a.estimated ? `No silence between tracks: counted in the quietest ${a.grade_seconds.toFixed(0)} s of music, so it may under-read` : `Measured in ${a.grade_seconds.toFixed(0)} s of silence between tracks`}</p>` +
    (a.long_repairs ? `<p class="warn small">${a.long_repairs} long repairs (possible scratches)</p>` : "") +
    (a.thumps ? `<p class="warn small">Needle thumps: ${a.thumps} in ${a.thump_series} series (repair in Corrections)</p>` : "");

  const methods = ["Simple", "Wavelet", "WaveletX2"];
  const fixes = [];
  if (p.flip) fixes.push("flip polarity");
  if (p.balance_on) fixes.push(`balance ${sign(a.balance_db, 1)} dB`);
  if (p.azimuth_on) fixes.push(`azimuth ${sign(a.azimuth_us, 1)} us`);
  if (p.mono_sum) fixes.push("Stereo>Mono");
  else if (p.elliptical_on) fixes.push(`elliptical ${p.elliptical_hz.toFixed(0)} Hz`);
  fixes.push("subsonic 18 Hz");
  const pk = a.pickup_dcl;
  const [pickup, pcls] = pk === null ? ["Repairs catch some music at every setting: listen to the Noise", "warn"]
    : pk >= s.declick ? [`Repairs stay on the surface up to about DeClick ${pk}`, "dim"]
    : [`Above about DeClick ${pk} some repairs catch music: listen to the Noise`, "warn"];
  $("suggest").innerHTML =
    `<h4>SUGGESTED START</h4><p><b>DeClick ${s.declick}</b> | ${methods[s.method]}</p>` +
    `<p>Reverse ${s.reverse ? "on" : "off"} | Pitch ${s.pitch ? "on" : "off"} | DeCrackle ${s.decrackle > 0 ? s.decrackle : "off"}</p>` +
    `<p class="small">Corrections: ${fixes.join(", ")}</p><p class="small ${pcls}">${pickup}</p>` +
    `<p style="margin-top:4px" id="applywrap"></p>` +
    `<details><summary>Why</summary>${a.rationale.map((l) => `<p>${esc(l)}</p>`).join("")}</details>`;
  $("suggest").classList.remove("hidden");

  const [peakText, peakCls] = a.clipped > 0 ? [`Peak ${a.peak_dbfs.toFixed(1)} dBFS, ${a.clipped} clipped samples`, "bad"]
    : a.peak_dbfs < -12 ? [`Peak ${a.peak_dbfs.toFixed(1)} dBFS: low capture level, bits unused`, "warn"]
    : [`Peak ${a.peak_dbfs.toFixed(1)} dBFS, no clipping`, ""];
  const chans = { stereo: "Stereo record", merge: "Mono record captured in stereo", mono: "Mono (identical channels)" }[a.mode];
  $("recording").innerHTML =
    `<h4>RECORDING</h4><p class="${peakCls}">${peakText}</p><p>Music level ${a.music_rms_db.toFixed(1)} dBFS RMS</p><p>${chans}</p>` +
    `<p class="dim small">L/R correlation ${a.lr_corr.toFixed(3)}, side/mid ${a.side_mid.toFixed(3)}</p>` +
    `<p class="dim small">Repairs at DeClick ${a.analysis_dcl}: ${a.repair_fraction > 0 ? `1 in ${Math.round(1 / a.repair_fraction).toLocaleString()}` : "none"} samples</p>`;
  $("recording").classList.remove("hidden");

  $("spots").innerHTML = `<h4>PROBLEM SPOTS (N = next)</h4>` + (ms.length ? `<div class="spots">${ms.map((sp, i) =>
    `<button data-i="${i}" title="${sp.clicks} clicks (${sp.pops} loud pops) standing clear of the music at ${sp.level_db.toFixed(0)} dBFS${sp.long ? `; longest repair ${sp.longest_ms.toFixed(1)} ms` : ""}${sp.edge ? ". Next to silence or the file's start or end: may be a needle drop or run-out groove" : ""}. Click to go there.">` +
    `${clock(sp.start / st.sr)}  ${String(sp.clicks).padStart(3)} clicks${sp.long ? "  long" : ""}${sp.edge ? "  edge" : ""}</button>`).join("")}</div>`
    : `<p class="good">No exposed clicks in the music.</p>`);
  $("spots").classList.remove("hidden");
  $("spots").querySelectorAll("button").forEach((b) => (b.onclick = () => goToSpot(+b.dataset.i)));

  // Corrections: the measured values become available.
  $("balance_t").textContent = `Balance ${sign(a.balance_db, 2)} dB`;
  $("azimuth_t").textContent = `Azimuth ${sign(a.azimuth_us, 1)} us`;
  $("elliptical_t").textContent = `Elliptical EQ ${p.elliptical_hz.toFixed(0)} Hz`;
  for (const id of ["balance", "azimuth"]) { $(id + "_on").disabled = false; $(id + "_l").classList.remove("off"); }
  if (a.thumps) {
    $("dethump_l").classList.remove("hidden");
    $("dethump_t").textContent = `Repair needle thumps (${a.thumps})`;
  }
  updateSuggestMatch();
}

function matchesSuggestion() {
  const a = st.analysis;
  if (!a) return false;
  const s = a.suggested, p = a.post, c = (id) => $(id).checked;
  return +$("declickn").value === s.declick && +$("decracklen").value === s.decrackle && +$("method").value === s.method &&
    c("pitch") === s.pitch && c("reverse") === s.reverse && c("post_on") && c("flip") === p.flip &&
    c("balance_on") === p.balance_on && c("azimuth_on") === p.azimuth_on && c("subsonic_on") === p.subsonic_on &&
    (c("elliptical_on") === p.elliptical_on || p.mono_sum) && c("mono_sum") === p.mono_sum;
}

function updateSuggestMatch() {
  const w = $("applywrap");
  if (!w) return;
  w.innerHTML = matchesSuggestion() ? `<span class="good small">Current settings match</span>` : `<button class="btn green" id="apply">Apply</button>`;
  if ($("apply")) $("apply").onclick = applySuggested;
}

function applySuggested() {
  const a = st.analysis, s = a.suggested, p = a.post;
  setValue("declick", s.declick);
  setValue("decrackle", s.decrackle);
  $("method").value = s.method;
  $("pitch").checked = s.pitch;
  $("reverse").checked = s.reverse;
  $("post_on").checked = true;
  $("flip").checked = p.flip;
  $("balance_on").checked = p.balance_on;
  $("azimuth_on").checked = p.azimuth_on;
  $("subsonic_on").checked = p.subsonic_on;
  $("elliptical_on").checked = p.elliptical_on;
  $("mono_sum").checked = p.mono_sum;
  settingsChanged();
  status("Applied the suggested settings.");
}

function goToSpot(i) {
  const sp = st.analysis.spots[i];
  if (!sp) return;
  st.spotIndex = i;
  const pad = st.sr * 2, span = Math.max(sp.len + 2 * pad, st.sr * 6);
  setView(sp.start - pad, sp.start - pad + span);
  seek(Math.max(0, sp.start - st.sr * 0.5));
}

// ---------- analysis, render and download ----------

function analyze() {
  if (!st.ready || st.busy) return;
  pause();
  st.busy = "analysis";
  $("analyze").disabled = true;
  $("aprogress").classList.remove("hidden");
  setBar($("aprogress").firstElementChild, 0);
  status("Analyzing the record: grading, mapping clicks, measuring channels...");
  st.worker.postMessage({ type: "analyze" });
}

function render() {
  if (!st.ready || st.busy) return;
  pause();
  st.busy = "render";
  $("modaltitle").textContent = "RENDERING";
  $("modaltext").textContent = "Declicking the whole file, then the corrections. Playback is paused meanwhile.";
  $("modalstat").textContent = "";
  setBar($("modalbar"), 0);
  $("modal").classList.remove("hidden");
  st.worker.postMessage({ type: "render", settings: settings() });
}

function download(l, r, seconds, levels) {
  // 24-bit WAV. With corrections on, the engine has already applied an album gain if they pushed
  // peaks over full scale (as the desktop does); without them, nothing goes over.
  const n = l.length, bytes = 44 + n * 6;
  const buf = new ArrayBuffer(bytes), v = new DataView(buf), u8 = new Uint8Array(buf);
  const str = (o, s) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
  str(0, "RIFF"); v.setUint32(4, bytes - 8, true); str(8, "WAVE");
  str(12, "fmt "); v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 2, true);
  v.setUint32(24, st.sr, true); v.setUint32(28, st.sr * 6, true); v.setUint16(32, 6, true); v.setUint16(34, 24, true);
  str(36, "data"); v.setUint32(40, n * 6, true);
  let o = 44, clipped = 0;
  for (let i = 0; i < n; i++) {
    for (const ch of [l, r]) {
      let s = Math.round(ch[i] * 8388608);
      if (s > 8388607) { s = 8388607; clipped++; } else if (s < -8388607) { s = -8388607; clipped++; }
      u8[o] = s & 255; u8[o + 1] = (s >> 8) & 255; u8[o + 2] = (s >> 16) & 255;
      o += 3;
    }
  }
  const url = URL.createObjectURL(new Blob([buf], { type: "audio/wav" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = st.file.name.replace(/\.[^.]+$/, "") + "_restored.wav";
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 30000);
  const speed = n / st.sr / Math.max(seconds, 1e-3);
  const [peak, tp, lufs, gain] = levels || [];
  const lv = Number.isFinite(peak)
    ? ` Last render: peak ${peak.toFixed(1)} dBFS, true peak ${tp.toFixed(1)} dBTP, ${lufs.toFixed(1)} LUFS${gain < 0 ? `, gain ${gain.toFixed(1)} dB` : ""}.`
    : "";
  status(`Rendered in ${seconds.toFixed(1)} s (${speed.toFixed(1)}x real time) and downloaded as ${a.download}.` + lv +
    (clipped ? ` ${clipped} samples were clipped at full scale.` : ""));
}

// ---------- helpers ----------

function clock(s, ms) {
  const m = Math.floor(s / 60), r = s - 60 * m;
  return `${String(m).padStart(2, "0")}:${ms ? r.toFixed(3).padStart(6, "0") : String(Math.floor(r)).padStart(2, "0")}`;
}
function sign(v, d) { return `${v >= 0 ? "+" : ""}${v.toFixed(d)}`; }
function esc(t) { return String(t).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]); }
function setBar(el, f) { el.style.width = `${Math.round(100 * Math.min(1, f))}%`; }
function status(text, bad) { $("status").textContent = text; $("status").style.color = bad ? "var(--bad)" : ""; }

// ---------- wiring ----------

$("open").onclick = () => $("file").click();
$("drop").onclick = () => $("file").click();
$("drop").onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); $("file").click(); } };
$("file").onchange = (e) => e.target.files[0] && loadFile(e.target.files[0]);
document.body.addEventListener("dragover", (e) => { e.preventDefault(); $("drop").classList.add("over"); });
document.body.addEventListener("dragleave", () => $("drop").classList.remove("over"));
document.body.addEventListener("drop", (e) => {
  e.preventDefault();
  $("drop").classList.remove("over");
  if (e.dataTransfer.files[0]) loadFile(e.dataTransfer.files[0]);
});

bindSlider("declick");
bindSlider("decrackle");
for (const id of ["pitch", "reverse", "method", "engine", ...SETTINGS_IDS]) $(id).onchange = settingsChanged;
$("preset").onchange = () => { applyPreset($("preset").value); $("preset").value = ""; };
$("play").onclick = play;
$("pause").onclick = pause;
$("stop").onclick = stop;
$("tostart").onclick = () => { if (!st.orig) return; seek(0); setView(0, st.view.b - st.view.a); };
$("toend").onclick = () => { if (!st.orig) return; seek(st.total); setView(st.total - (st.view.b - st.view.a), st.total); };
$("zin").onclick = () => st.orig && zoom(0.5);
$("zout").onclick = () => st.orig && zoom(2);
$("fit").onclick = () => st.orig && setView(0, st.total);
document.querySelectorAll("#modes button").forEach((b) => (b.onclick = () => setMode(+b.dataset.mode)));
$("analyze").onclick = analyze;
$("render").onclick = render;
$("helpbtn").onclick = () => $("help").classList.remove("hidden");
$("helpclose").onclick = () => $("help").classList.add("hidden");

const frameAt = (e, c, a, b) => { const r = c.getBoundingClientRect(); return a + ((e.clientX - r.left) / r.width) * (b - a); };
$("wave").onpointerdown = (e) => { if (st.orig) seek(frameAt(e, $("wave"), st.view.a, st.view.b)); };
$("wave").addEventListener("wheel", (e) => {
  if (!st.orig) return;
  e.preventDefault();
  if (e.shiftKey || Math.abs(e.deltaX) > Math.abs(e.deltaY)) {
    const span = st.view.b - st.view.a, d = ((e.shiftKey ? e.deltaY : e.deltaX) / 600) * span;
    setView(st.view.a + d, st.view.b + d);
  } else {
    zoom(e.deltaY > 0 ? 1.25 : 0.8, frameAt(e, $("wave"), st.view.a, st.view.b));
  }
}, { passive: false });
let dragging = false;
const centreView = (e) => {
  const f = frameAt(e, $("overview"), 0, st.total), span = st.view.b - st.view.a;
  setView(f - span / 2, f + span / 2);
};
$("overview").onpointerdown = (e) => { if (!st.orig) return; dragging = true; $("overview").setPointerCapture(e.pointerId); centreView(e); };
$("overview").onpointermove = (e) => { if (dragging) centreView(e); };
$("overview").onpointerup = () => (dragging = false);

document.addEventListener("keydown", (e) => {
  if (!st.orig || e.target.matches("input[type=number], select") || e.ctrlKey || e.metaKey || e.altKey) return;
  const k = e.key;
  if (k === " ") { e.preventDefault(); st.playing ? pause() : play(); }
  else if (k === "1" || k === "2" || k === "3") setMode(+k - 1);
  else if (k === "+" || k === "=") zoom(0.5);
  else if (k === "-") zoom(2);
  else if (k === "Home") $("tostart").onclick();
  else if (k === "End") $("toend").onclick();
  else if ((k === "n" || k === "N") && st.analysis && st.analysis.spots.length) {
    goToSpot(((st.spotIndex ?? -1) + 1) % st.analysis.spots.length);
  }
});
window.addEventListener("resize", () => draw(true));

// ?src=URL loads a file from the same site (for example a sample recording next to the page).
const src = new URLSearchParams(location.search).get("src");
if (src) {
  fetch(src).then((r) => { if (!r.ok) throw new Error(r.status); return r.blob(); })
    .then((b) => loadFile(new File([b], decodeURIComponent(src.split("/").pop()))))
    .catch(() => status(`Could not load ${src}.`, true));
}
