// The "On this device" poll: the next load starts only after the previous
// one FINISHED, so a load slower than the interval still lands (an interval
// that invalidated every in-flight load whenever a new tick began threw away
// every result of a slow server, and piled requests up), and a stopped poll
// neither starts another load nor lets a late result through.

const { pollSequential } = require('../src/routes/MetaDetails/StreamsList/pollSequential');

// A load whose completion the test controls.
const controlledLoad = () => {
    const calls = [];
    const load = (isCurrent) => new Promise((resolve) => {
        calls.push({ isCurrent, resolve });
    });
    return { load, calls };
};

// Let pending promise callbacks run (the poll chains on .finally).
const flush = async () => {
    for (let i = 0; i < 5; i += 1) await Promise.resolve();
};

beforeEach(() => jest.useFakeTimers());
afterEach(() => jest.useRealTimers());

test('a load slower than the interval is never overlapped and its result stays current', async () => {
    const { load, calls } = controlledLoad();
    const stop = pollSequential(load, 3000);
    await flush();
    expect(calls).toHaveLength(1);
    // Ten seconds of a slow server: no second load starts meanwhile.
    jest.advanceTimersByTime(10000);
    await flush();
    expect(calls).toHaveLength(1);
    expect(calls[0].isCurrent()).toBe(true);
    // It finishes; the next one starts one interval later, not before.
    calls[0].resolve();
    await flush();
    jest.advanceTimersByTime(2999);
    await flush();
    expect(calls).toHaveLength(1);
    jest.advanceTimersByTime(1);
    await flush();
    expect(calls).toHaveLength(2);
    stop();
});

test('stopping (unmount, another title) invalidates the in-flight load and ends the chain', async () => {
    const { load, calls } = controlledLoad();
    const stop = pollSequential(load, 3000);
    await flush();
    stop();
    expect(calls[0].isCurrent()).toBe(false);
    calls[0].resolve();
    await flush();
    jest.advanceTimersByTime(30000);
    await flush();
    expect(calls).toHaveLength(1);
});

test('a load that throws does not end the poll', async () => {
    let count = 0;
    const stop = pollSequential(() => {
        count += 1;
        return Promise.reject(new Error('server down'));
    }, 3000);
    const spy = jest.spyOn(console, 'error').mockImplementation(() => {});
    await flush();
    jest.advanceTimersByTime(3000);
    await flush();
    expect(count).toBe(2);
    stop();
    spy.mockRestore();
});
