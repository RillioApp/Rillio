// ShellVideo against a fake shell transport (no mpv). The transport is ONE
// process-wide event bus in the real app (common/Platform/shell/useShell.ts),
// and the shell has one mpv: N player visits must not mean N reactions to an
// mpv event, and an END_FILE must only ever act on the load it belongs to.
// The shell echoes the `loadId` a loadfile was sent with on the END_FILE of
// the playlist entry that loadfile created (shell.rs `LoadIds`).

const EventEmitter = require('eventemitter3');
const ShellVideo = require('../../../packages/video/src/ShellVideo/ShellVideo');

const IH = '08ada5a7a6183aae1e09d831df6748d566095a10';
const IH2 = 'dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c';
const httpUrl = (ih, idx) => `http://127.0.0.1:5555/${ih}/${idx}`;
const SIGNALS = ['mpv-prop-change', 'mpv-event-ended', 'rillio-stream-cb'];

// `deferHandshake`: the shell's answers to the version / byte-plane queries
// wait until `answer()`, the window in which a load is pending in ShellVideo.
function fakeShell({ deferHandshake = false } = {}) {
    const bus = new EventEmitter();
    const sent = [];
    const held = [];
    const answer = () => held.splice(0).forEach((emit) => emit());
    const reply = (emit) => (deferHandshake ? held.push(emit) : Promise.resolve().then(emit));
    return {
        sent,
        bus,
        answer,
        on: (name, listener) => bus.on(name, listener),
        off: (name, listener) => bus.off(name, listener),
        send: (method, ...args) => {
            sent.push([method, ...args]);
            if (method === 'mpv-observe-prop' && args[0] === 'mpv-version') {
                reply(() => bus.emit('mpv-prop-change', { name: 'mpv-version', data: 'mpv v0.41.0' }));
            }
            if (method === 'rillio-stream-cb-query') {
                reply(() => bus.emit('rillio-stream-cb', { available: true }));
            }
        },
    };
}

// ShellVideo paints the page background through `window.document`; nothing
// here has a page, and it guards a missing document.
global.window = global.window || {};
// The capability-query timeout is a 3 s timer; fake timers keep it from
// holding the run open, and the handshake answers by microtask anyway.
beforeEach(() => jest.useFakeTimers());
afterEach(() => jest.useRealTimers());
const flush = async () => {
    for (let i = 0; i < 10; i++) await Promise.resolve();
};
const loadfileSends = (shell) => shell.sent.filter(([method, args]) => method === 'mpv-command' && args[0] === 'loadfile');
const loadfiles = (shell) => loadfileSends(shell).map(([, args]) => args[1]);
const lastLoadId = (shell) => {
    const sends = loadfileSends(shell);
    const [, , meta] = sends[sends.length - 1];
    return meta.loadId;
};
const stops = (shell) => shell.sent.filter(([method, args]) => method === 'mpv-command' && args[0] === 'stop').length;
const ended = (shell, loadId, error) => shell.bus.emit('mpv-event-ended', error ?
    { reason: 'error', error, loadId }
    :
    { reason: 'stop', error: null, loadId });

function build(shell) {
    const video = new ShellVideo({ shellTransport: shell });
    const errors = [];
    const changes = [];
    video.on('error', (error) => errors.push(error));
    video.on('propChanged', (name, value) => changes.push([name, value]));
    video.dispatch({ type: 'observeProp', propName: 'volume' });
    return { video, errors, changes };
}

function load(video, ih = IH, idx = 5) {
    video.dispatch({
        type: 'command',
        commandName: 'load',
        commandArgs: { stream: { url: httpUrl(ih, idx), infoHash: ih, fileIdx: idx }, platform: 'windows', videoMode: null },
    });
}

const destroy = (video) => video.dispatch({ type: 'command', commandName: 'destroy' });

test('a superseded visit, torn down, drives nothing on the shared mpv', async () => {
    // The shell has ONE mpv. When a new player visit (B) takes over, the old
    // visit (A) being destroyed must not send `stop` (or anything) to it: that
    // would halt B's playback on the shared instance. A resets only its own
    // state. (Measured risk: A.destroy -> command('unload') -> stop -> B stops.)
    const shell = fakeShell();
    const a = build(shell);
    load(a.video, IH, 5);
    await flush();
    const b = build(shell); // attaching B detaches A (the singleton owner moves)
    load(b.video, IH2, 1);
    await flush();
    const stopsBefore = stops(shell);
    const loadfilesBefore = loadfiles(shell).length;

    destroy(a.video);
    expect(stops(shell)).toBe(stopsBefore);
    expect(loadfiles(shell).length).toBe(loadfilesBefore);

    // B is unharmed: its own load still retries over http on a byte-plane
    // refusal, and A's teardown surfaced no error on B.
    ended(shell, lastLoadId(shell), 'loading failed');
    expect(loadfiles(shell).slice(-1)).toEqual([httpUrl(IH2, 1)]);
    expect(b.errors).toEqual([]);
    expect(a.errors).toEqual([]);
});

test('the live visit still stops the shared mpv on a normal unload', async () => {
    // The real single-visit case must keep working: a visit that is the owner
    // stops mpv when the user navigates away with nothing after it.
    const shell = fakeShell();
    const { video } = build(shell);
    load(video);
    await flush();
    const stopsBefore = stops(shell);
    video.dispatch({ type: 'command', commandName: 'unload' });
    expect(stops(shell)).toBe(stopsBefore + 1);
});

