// The seek bar's thumbnail cache is a rolling set: at the cap each new frame
// overwrites the least recently used one (a read counts as a use), and frames
// captured for another pixel width are dropped rather than shown stretched.

const { createThumbCache, THUMB_CACHE_MAX } = require('../src/routes/Player/ControlBar/SeekBar/thumbCache');

const WIDTH = 640;

describe('thumbCache', () => {
    it('holds frames under the cap and misses what it never had', () => {
        const cache = createThumbCache(3);
        expect(cache.get(WIDTH, 10)).toBe(null);
        cache.set(WIDTH, 10, 'frame 10');
        cache.set(WIDTH, 11, 'frame 11');
        expect(cache.get(WIDTH, 10)).toBe('frame 10');
        expect(cache.get(WIDTH, 11)).toBe('frame 11');
        expect(cache.get(WIDTH, 12)).toBe(null);
        expect(cache.size()).toBe(2);
    });

    it('overwrites the least recently used frame once full', () => {
        const cache = createThumbCache(3);
        cache.get(WIDTH, 0);
        for (const bucket of [10, 11, 12]) cache.set(WIDTH, bucket, `frame ${bucket}`);
        // 10 is the oldest write, but reading it makes 11 the stalest.
        expect(cache.get(WIDTH, 10)).toBe('frame 10');
        cache.set(WIDTH, 13, 'frame 13');
        expect(cache.size()).toBe(3);
        expect(cache.get(WIDTH, 11)).toBe(null);
        // One out for each one in: 12 was the stalest left.
        cache.set(WIDTH, 14, 'frame 14');
        expect(cache.get(WIDTH, 12)).toBe(null);
        expect(cache.get(WIDTH, 10)).toBe('frame 10');
        expect(cache.get(WIDTH, 13)).toBe('frame 13');
        expect(cache.get(WIDTH, 14)).toBe('frame 14');
    });

    it('rewrites a held bucket without evicting another', () => {
        const cache = createThumbCache(2);
        cache.get(WIDTH, 0);
        cache.set(WIDTH, 1, 'a');
        cache.set(WIDTH, 2, 'b');
        cache.set(WIDTH, 1, 'a again');
        expect(cache.size()).toBe(2);
        expect(cache.get(WIDTH, 2)).toBe('b');
        expect(cache.get(WIDTH, 1)).toBe('a again');
    });

    it('drops every frame when the width changes, and refuses late ones for the old width', () => {
        const cache = createThumbCache(3);
        cache.get(WIDTH, 0);
        cache.set(WIDTH, 1, 'small 1');
        cache.set(WIDTH, 2, 'small 2');
        // The window moved to a denser screen.
        expect(cache.get(WIDTH * 2, 1)).toBe(null);
        expect(cache.size()).toBe(0);
        // A capture requested before the move lands after it.
        cache.set(WIDTH, 3, 'small 3');
        expect(cache.size()).toBe(0);
        cache.set(WIDTH * 2, 1, 'large 1');
        expect(cache.get(WIDTH * 2, 1)).toBe('large 1');
    });

    it('holds nothing before a width is asked for, or after a clear', () => {
        const cache = createThumbCache(3);
        cache.set(WIDTH, 1, 'unasked');
        expect(cache.size()).toBe(0);
        cache.get(WIDTH, 1);
        cache.set(WIDTH, 1, 'asked');
        cache.clear();
        expect(cache.size()).toBe(0);
        expect(cache.get(WIDTH, 1)).toBe(null);
        // The width survives a clear: a frame for it can land straight away.
        cache.clear();
        cache.set(WIDTH, 2, 'after clear');
        expect(cache.get(WIDTH, 2)).toBe('after clear');
    });

    it('ships with the named cap', () => {
        const cache = createThumbCache();
        cache.get(WIDTH, 0);
        for (let bucket = 0; bucket < THUMB_CACHE_MAX + 5; bucket += 1) cache.set(WIDTH, bucket, 'x');
        expect(cache.size()).toBe(THUMB_CACHE_MAX);
        expect(cache.get(WIDTH, 0)).toBe(null);
        expect(cache.get(WIDTH, THUMB_CACHE_MAX + 4)).toBe('x');
    });
});
