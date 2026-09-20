// Fine-grained playback sampling around the dub track with a SLOW worker
// (RILLIO_DUB_MIN_WINDOW_S set on the shell): does a blocked read-ahead stall
// playback, or only the prefetch?
//   node s1-sampler.js <stream url> <seek target seconds>
const url = process.argv[2];
const seekTo = process.argv[3] || "600";
const port = process.env.CDP_PORT || 9222;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let ws, nextId = 1; const pending = new Map();
async function connect() {
  const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
  const page = targets.find((t) => t.type === "page" && t.url.startsWith("http://tauri.localhost"));
  ws = new WebSocket(page.webSocketDebuggerUrl); await new Promise((r) => (ws.onopen = r));
  ws.onmessage = (e) => { const m = JSON.parse(e.data); if (pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); } };
}
function evaluate(expression) {
  const id = nextId++;
  ws.send(JSON.stringify({ id, method: "Runtime.evaluate", params: { expression, awaitPromise: true, returnByValue: true } }));
  return new Promise((resolve, reject) => pending.set(id, (m) => m.result?.exceptionDetails ? reject(new Error(m.result.exceptionDetails.exception?.description || JSON.stringify(m))) : resolve(m.result?.result?.value)));
}
const invoke = (cmd, args) => evaluate(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args || {})})`);
const setProp = (name, value) => invoke("shell_send", { method: "mpv-set-prop", args: [[name, value]] });
const observe = (name) => invoke("shell_send", { method: "mpv-observe-prop", args: [name] });
const stats = async () => { const s = await invoke("shell_mpv_stats"); return { t: Date.now(), tp: s["time-pos"], aid: s["aid"], pfc: s["paused-for-cache"], buf: s["cache-buffering-state"], cache: s["demuxer-cache-duration"] }; };
async function sample(label, seconds) {
  const rows = []; const t0 = Date.now();
  while (Date.now() - t0 < seconds * 1000) { rows.push(await stats()); await sleep(250); }
  const summary = rows.map((r) => `${((r.t - t0) / 1000).toFixed(1)}s tp=${r.tp?.toFixed(2)} pfc=${r.pfc} buf=${r.buf} cache=${r.cache?.toFixed?.(1)}`);
  const advanced = rows[rows.length - 1].tp - rows[0].tp;
  console.log(`== ${label}: advanced ${advanced.toFixed(2)} s in ${seconds} s`);
  console.log(summary.filter((_, i) => i % 4 === 0).join("\n"));
}
(async () => {
  await connect();
  for (const p of ["paused-for-cache", "cache-buffering-state", "demuxer-cache-duration"]) await observe(p);
  await invoke("dub_start", { url });
  const t0 = Date.now();
  while ((await invoke("dub_start", { url })).produced < 1) await sleep(250);
  console.log(`window 0 after ${Date.now() - t0} ms; selecting`);
  await invoke("dub_select");
  await sample("after select (slow worker)", 60);
  await setProp("time-pos", seekTo);
  await sample(`after seek to ${seekTo} s`, 60);
  console.log("produced now:", (await invoke("dub_start", { url })).produced);
  ws.close();
})().catch((e) => { console.error(String(e)); process.exit(1); });
