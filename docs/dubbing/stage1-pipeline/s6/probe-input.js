// Does trusted input reach the player? ArrowRight should seek +10 s; a mouse
// move should wake the control bar (lucide icons appear).
const port = process.env.CDP_PORT || 9222;
(async () => {
  const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
  const page = targets.find((t) => t.type === "page" && t.url.startsWith("http://tauri.localhost"));
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  let id = 0; const pending = new Map();
  ws.onmessage = (e) => { const m = JSON.parse(e.data); if (pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); } };
  const send = (method, params) => new Promise((resolve) => { const i = ++id; pending.set(i, resolve); ws.send(JSON.stringify({ id: i, method, params })); });
  const ev = async (expression) => (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result?.result?.value;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const tp = () => ev("window.__TAURI_INTERNALS__.invoke('shell_mpv_stats').then(s => s['time-pos'])");
  const before = await tp();
  await send("Input.dispatchKeyEvent", { type: "keyDown", key: "ArrowRight", code: "ArrowRight", windowsVirtualKeyCode: 39 });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: "ArrowRight", code: "ArrowRight", windowsVirtualKeyCode: 39 });
  await sleep(1500);
  const after = await tp();
  console.log("seek by ArrowRight:", before, "->", after);
  const size = await ev("({ w: innerWidth, h: innerHeight })");
  for (let i = 0; i < 5; i++) {
    await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: 300 + i * 20, y: 300 + i * 10 });
    await sleep(100);
  }
  await sleep(400);
  const icons = await ev("[...document.querySelectorAll('button svg')].map(s => (s.getAttribute('class') || '').match(/lucide-[a-z-]+/)).filter(Boolean).map(m => m[0])");
  console.log("window", JSON.stringify(size), "icons after mouse move:", JSON.stringify(icons));
  ws.close();
})().catch((e) => { console.error(String(e)); process.exit(1); });
