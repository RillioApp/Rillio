// The Cache window's data bookkeeping: stale responses, server changes,
// removed hashes, per-addon episode lists, retries, the open card.

const {
    RETRY_MS, emptyBook, bookFor, beginRequest, applyResponse, applyFailure, makeFileListLoader, prune,
    videoKey, keepWanted, retryInMs, stillOpen,
} = require('../src/routes/Cached/cacheDetailsState');

describe('file lists', () => {
    // The hook's own path: requests and responses are decided against the
    // CURRENT book (book.current), with generations from one counter.
    let counter = 0;
    const next = () => { counter += 1; return counter; };

    test('an older response never overwrites a newer one', () => {
        let book = emptyBook('http://a');
        const first = next();
        book = beginRequest(book, 'http://a', 'h', first);
        const second = next();
        book = beginRequest(book, 'http://a', 'h', second);
        book = applyResponse(book, 'http://a', 'h', second, ['new']);
        expect(applyResponse(book, 'http://a', 'h', first, ['old'])).toBeNull();
        expect(book.lists.h).toEqual(['new']);
    });

    test('a late response from the old server after a server change is dropped, the new lists kept', () => {
        let book = emptyBook('http://a');
        const old = next();
        book = beginRequest(book, 'http://a', 'h', old);
        // The server changes; the new server's lists arrive first.
        book = bookFor(book, 'http://b');
        const fresh = next();
        book = beginRequest(book, 'http://b', 'h', fresh);
        book = applyResponse(book, 'http://b', 'h', fresh, ['from b']);
        // The old server's answer lands now: the callback gets null and commits nothing.
        expect(applyResponse(book, 'http://a', 'h', old, ['from a'])).toBeNull();
        expect(applyFailure(book, 'http://a', 'h', old)).toBeNull();
        expect(book.lists.h).toEqual(['from b']);
        // And no request is recorded against a book of another server.
        expect(beginRequest(book, 'http://a', 'h', next())).toBeNull();
    });

    test('a hash deleted and re-added within one round trip never gets the deleted list', () => {
        let book = emptyBook('http://a');
        const before = next();
        book = beginRequest(book, 'http://a', 'h', before);
        book = prune(book, []);
        const after = next();
        book = beginRequest(book, 'http://a', 'h', after);
        expect(applyResponse(book, 'http://a', 'h', before, ['deleted'])).toBeNull();
        expect(applyResponse(book, 'http://a', 'h', after, ['re-added']).lists.h).toEqual(['re-added']);
    });

    test('a removed hash is forgotten, so a re-add refetches', () => {
        let book = beginRequest(emptyBook('http://a'), 'http://a', 'gone', next());
        book = { ...book, lists: { gone: ['x'] }, signatures: { gone: '1|2|3|live' } };
        book = prune(book, ['other']);
        expect(book.lists).toEqual({});
        expect(book.signatures).toEqual({});
        expect(book.generations).toEqual({});
    });
});
describe('makeFileListLoader: the loader the hook runs', () => {
    // Responses resolve only when the test says so, in any order.
    const setup = () => {
        const pending = [];
        const ref = { current: emptyBook('http://a') };
        const committed = [];
        let counter = 0;
        const load = makeFileListLoader({
            ref,
            commit: (book) => committed.push(book.lists),
            fetchList: (server, infoHash) => new Promise((resolve, reject) => pending.push({ server, infoHash, resolve, reject })),
            nextGeneration: () => { counter += 1; return counter; },
        });
        const settle = (index, list) => { pending[index].resolve(list); return new Promise((r) => setTimeout(r, 0)); };
        return { ref, committed, load, pending, settle };
    };

    test('two loads of one hash: the older answer, arriving last, is dropped', async () => {
        const { ref, load, settle } = setup();
        load('http://a', 'h');
        load('http://a', 'h');
        await settle(1, ['new']);
        await settle(0, ['old']);
        expect(ref.current.lists.h).toEqual(['new']);
    });

    test('a late answer from the old server never wipes the new server\'s lists', async () => {
        const { ref, committed, load, settle } = setup();
        load('http://a', 'h');
        // The hook's effect moves the book to the new server.
        ref.current = bookFor(ref.current, 'http://b');
        load('http://b', 'h');
        await settle(1, ['from b']);
        await settle(0, ['from a']);
        expect(ref.current.server).toBe('http://b');
        expect(ref.current.lists).toEqual({ h: ['from b'] });
        expect(committed).toEqual([{ h: ['from b'] }]);
    });

    test('a hash deleted and re-added within one round trip gets the new list only', async () => {
        const { ref, load, settle } = setup();
        load('http://a', 'h');
        ref.current = prune(ref.current, []);
        load('http://a', 'h');
        await settle(0, ['deleted']);
        expect(ref.current.lists.h).toBeUndefined();
        await settle(1, ['re-added']);
        expect(ref.current.lists.h).toEqual(['re-added']);
    });

    test('no request against another server\'s book', () => {
        const { load, pending } = setup();
        load('http://other', 'h');
        expect(pending).toHaveLength(0);
    });
});

describe('episode lists', () => {
    test('keyed by addon, type and id; movies and addon-less titles have none', () => {
        expect(videoKey({ metaId: 'tt1', type: 'series', metaTransportUrl: 'https://a/manifest.json' })).toBe('https://a/manifest.json|series|tt1');
        expect(videoKey({ metaId: 'tt1', type: 'series', metaTransportUrl: 'https://b/manifest.json' })).not.toBe(videoKey({ metaId: 'tt1', type: 'series', metaTransportUrl: 'https://a/manifest.json' }));
        expect(videoKey({ metaId: 'tt2', type: 'movie', metaTransportUrl: 'https://a/manifest.json' })).toBeNull();
        expect(videoKey({ metaId: 'tt3', type: 'series', metaTransportUrl: null })).toBeNull();
        expect(videoKey(undefined)).toBeNull();
    });

    test('obsolete associations are dropped', () => {
        expect(keepWanted({ a: [1], b: null }, ['b'])).toEqual({ b: null });
    });

    test('a failure is retried once RETRY_MS has passed; a success never', () => {
        expect(retryInMs({ videos: [], at: 0 }, 10)).toBeNull();
        expect(retryInMs({ videos: null, at: 1000 }, 1000)).toBe(RETRY_MS);
        expect(retryInMs({ videos: null, at: 0 }, RETRY_MS + 5)).toBe(0);
    });
});

describe('the open card', () => {
    test('closes when its download leaves the list (a re-add starts closed)', () => {
        expect(stillOpen('a', [{ infoHash: 'a' }])).toBe('a');
        expect(stillOpen('a', [{ infoHash: 'b' }])).toBeNull();
        expect(stillOpen(null, [{ infoHash: 'a' }])).toBeNull();
    });
});
