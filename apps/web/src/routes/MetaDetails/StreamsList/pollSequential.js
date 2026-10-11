// Copyright (C) 2017-2026 Smart code 203358507

// A poll whose next load starts only once the previous one has FINISHED, then
// waits intervalMs. An interval timer does not: it starts a load every tick
// whatever the last one is doing, so when one load (the cache list plus a
// file listing per torrent) takes longer than the interval, requests pile up,
// and a "newest load wins" guard then throws away every result.
//
// load(isCurrent) returns a promise; isCurrent() turns false once the poll is
// stopped, so a load in flight at unmount or a title change can drop its
// result. Returns stop(). Plain CommonJS so jest can require it
// (tests/pollSequential.spec.js); useCachedStreams uses it.

const pollSequential = (load, intervalMs) => {
    let stopped = false;
    let timer = null;
    const isCurrent = () => !stopped;
    const run = () => {
        Promise.resolve()
            // The first run is queued a microtask after the call, and a stop()
            // can land in between (mount then unmount in the same tick).
            .then(() => (stopped ? undefined : load(isCurrent)))
            // A failed load is the load's own business to report; it must
            // not end the poll.
            .catch((error) => console.error('pollSequential: a load failed', error))
            .finally(() => {
                if (!stopped) timer = setTimeout(run, intervalMs);
            });
    };
    run();
    return () => {
        stopped = true;
        if (timer !== null) clearTimeout(timer);
    };
};

module.exports = { pollSequential };
