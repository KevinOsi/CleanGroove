// Runs CleanGroove's engine (cr_wasm.wasm) off the page's thread, so the page stays responsive.
//
// In:  { type: "run", sampleRate, left, right, settings }
// Out: { type: "progress", done, total, seconds }  then  { type: "done", left, right, seconds }

let exportsP = null;

function loadEngine() {
  if (!exportsP) {
    const url = new URL("cr_wasm.wasm", self.location);
    exportsP = (async () => {
      try {
        const { instance } = await WebAssembly.instantiateStreaming(fetch(url), {});
        return instance.exports;
      } catch (e) {
        // Servers that do not send application/wasm: fall back to a plain download.
        const bytes = await (await fetch(url)).arrayBuffer();
        const { instance } = await WebAssembly.instantiate(bytes, {});
        return instance.exports;
      }
    })();
  }
  return exportsP;
}

self.onmessage = async (ev) => {
  const msg = ev.data;
  if (msg.type !== "run") return;
  try {
    const x = await loadEngine();
    const { sampleRate, left, right, settings: s } = msg;
    const total = left.length;
    const t0 = performance.now();

    const job = x.cg_job_new(sampleRate, total, s.preset, s.declick, s.decrackle, s.method,
      s.pitch ? 1 : 0, s.reverse ? 1 : 0, s.engine);
    // Views are taken after the job exists: creating it may have grown the memory.
    const mem = () => x.memory.buffer;
    new Float32Array(mem(), x.cg_job_input_l(job), total).set(left);
    new Float32Array(mem(), x.cg_job_input_r(job), total).set(right);

    const CHUNK = 65536;
    let lastPost = 0;
    for (let start = 0; start < total; start += CHUNK) {
      x.cg_job_process(job, start, CHUNK);
      const now = performance.now();
      if (now - lastPost > 100) {
        lastPost = now;
        self.postMessage({ type: "progress", done: Math.min(start + CHUNK, total), total, seconds: (now - t0) / 1000 });
      }
    }
    x.cg_job_finish(job);

    const outL = new Float32Array(new Float32Array(mem(), x.cg_job_output_l(job), total));
    const outR = new Float32Array(new Float32Array(mem(), x.cg_job_output_r(job), total));
    x.cg_job_free(job);
    const seconds = (performance.now() - t0) / 1000;
    self.postMessage({ type: "done", left: outL, right: outR, seconds }, [outL.buffer, outR.buffer]);
  } catch (e) {
    self.postMessage({ type: "error", message: String(e && e.message || e) });
  }
};
