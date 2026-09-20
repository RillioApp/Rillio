// S1 seam run over CDP: start the dub worker, add the track, measure first-audio
// latency, observe sync, seek into an unproduced region (must wait, not stop),
// seek back into a produced one, switch aid back to the original.
//   node s1-driver.js <stream url>
const url = process.argv[2];
const port = process.env.CDP_PORT || 9222;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let ws, nextId = 1;
const pending = new Map();
async function connect() {
  const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
  const page = targets.find((t) => t.type === "page" && t.url.startsWith("http://tauri.localhost"));
  ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  ws.onmessage = (e) => { const m = JSON.parse(e.data); if (pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); } };
}
function evaluate(expression) {
  const id = nextId++;
  ws.send(JSON.stringify({ id, method: "Runtime.evaluate", params: { expression, awaitPromise: true, returnByValue: true } }));
  return new Promise((resolve, reject) => pending.set(id, (m) => m.result?.exceptionDetails ? reject(new Error(JSON.stringify(m.result.exceptionDetails.exception?.description || m))) : resolve(m.result?.result?.value)));
}
const invoke = (cmd, args) => evaluate(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args || {})})`);
const stats = async () => { const s = await invoke("shell_mpv_stats"); return { tp: s["time-pos"], dur: s["duration"], aid: s["aid"], pfc: s["paused-for-cache"], pause: s["pause"] }; };
const setProp = (name, value) => invoke("shell_send", { method: "mpv-set-prop", args: [[name, value]] });
async function waitFor(label, pred, timeoutMs, every = 250) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) { const v = await pred(); if (v) return { ok: true, ms: Date.now() - t0, v }; await sleep(every); }
  return { ok: false, ms: Date.now() - t0 };
}
(async () => {
  await connect();
  const report = {};
  await waitFor("core", () => evaluate("!!(window.core && window.core.encodeStream)"), 60000);
  const first = await stats();
  if (first.tp === undefined) {
    await evaluate(`Promise.resolve(window.core.encodeStream({name:'',description:'',url:${JSON.stringify(url)}})).then(e => { location.hash = '#/player/' + encodeURIComponent(e); })`);
  }
  report.playing = await waitFor("playing", async () => { const s = await stats(); return s.tp > 2 && s.dur > 0; }, 60000);
  const info = await invoke("dub_start", { url });
  report.dub_start = info;
  report.first_window = await waitFor("window0", async () => (await invoke("dub_start", { url })).produced >= 1, 120000);
  const before = await stats();
  const t0 = Date.now();
  await invoke("dub_select");
  report.selected = await waitFor("aid2", async () => (await stats()).aid === 2, 15000);
  report.first_audio_ms = Date.now() - t0;
  const a = await stats(); await sleep(8000); const b = await stats();
  report.sync = { before_tp: before.tp, tp_a: a.tp, tp_b: b.tp, advanced_s: +(b.tp - a.tp).toFixed(2), pfc: b.pfc };
  report.produced_after_sync = (await invoke("dub_start", { url })).produced;
  // Seek into an unproduced region (well ahead of the worker).
  await setProp("time-pos", "900");
  await sleep(1500);
  const s1 = await stats(); await sleep(6000); const s2 = await stats();
  report.seek_unproduced = { tp1: s1.tp, tp2: s2.tp, pfc1: s1.pfc, pfc2: s2.pfc, aid: s2.aid };
  report.worker_reached = await waitFor("window30", async () => { const st = await stats(); return st.tp > 903 ? st : false; }, 180000);
  const s3 = await stats(); await sleep(5000); const s4 = await stats();
  report.after_reach = { tp3: s3.tp, tp4: s4.tp, advanced_s: +(s4.tp - s3.tp).toFixed(2), pfc: s4.pfc };
  // Seek back into a produced region.
  await setProp("time-pos", "40");
  await sleep(2000);
  const s5 = await stats(); await sleep(4000); const s6 = await stats();
  report.seek_produced = { tp5: s5.tp, tp6: s6.tp, advanced_s: +(s6.tp - s5.tp).toFixed(2), pfc: s6.pfc, aid: s6.aid };
  // Back to the original track.
  await setProp("aid", "1");
  report.aid_back = await waitFor("aid1", async () => (await stats()).aid === 1, 10000);
  report.final = await stats();
  console.log(JSON.stringify(report, null, 1));
  ws.close();
})().catch((e) => { console.error(String(e)); process.exit(1); });
