// The Cache window's data bookkeeping: stale responses, server changes,
// removed hashes, per-addon episode lists, retries, the open card.

const {
    RETRY_MS, emptyBook, bookFor, beginRequest, applyResponse, applyFailure, prune,
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
