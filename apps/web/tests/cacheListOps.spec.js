// The Cache page's sort / filter / search decision table.

const { SORTS, FILTERS, sortEntries, filterEntries } = require('../src/routes/Cached/cacheListOps');

// Quality stub keyed by name: tests inject parsing instead of depending on
// streamQuality's real tokenizer (covered by its own usage).
const QUALITY = {
    'old-4k-hdr': { resolution: 2160, hdr: true },
    'new-1080p': { resolution: 1080, hdr: false },
    'mid-4k': { resolution: 2160, hdr: false },
    'unknown': { resolution: 0, hdr: false },
};
const getQuality = (entry) => QUALITY[entry.name] || { resolution: 0, hdr: false };
const getDisplayName = (entry) => entry.title || entry.name || '';

const entry = (name, over = {}) => ({
    infoHash: name,
    name,
    downloaded: 0,
    total: 0,
    state: 'live',
    pinned: false,
    watched: false,
    fileCount: 1,
    addedAt: 0,
    ...over,
});

const names = (entries) => entries.map((e) => e.name);

describe('cache sort', () => {
    const list = [
        entry('old-4k-hdr', { addedAt: 100, downloaded: 900 }),
        entry('new-1080p', { addedAt: 300, downloaded: 100 }),
        entry('mid-4k', { addedAt: 200, downloaded: 500 }),
    ];

    it('date sorts newest first (the default)', () => {
        expect(names(sortEntries(list, 'date', getQuality))).toEqual(['new-1080p', 'mid-4k', 'old-4k-hdr']);
    });

    it('an entry with no addedAt sorts oldest, not on top', () => {
        const withMissing = [...list, entry('unknown', { downloaded: 1 })];
        expect(names(sortEntries(withMissing, 'date', getQuality)).pop()).toBe('unknown');
    });

    it('"Recent" ranks by the recency accessor: a download played just now outranks a newer add', () => {
        const playedAt = { 'old-4k-hdr': 500 };
        const recency = (e) => Math.max(e.addedAt, playedAt[e.name] || 0);
        expect(names(sortEntries(list, 'date', getQuality, recency))).toEqual(['old-4k-hdr', 'new-1080p', 'mid-4k']);
    });

    it('size sorts largest first', () => {
        expect(names(sortEntries(list, 'size', getQuality))).toEqual(['old-4k-hdr', 'mid-4k', 'new-1080p']);
    });

    it('quality sorts by resolution, HDR breaking ties', () => {
        expect(names(sortEntries(list, 'quality', getQuality))).toEqual(['old-4k-hdr', 'mid-4k', 'new-1080p']);
    });

    it('returns a new array (React state must not be mutated in place)', () => {
        const before = names(list);
        sortEntries(list, 'size', getQuality);
        expect(names(list)).toEqual(before);
    });
});

describe('cache filters', () => {
    const list = [
        entry('old-4k-hdr', { downloaded: 10, total: 10 }),
        entry('new-1080p', { downloaded: 5, total: 10 }),
        entry('mid-4k', { downloaded: 10, total: 10, pinned: true }),
        entry('unknown', { state: 'error', total: 10 }),
    ];

    it('4k / 1080p / hdr follow the parsed quality', () => {
        expect(names(filterEntries(list, ['4k'], '', getQuality, getDisplayName))).toEqual(['old-4k-hdr', 'mid-4k']);
        expect(names(filterEntries(list, ['1080p'], '', getQuality, getDisplayName))).toEqual(['new-1080p']);
        expect(names(filterEntries(list, ['hdr'], '', getQuality, getDisplayName))).toEqual(['old-4k-hdr']);
    });

    it('downloading means incomplete and not failed; complete means done', () => {
        expect(names(filterEntries(list, ['downloading'], '', getQuality, getDisplayName))).toEqual(['new-1080p']);
        expect(names(filterEntries(list, ['complete'], '', getQuality, getDisplayName))).toEqual(['old-4k-hdr', 'mid-4k']);
    });

    it('kept means pinned', () => {
        expect(names(filterEntries(list, ['kept'], '', getQuality, getDisplayName))).toEqual(['mid-4k']);
    });

    it('active filters combine with AND', () => {
        expect(names(filterEntries(list, ['4k', 'kept'], '', getQuality, getDisplayName))).toEqual(['mid-4k']);
        expect(names(filterEntries(list, ['1080p', 'kept'], '', getQuality, getDisplayName))).toEqual([]);
    });

    it('no active filters passes everything', () => {
        expect(filterEntries(list, [], '', getQuality, getDisplayName)).toHaveLength(list.length);
    });
});

describe('cache search', () => {
    const list = [
        entry('Silo.S03E07.2160p.WEB.mkv', { title: 'Silo S03E07' }),
        entry('strange.new.worlds.s04e03.mkv', { title: 'Star Trek: Strange New Worlds S04E03' }),
    ];

    it('matches the display title case-insensitively', () => {
        expect(names(filterEntries(list, [], 'star trek', getQuality, getDisplayName)))
            .toEqual(['strange.new.worlds.s04e03.mkv']);
    });

    it('matches the raw scene filename too', () => {
        expect(names(filterEntries(list, [], '2160P', getQuality, getDisplayName)))
            .toEqual(['Silo.S03E07.2160p.WEB.mkv']);
    });

    it('whitespace-only queries pass everything', () => {
        expect(filterEntries(list, [], '   ', getQuality, getDisplayName)).toHaveLength(2);
    });

    it('search and filters compose', () => {
        expect(filterEntries(list, ['4k'], 'silo', getQuality, getDisplayName)).toHaveLength(0);
    });
});

describe('toolbar vocabularies', () => {
    it('exports the sort and filter keys the UI renders', () => {
        expect(SORTS).toEqual(['date', 'size', 'quality']);
        expect(FILTERS).toEqual(['4k', '1080p', 'hdr', 'downloading', 'complete', 'kept']);
    });
});
