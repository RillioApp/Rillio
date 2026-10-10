// "On this device" on a title's streams list: which cached file is the one
// being viewed. Matched PER FILE, not per torrent, so a season pack that holds
// the episode is offered on that episode's page even when the torrent was
// first played from (and recorded as) another episode.

const { matchCachedEpisode, tiedEntries } = require('../src/routes/MetaDetails/StreamsList/cachedEpisodeMatch');

const SHOW = 'tt7772588';
const PACK = 'a'.repeat(40);
const GB = 1024 ** 3;
const DIR = 'For.All.Mankind.S01.2160p.ATVP.WEB-DL.DDP5.1.Atmos.DoVi.HEVC-DVT';
const ep = (n, title) => `${DIR}/Mankind.S01E0${n}.${title}.2160p.ATVP.WEB-DL.DDP5.1.Atmos.DoVi.HEVC.by.DVT.mp4`;

// The owner's Cache page: E02-E04 complete, E01 and E05 never downloaded.
const packFiles = () => [
    { index: 0, path: ep(1, 'Red.Moon'), length: 9 * GB, downloaded: 0, selected: false, video: true },
    { index: 1, path: ep(2, 'He.Built.the.Saturn.V'), length: 10 * GB, downloaded: 10 * GB, selected: true, video: true },
    { index: 2, path: ep(3, 'Nixons.Women'), length: 11 * GB, downloaded: 11 * GB, selected: true, video: true },
    { index: 3, path: ep(4, 'Prime.Crew'), length: 12 * GB, downloaded: 12 * GB, selected: true, video: true },
    { index: 4, path: ep(5, 'Into.the.Abyss'), length: 13 * GB, downloaded: 0, selected: false, video: true },
    { index: 5, path: `${DIR}/${DIR}.nfo`, length: 4096, downloaded: 4096, selected: true, video: false },
];

// What the player's eager write records: the episode FIRST played (E02) and
// its file. A season pack has no entry-level fileIdx (no main feature).
const showMeta = (overrides = {}) => ({
    metaId: SHOW,
    type: 'series',
    name: 'For All Mankind',
    poster: null,
    background: null,
    logo: null,
    year: '2019-',
    videoId: `${SHOW}:1:2`,
    season: 1,
    episode: 2,
    metaTransportUrl: 'https://v3-cinemeta.strem.io/manifest.json',
    streamTransportUrl: 'https://torrentio.example/manifest.json',
    fileIdx: 1,
    ...overrides,
});

const packEntry = (overrides = {}) => ({
    infoHash: PACK,
    name: DIR,
    downloaded: 33 * GB + 4096,
    total: 33 * GB + 4096,
    state: 'live',
    pinned: false,
    fileCount: 4,
    meta: showMeta(),
    ...overrides,
});

const viewing = (episode, extra = {}) => ({
    metaId: SHOW,
    videoId: `${SHOW}:1:${episode}`,
    season: 1,
    episode,
    addonStreams: [],
    ...extra,
});

describe('a season pack recorded as another episode (the reported bug)', () => {
    test('viewing S1E3 offers the pack\'s E03 file, complete', () => {
        const offers = matchCachedEpisode({
            entries: [packEntry()],
            filesByHash: { [PACK]: packFiles() },
            ...viewing(3),
        });
        expect(offers).toHaveLength(1);
        const [offer] = offers;
        expect(offer.infoHash).toBe(PACK);
        expect(offer.fileIdx).toBe(2);
        expect(offer.downloaded).toBe(11 * GB);
        expect(offer.total).toBe(11 * GB);
        expect(offer.name).toBe('Mankind.S01E03.Nixons.Women.2160p.ATVP.WEB-DL.DDP5.1.Atmos.DoVi.HEVC.by.DVT.mp4');
        // The deep link must open THIS episode, not the one first recorded.
        expect(offer.meta.videoId).toBe(`${SHOW}:1:3`);
        expect(offer.meta.season).toBe(1);
        expect(offer.meta.episode).toBe(3);
        expect(offer.meta.fileIdx).toBe(2);
        expect(offer.meta.metaTransportUrl).toBe('https://v3-cinemeta.strem.io/manifest.json');
    });

    test('the recorded episode still resolves to its recorded file', () => {
        const [offer] = matchCachedEpisode({
            entries: [packEntry()],
            filesByHash: { [PACK]: packFiles() },
            ...viewing(2),
        });
        expect(offer.fileIdx).toBe(1);
        expect(offer.downloaded).toBe(10 * GB);
    });

    test('a pack the matcher identified (no videoId, no fileIdx) resolves per file too', () => {
        const [offer] = matchCachedEpisode({
            entries: [packEntry({ meta: showMeta({ videoId: null, season: null, episode: null, fileIdx: null }) })],
            filesByHash: { [PACK]: packFiles() },
            ...viewing(4),
        });
        expect(offer.fileIdx).toBe(3);
        expect(offer.downloaded).toBe(12 * GB);
    });
});