test('a cold byte-plane refusal retries once over http, and a failing retry is the error', async () => {
    const shell = fakeShell();
    const { video, errors } = build(shell);
    load(video);
    await flush();
    expect(loadfiles(shell)).toEqual([`rillio://${IH}/5`]);
    ended(shell, lastLoadId(shell), 'loading failed');
    expect(loadfiles(shell)).toEqual([`rillio://${IH}/5`, httpUrl(IH, 5)]);
    expect(errors).toEqual([]);
    ended(shell, lastLoadId(shell), 'loading failed');
    expect(loadfiles(shell).length).toBe(2);
    expect(errors.map((e) => [e.critical, e.message])).toEqual([[true, 'loading failed']]);
});

test('N player visits: one subscription, one reaction per mpv event', async () => {
    const shell = fakeShell();
    const N = 6;
    let visit = null;
    for (let i = 0; i < N; i++) {
        if (visit) destroy(visit.video);
        visit = build(shell);
        load(visit.video);
        await flush();
    }
    // One listener per signal and one observer per property, whatever N is.
    SIGNALS.forEach((name) => expect(shell.bus.listenerCount(name)).toBe(1));
    const observes = shell.sent.filter(([method]) => method === 'mpv-observe-prop');
    expect(new Set(observes.map(([, name]) => name)).size).toBe(observes.length);

    const stopsBefore = stops(shell);
    ended(shell, lastLoadId(shell), 'loading failed');
    // The live visit retries over http; nothing else sends anything.
    expect(stops(shell)).toBe(stopsBefore);
    expect(loadfiles(shell).slice(-1)).toEqual([httpUrl(IH, 5)]);

    visit.video.dispatch({ type: 'observeProp', propName: 'paused' });
    visit.changes.length = 0;
    shell.bus.emit('mpv-prop-change', { name: 'pause', data: true });
    expect(visit.changes.filter(([name]) => name === 'paused')).toEqual([['paused', true]]);
});

test('a new visit starts from the current property values, not from nothing', async () => {
    const shell = fakeShell();
    const first = build(shell);
    await flush();
    shell.bus.emit('mpv-prop-change', { name: 'volume', data: 70 });
    destroy(first.video);
    const second = new ShellVideo({ shellTransport: shell });
    const values = [];
    second.on('propValue', (name, value) => values.push([name, value]));
    second.dispatch({ type: 'observeProp', propName: 'volume' });
    expect(values).toEqual([['volume', 70]]);
});

test('two loads back to back: the first load\'s END_FILE is neither an error nor the second\'s retry', async () => {
    const shell = fakeShell();
    const { video, errors } = build(shell);
    load(video, IH, 5);
    await flush();
    const first = lastLoadId(shell);
    load(video, IH2, 1);
    await flush();
    const second = lastLoadId(shell);
    expect(second).not.toBe(first);
    expect(loadfiles(shell)).toEqual([`rillio://${IH}/5`, `rillio://${IH2}/1`]);

    // The first loadfile's immediate refusal lands after the second went out.
    ended(shell, first, 'loading failed');
    expect(errors).toEqual([]);
    expect(loadfiles(shell)).toEqual([`rillio://${IH}/5`, `rillio://${IH2}/1`]);

    // The second load still has its own retry.
    ended(shell, second, 'loading failed');
    expect(errors).toEqual([]);
    expect(loadfiles(shell).slice(-1)).toEqual([httpUrl(IH2, 1)]);
});

test('an unload between a loadfile and its END_FILE does not turn that END_FILE into an error', async () => {
    // The sequence measured in a 0.1.51 dev shell (race-0151-sweep #3): the
    // player's load effect re-ran (unload, then load) between a rillio://
    // loadfile and its refusal, and the error screen came up.
    const shell = fakeShell();
    const { video, errors } = build(shell);
    load(video);
    await flush();
    const superseded = lastLoadId(shell);
    video.dispatch({ type: 'command', commandName: 'unload' });
    load(video);
    ended(shell, superseded, 'loading failed');
    await flush();
    expect(errors).toEqual([]);
    expect(loadfiles(shell)).toEqual([`rillio://${IH}/5`, `rillio://${IH}/5`]);
    ended(shell, lastLoadId(shell), 'loading failed');
    expect(loadfiles(shell).slice(-1)).toEqual([httpUrl(IH, 5)]);
    expect(errors).toEqual([]);
});

test('an old visit\'s END_FILE arriving after the next visit loaded is not the new player\'s error', async () => {
    const shell = fakeShell();
    const first = build(shell);
    load(first.video, IH, 5);
    await flush();
    const old = lastLoadId(shell);
    destroy(first.video);

    const second = build(shell);
    load(second.video, IH2, 1);
    await flush();
    ended(shell, old, 'loading failed');
    ended(shell, old, null);
    expect(second.errors).toEqual([]);
    expect(first.errors).toEqual([]);
    expect(loadfiles(shell)).toEqual([`rillio://${IH}/5`, `rillio://${IH2}/1`]);
    // ...and the new load keeps its own retry.
    ended(shell, lastLoadId(shell), 'loading failed');
    expect(loadfiles(shell).slice(-1)).toEqual([httpUrl(IH2, 1)]);
});

test('a load still waiting on the shell handshake when the video is destroyed never reaches mpv', async () => {
    const shell = fakeShell({ deferHandshake: true });
    const { video } = build(shell);
    load(video);
    await flush();
    destroy(video);
    shell.answer();
    await flush();
    expect(loadfiles(shell)).toEqual([]);
});

test('a load superseded before the shell handshake answers never reaches mpv', async () => {
    const shell = fakeShell({ deferHandshake: true });
    const { video } = build(shell);
    load(video, IH, 5);
    await flush();
    load(video, IH2, 1);
    await flush();
    shell.answer();
    await flush();
    expect(loadfiles(shell)).toEqual([`rillio://${IH2}/1`]);
});
