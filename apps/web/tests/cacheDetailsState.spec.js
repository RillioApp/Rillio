// The Cache window's data bookkeeping: stale responses, server changes,
// removed hashes, per-addon episode lists, retries, the open card.

const {
    RETRY_MS, emptyBook, bookFor, beginRequest, applyResponse, prune,
    videoKey, keepWanted, retryInMs, stillOpen,
} = require('../src/routes/Cached/cacheDetailsState');

describe('file lists', () => {
    test('an older response never overwrites a newer one', () => {
        let book = emptyBook('http://a');
        const first = beginRequest(book, 'h');
        book = first.book;
        const second = beginRequest(book, 'h');
        book = second.book;
        book = applyResponse(book, 'http://a', 'h', second.generation, ['new']);
        book = applyResponse(book, 'http://a', 'h', first.generation, ['old']);
        expect(book.lists.h).toEqual(['new']);
    });

    test('a response from another server does not apply, and a server change starts over', () => {
        let book = emptyBook('http://a');
        const request = beginRequest(book, 'h');
        book = request.book;
        expect(applyResponse(book, 'http://b', 'h', request.generation, ['x']).lists).toEqual({});
        book = applyResponse(book, 'http://a', 'h', request.generation, ['x']);
        expect(bookFor(book, 'http://a')).toBe(book);
        expect(bookFor(book, 'http://b').lists).toEqual({});
    });

    test('a removed hash is forgotten, so a re-add refetches', () => {
        let book = emptyBook('http://a');
        const request = beginRequest(book, 'gone');
        book = applyResponse(request.book, 'http://a', 'gone', request.generation, ['x']);
        book = { ...book, signatures: { gone: '1|2|3|live' } };
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
