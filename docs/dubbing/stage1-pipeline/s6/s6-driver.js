// S6: the real dub in the dev shell over CDP. The shell runs with
// RILLIO_DEVTOOLS_PORT=9222 and RILLIO_DUB_PACK_DIR pointing at the staged
// pack; the stream is F6 over ../s1/range-server.js.
//   node s6-driver.js <stream url>
// Measures: pack status/adopt, time to "running" (sidecars + models + the
// first window), first audio after select, playback sync, the Audio menu
// row's text, a seek into an unproduced region and back, aid restored.
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
// A failed command names itself in the error (mpv's -12 says nothing else).
const invoke = (cmd, args) => evaluate(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args || {})})`).catch((e) => { throw new Error(`${cmd}(${JSON.stringify(args || {})}): ${e.message}`); });
const stats = async () => { const s = await invoke("shell_mpv_stats"); return { tp: s["time-pos"], dur: s["duration"], aid: s["aid"], pfc: s["paused-for-cache"] }; };
const setProp = (name, value) => invoke("shell_send", { method: "mpv-set-prop", args: [[name, value]] });
async function waitFor(label, pred, timeoutMs, every = 500) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) { const v = await pred(); if (v) return { ok: true, ms: Date.now() - t0, v }; await sleep(every); }
  return { ok: false, ms: Date.now() - t0 };
}
// The dub row in the Audio menu, by its hint (title) text.
const rowText = () => evaluate(`(() => { const b = [...document.querySelectorAll('button')].find(b => (b.title || '').startsWith('The dialogue translated')); return b ? b.innerText.replace(/\\n/g, ' | ') : null; })()`);
(async () => {
  await connect();
  const report = {};
  // The shell's dub status events, captured in the page.
  await evaluate(`window.__dubEvents = []; window.__TAURI__.event.listen('dub', (e) => window.__dubEvents.push([Date.now(), e.payload.state, e.payload.produced, e.payload.detail, e.payload.aheadS, e.payload.waiting]))`);
  await waitFor("core", () => evaluate("!!(window.core && window.core.encodeStream)"), 60000);
  const first = await stats();
  if (first.tp === undefined) {
    await evaluate(`Promise.resolve(window.core.encodeStream({name:'',description:'',url:${JSON.stringify(url)}})).then(e => { location.hash = '#/player/' + encodeURIComponent(e); })`);
  }
  report.playing = await waitFor("playing", async () => { const s = await stats(); return s.tp > 2 && s.dur > 0; }, 60000);
  report.pack_before = await invoke("pack_status");
  if (!report.pack_before.installed) {
    const t = Date.now();
    await invoke("pack_install");
    report.pack_install_ms = Date.now() - t;
  }
  const after = await invoke("pack_status");
  report.pack_after = { installed: after.installed, bytes_present: after.bytes_present, bytes_total: after.bytes_total };
  // Through the UI: open the Audio menu with its control-bar button and pick
  // the dub row; the hook starts the worker and switches the track once a
  // window is ready ahead of the playhead.
  // The control bar leaves the DOM when the chrome idles; the "A" shortcut
  // (shortcuts.json: audioMenu) opens the menu regardless.
  const openMenu = async () => {
    await evaluate(`document.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', code: 'KeyA', bubbles: true }))`);
    await sleep(500);
    return evaluate(`!![...document.querySelectorAll('button')].find(b => (b.title || '').startsWith('The dialogue translated'))`);
  };
  // CDP input does not reach this WebView2 (no shortcut, no chrome wake), so
  // the row cannot be clicked from here: the shell commands are driven
  // directly and the hook's switch rule (select once a window is ready
  // ahead of the playhead) is emulated. The hook must stay quiet: it did
  // not arm this run.
  report.menu_opened = await openMenu();
  // Mid-episode start (Michael's case): the worker begins on the playhead's
  // window, not window 0, and mpv's open of the track probes window 0.
  await setProp("time-pos", "300");
  await sleep(2000);
  const t0 = Date.now();
  report.dub_start = await invoke("dub_start", { url });
  // The pick selects the track at once (the hook's flow): the player holds on
  // the unmade window, then plays as audio arrives. The head of the file must
  // not freeze the core: the player keeps answering.
  await invoke("dub_select");
  report.selected = await waitFor("aid2", async () => (await stats()).aid === 2, 15000);
  report.alive_after_select = await waitFor("stats", async () => (await stats()).dur > 0, 5000);
  const hold0 = await stats();
  report.running = await waitFor("running", () => evaluate("window.__dubEvents.some(e => e[1] === 'running')"), 300000);
  report.ms_to_running = Date.now() - t0;
  report.first_audio = await waitFor("moving", async () => { const s = await stats(); return s.tp > hold0.tp + 1 ? s : false; }, 300000);
  report.ms_to_first_audio = Date.now() - t0;
  report.hold = { tp_at_pick: hold0.tp, tp_resumed: report.first_audio.v && report.first_audio.v.tp };
  const a = await stats(); await sleep(10000); const b = await stats();
  report.sync = { tp_a: a.tp, tp_b: b.tp, advanced_s: +(b.tp - a.tp).toFixed(2), pfc: b.pfc };
  report.waiting_seen = await evaluate("window.__dubEvents.some(e => e[5] === true)");
  report.produced_after_sync = (await invoke("dub_start", { url })).produced;
  await openMenu();
  await sleep(800);
  report.row_running = await rowText();
  await evaluate(`document.body.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))`);
  // Seek far ahead: the worker must re-target, playback waits then resumes.
  await setProp("time-pos", "600");
  await sleep(1500);
  const s1 = await stats(); await sleep(5000); const s2 = await stats();
  report.seek_unproduced = { tp1: s1.tp, tp2: s2.tp, pfc1: s1.pfc, pfc2: s2.pfc, aid: s2.aid };
  report.worker_reached = await waitFor("window20", async () => { const st = await stats(); return st.tp > 603 ? st : false; }, 300000);
  // Playback must not have frozen at any point: the player still answers.
  report.player_answers = await waitFor("stats", async () => (await stats()).dur > 0, 5000);
  const s3 = await stats(); await sleep(8000); const s4 = await stats();
  report.after_reach = { tp3: s3.tp, tp4: s4.tp, advanced_s: +(s4.tp - s3.tp).toFixed(2), pfc: s4.pfc };
  await setProp("time-pos", "40");
  await sleep(2000);
  const s5 = await stats(); await sleep(4000); const s6 = await stats();
  report.seek_produced = { tp5: s5.tp, tp6: s6.tp, advanced_s: +(s6.tp - s5.tp).toFixed(2), pfc: s6.pfc, aid: s6.aid };
  await setProp("aid", "1");
  report.aid_back = await waitFor("aid1", async () => (await stats()).aid === 1, 10000);
  report.events = await evaluate("window.__dubEvents");
  report.final = await stats();
  console.log(JSON.stringify(report, null, 1));
  ws.close();
})().catch(async (e) => {
  console.error(String(e));
  try { console.error(JSON.stringify(await evaluate("window.__dubEvents"))); } catch {}
  process.exit(1);
});