describe('the episode\'s file is not here', () => {
    test('never downloaded (not selected, 0 bytes): not offered', () => {
        expect(matchCachedEpisode({
            entries: [packEntry()],
            filesByHash: { [PACK]: packFiles() },
            ...viewing(5),
        })).toEqual([]);
    });

    test('downloading: offered with that file\'s own progress', () => {
        const files = packFiles();
        files[4] = { ...files[4], selected: true, downloaded: 2 * GB };
        const [offer] = matchCachedEpisode({
            entries: [packEntry()],
            filesByHash: { [PACK]: files },
            ...viewing(5),
        });
        expect(offer.fileIdx).toBe(4);
        expect(offer.downloaded).toBe(2 * GB);
        expect(offer.total).toBe(13 * GB);
    });

    test('a pack of another season: not offered', () => {
        const files = packFiles().map((file) => ({ ...file, path: file.path.replace(/S01E0/, 'S02E0') }));
        expect(matchCachedEpisode({
            entries: [packEntry({ meta: showMeta({ videoId: null, fileIdx: null }) })],
            filesByHash: { [PACK]: files },
            ...viewing(3),
        })).toEqual([]);
    });

    test('the torrent\'s files could not be read: not offered (no guessing)', () => {
        expect(matchCachedEpisode({
            entries: [packEntry()],
            filesByHash: {},
            ...viewing(3),
        })).toEqual([]);
    });
});

describe('tying a torrent to the title', () => {
    const addonStream = (overrides = {}) => ({
        infoHash: PACK.toUpperCase(),
        fileIdx: 2,
        name: 'Torrentio\n4k',
        deepLinks: { player: '#/player/encoded-e03/torrentio/cinemeta/series/tt7772588/tt7772588%3A1%3A3' },
        ...overrides,
    });

    test('no metadata, but an addon on this page lists the same torrent: offered, via the addon\'s own link', () => {
        const entries = [packEntry({ meta: undefined })];
        const addonStreams = [addonStream()];
        expect(tiedEntries(entries, SHOW, addonStreams).map((entry) => entry.infoHash)).toEqual([PACK]);
        const [offer] = matchCachedEpisode({
            entries,
            filesByHash: { [PACK]: packFiles() },
            ...viewing(3, { addonStreams }),
        });
        expect(offer.fileIdx).toBe(2);
        expect(offer.downloaded).toBe(11 * GB);
        expect(offer.playHref).toBe('#/player/encoded-e03/torrentio/cinemeta/series/tt7772588/tt7772588%3A1%3A3');
    });

    test('the addon\'s fileIdx is authoritative over file names', () => {
        // Unparseable names: only the addon knows which file is E03.
        const files = packFiles().map((file) => ({ ...file, path: `${DIR}/part${file.index}.mp4` }));
        const [offer] = matchCachedEpisode({
            entries: [packEntry({ meta: undefined })],
            filesByHash: { [PACK]: files },
            ...viewing(3, { addonStreams: [addonStream()] }),
        });
        expect(offer.fileIdx).toBe(2);
    });

    test('no metadata and no addon evidence: not tied, even though the files parse as this show\'s episodes', () => {
        const entries = [packEntry({ meta: undefined })];
        expect(tiedEntries(entries, SHOW, [])).toEqual([]);
        expect(matchCachedEpisode({
            entries,
            filesByHash: { [PACK]: packFiles() },
            ...viewing(3),
        })).toEqual([]);
    });

    test('metadata of another title: not tied', () => {
        expect(tiedEntries([packEntry({ meta: showMeta({ metaId: 'tt0000001' }) })], SHOW, [])).toEqual([]);
    });

    test('two files parse as the episode (a sample beside it): the larger one', () => {
        const files = [
            ...packFiles(),
            { index: 6, path: `${DIR}/Sample/Mankind.S01E03.sample.mp4`, length: 50 * 1024 ** 2, downloaded: 50 * 1024 ** 2, selected: true, video: true },
        ];
        const [offer] = matchCachedEpisode({
            entries: [packEntry({ meta: showMeta({ videoId: null, fileIdx: null }) })],
            filesByHash: { [PACK]: files },
            ...viewing(3),
        });
        expect(offer.fileIdx).toBe(2);
    });
});

describe('movies behave as before', () => {
    const MOVIE = 'tt0816692';
    const MOVIE_HASH = 'b'.repeat(40);
    const movieEntry = (overrides = {}) => ({
        infoHash: MOVIE_HASH,
        name: 'Interstellar.2014.2160p.UHD.BluRay.x265-GRP.mkv',
        downloaded: 20 * GB,
        total: 40 * GB,
        state: 'live',
        fileCount: 2,
        fileIdx: 0,
        meta: { metaId: MOVIE, type: 'movie', name: 'Interstellar', poster: null, background: null, logo: null, year: '2014', videoId: null, fileIdx: 0 },
        ...overrides,
    });

    test('the movie\'s torrent is offered with its entry-level progress and file, no file listing needed', () => {
        const [offer] = matchCachedEpisode({
            entries: [movieEntry()],
            filesByHash: {},
            metaId: MOVIE,
            videoId: MOVIE,
            season: null,
            episode: null,
            addonStreams: [],
        });
        expect(offer.infoHash).toBe(MOVIE_HASH);
        expect(offer.fileIdx).toBe(0);
        expect(offer.downloaded).toBe(20 * GB);
        expect(offer.total).toBe(40 * GB);
        expect(offer.meta.metaId).toBe(MOVIE);
    });

    test('another movie\'s torrent is not', () => {
        expect(matchCachedEpisode({
            entries: [movieEntry({ meta: { metaId: 'tt0000002', type: 'movie', name: 'Other' } })],
            filesByHash: {},
            metaId: MOVIE,
            videoId: MOVIE,
            season: null,
            episode: null,
            addonStreams: [],
        })).toEqual([]);
    });
});
