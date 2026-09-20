// Evaluate one expression in the dev shell's page over CDP and print the value;
// or press a key first: `node eval.js --key a "<expression>"` sends a trusted
// keyDown/keyUp through Input.dispatchKeyEvent (synthetic DOM key events are
// not enough for the app's shortcuts).
const port = process.env.CDP_PORT || 9222;
const args = process.argv.slice(2);
const key = args[0] === "--key" ? args.splice(0, 2)[1] : null;
(async () => {
  const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
  const page = targets.find((t) => t.type === "page" && t.url.startsWith("http://tauri.localhost"));
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  let id = 0;
  const pending = new Map();
  ws.onmessage = (e) => { const m = JSON.parse(e.data); if (pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); } };
  const send = (method, params) => new Promise((resolve) => { const i = ++id; pending.set(i, resolve); ws.send(JSON.stringify({ id: i, method, params })); });
  if (key) {
    const code = "Key" + key.toUpperCase();
    const vk = key.toUpperCase().charCodeAt(0);
    await send("Input.dispatchKeyEvent", { type: "keyDown", key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk });
    await send("Input.dispatchKeyEvent", { type: "keyUp", key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk });
    await new Promise((r) => setTimeout(r, 600));
  }
  const m = await send("Runtime.evaluate", { expression: args[0] || "true", awaitPromise: true, returnByValue: true });
  console.log(JSON.stringify(m.result?.result?.value ?? m.result, null, 1));
  ws.close();
})().catch((e) => { console.error(String(e)); process.exit(1); });
