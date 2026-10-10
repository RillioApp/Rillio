// "On this device" on a title's streams list: which cached file is the one
// being viewed. Matched PER FILE, not per torrent, so a season pack that holds
// the episode is offered on that episode's page even when the torrent was
// first played from (and recorded as) another episode.

const { matchCachedEpisode, tiedEntries, isViewedEpisode } = require('../src/routes/MetaDetails/StreamsList/cachedEpisodeMatch');
const { playerDeepLink } = require('../src/common/playerDeepLink');

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

// The owner's S1E2 page showed "Downloaded 35.48 GB" and "22.51 GB": the whole
// selection of each pack. A row's size and state are the episode file's.
describe('size and progress are the episode file\'s, never the torrent\'s', () => {
    test('episode file complete, torrent still downloading: complete, file-sized', () => {
        const files = packFiles();
        files[4] = { ...files[4], selected: true, downloaded: 1 * GB };
        const entry = packEntry({ downloaded: 34 * GB, total: 46 * GB });
        const [offer] = matchCachedEpisode({ entries: [entry], filesByHash: { [PACK]: files }, ...viewing(3) });
        expect(offer.downloaded).toBe(11 * GB);
        expect(offer.total).toBe(11 * GB);
        expect(offer.downloaded >= offer.total).toBe(true);
    });

    test('torrent selection complete, episode file partial: partial, file-sized', () => {
        // The entry claims its selection is done (as /cache/list reports for a
        // finished selection), but this file is only partly here.
        const files = packFiles();
        files[2] = { ...files[2], downloaded: 4 * GB };
        const entry = packEntry({ downloaded: 33 * GB, total: 33 * GB });
        const [offer] = matchCachedEpisode({ entries: [entry], filesByHash: { [PACK]: files }, ...viewing(3) });
        expect(offer.downloaded).toBe(4 * GB);
        expect(offer.total).toBe(11 * GB);
        expect(offer.downloaded < offer.total).toBe(true);
    });
});

describe('single-episode torrents and numbering', () => {
    test('a lone S01E02 torrent recorded as E02 is not offered on E03', () => {
        const ih = 'c'.repeat(40);
        const files = [{ index: 0, path: 'For.All.Mankind.S01E02.1080p.mkv', length: 3 * GB, downloaded: 3 * GB, selected: true, video: true }];
        const entry = packEntry({ infoHash: ih, fileIdx: 0, fileCount: 1, meta: showMeta({ fileIdx: 0 }) });
        expect(matchCachedEpisode({ entries: [entry], filesByHash: { [ih]: files }, ...viewing(3) })).toEqual([]);
        expect(matchCachedEpisode({ entries: [entry], filesByHash: { [ih]: files }, ...viewing(2) })[0].fileIdx).toBe(0);
    });

    test('a double-episode file is offered on BOTH episodes\' pages', () => {
        const ih = 'e'.repeat(40);
        const files = [
            { index: 0, path: `${DIR}/Mankind.S01E03E04.2160p.mp4`, length: 15 * GB, downloaded: 15 * GB, selected: true, video: true },
            { index: 1, path: `${DIR}/Mankind.S01E05.2160p.mp4`, length: 8 * GB, downloaded: 8 * GB, selected: true, video: true },
        ];
        const entry = packEntry({ infoHash: ih, meta: showMeta({ videoId: null, fileIdx: null }) });
        for (const n of [3, 4]) {
            const [offer] = matchCachedEpisode({ entries: [entry], filesByHash: { [ih]: files }, ...viewing(n) });
            expect(offer.fileIdx).toBe(0);
            expect(offer.meta.videoId).toBe(`${SHOW}:1:${n}`);
        }
        expect(matchCachedEpisode({ entries: [entry], filesByHash: { [ih]: files }, ...viewing(2) })).toEqual([]);
        expect(isViewedEpisode({ season: 1, episode: 3, lastEpisode: 5 }, 1, 4)).toBe(true);
        expect(isViewedEpisode({ season: 1, episode: 3, lastEpisode: 5 }, 1, 6)).toBe(false);
    });

    test('absolute numbering matches season 1 or a seasonless addon, never a later season', () => {
        const parsed = { season: null, episode: 3 };
        expect(isViewedEpisode(parsed, 1, 3)).toBe(true);
        expect(isViewedEpisode(parsed, null, 3)).toBe(true);
        expect(isViewedEpisode(parsed, 2, 3)).toBe(false);
        expect(isViewedEpisode({ season: 2, episode: 3 }, 2, 3)).toBe(true);
        expect(isViewedEpisode({ season: 2, episode: 3 }, 1, 3)).toBe(false);
        expect(isViewedEpisode({ season: 1, episode: 4 }, 1, 3)).toBe(false);
        expect(isViewedEpisode(null, 1, 3)).toBe(false);
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

// An offer found by file name, for a torrent with no sidecar of this title
// (tied by an addon stream that carries no fileIdx), must still open the
// player as a catalog play would: title, episode, meta and stream addons, so
// library progress, the next episode and artwork all work. The page knows all
// of that; only a bare /player/:stream link would lose it.
describe('a filename-matched offer keeps the episode context', () => {
    const CINEMETA = 'https://v3-cinemeta.strem.io/manifest.json';
    const TORRENTIO = 'https://torrentio.example/manifest.json';
    const page = { type: 'series', name: 'For All Mankind', metaTransportUrl: CINEMETA };
    // The addon lists the pack for this episode but without a fileIdx (it
    // would play the largest file), so the episode's file is found by name.
    const hashOnlyStream = { infoHash: PACK, addonTransportUrl: TORRENTIO, deepLinks: { player: '#/player/pack-largest' } };

    test('the deep link is built from this page: title, episode, both addons, the resolved file', () => {
        const [offer] = matchCachedEpisode({
            entries: [packEntry({ meta: undefined })],
            filesByHash: { [PACK]: packFiles() },
            ...viewing(3, { addonStreams: [hashOnlyStream], page }),
        });
        expect(offer.fileIdx).toBe(2);
        // The addon's own link would open the largest file, not E03.
        expect(offer.playHref).toBeNull();
        expect(offer.meta).toMatchObject({
            metaId: SHOW,
            type: 'series',
            videoId: `${SHOW}:1:3`,
            season: 1,
            episode: 3,
            fileIdx: 2,
            metaTransportUrl: CINEMETA,
            streamTransportUrl: TORRENTIO,
        });
        expect(playerDeepLink('ENC', offer.meta)).toBe(
            `/player/ENC/${encodeURIComponent(TORRENTIO)}/${encodeURIComponent(CINEMETA)}` +
            `/series/${SHOW}/${encodeURIComponent(`${SHOW}:1:3`)}`,
        );
    });

    test('a movie tied only by an addon gets the same full link', () => {
        const MOVIE = 'tt0000102';
        const ih = 'f'.repeat(40);
        const [offer] = matchCachedEpisode({
            entries: [{ infoHash: ih, name: 'Movie.B.2004.mkv', downloaded: 9 * GB, total: 9 * GB, state: 'live', fileCount: 1, fileIdx: 0 }],
            filesByHash: {},
            metaId: MOVIE,
            videoId: MOVIE,
            season: null,
            episode: null,
            addonStreams: [{ infoHash: ih, addonTransportUrl: TORRENTIO }],
            page: { type: 'movie', name: 'Movie B', metaTransportUrl: CINEMETA },
        });
        expect(playerDeepLink('ENC', offer.meta)).toBe(
            `/player/ENC/${encodeURIComponent(TORRENTIO)}/${encodeURIComponent(CINEMETA)}/movie/${MOVIE}/${MOVIE}`,
        );
    });
});

// A torrent shared by several titles (a collection pack, a season pack) must
// play the file THIS page is about. The sidecar's file belongs to whatever was
// played when it was written; an addon stream on this page resolved this very
// title/episode, so its fileIdx wins.
describe('a shared torrent plays the file this page is about', () => {
    const COLLECTION = 'd'.repeat(40);
    const MOVIE_A = 'tt0000101';
    const MOVIE_B = 'tt0000102';
    const collectionFiles = () => [
        { index: 0, path: 'Collection/Movie.A.2001.1080p.mkv', length: 8 * GB, downloaded: 8 * GB, selected: true, video: true },
        { index: 1, path: 'Collection/Movie.B.2004.1080p.mkv', length: 9 * GB, downloaded: 3 * GB, selected: true, video: true },
    ];
    // Cached while playing movie A: the sidecar names A and its file 0, and
    // the server's playable file for the selection is 0 too.
    const collectionEntry = () => ({
        infoHash: COLLECTION,
        name: 'Movie.A.2001.1080p.mkv',
        downloaded: 11 * GB,
        total: 17 * GB,
        state: 'live',
        fileCount: 2,
        fileIdx: 0,
        meta: { metaId: MOVIE_A, type: 'movie', name: 'Movie A', poster: null, background: null, logo: null, year: '2001', videoId: null, fileIdx: 0 },
    });
    const streamFor = (fileIdx) => ({
        infoHash: COLLECTION,
        fileIdx,
        addonTransportUrl: 'https://torrentio.example/manifest.json',
        deepLinks: { player: `#/player/encoded-${fileIdx}/torrentio/cinemeta/movie/${MOVIE_B}/${MOVIE_B}` },
    });

    test('movie B\'s page plays file 1 (the addon\'s), never movie A\'s file 0', () => {
        const [offer] = matchCachedEpisode({
            entries: [collectionEntry()],
            filesByHash: { [COLLECTION]: collectionFiles() },
            metaId: MOVIE_B,
            videoId: MOVIE_B,
            season: null,
            episode: null,
            addonStreams: [streamFor(1)],
        });
        expect(offer.fileIdx).toBe(1);
        // And it is sized as that file, not the collection's selection.
        expect(offer.downloaded).toBe(3 * GB);
        expect(offer.total).toBe(9 * GB);
        // The sidecar describes movie A: it must not steer B's deep link.
        expect(offer.meta === undefined || offer.meta.metaId === MOVIE_B).toBe(true);
        expect(offer.playHref).toBe(streamFor(1).deepLinks.player);
    });

    test('movie A\'s own page still uses its recorded file', () => {
        const [offer] = matchCachedEpisode({
            entries: [collectionEntry()],
            filesByHash: { [COLLECTION]: collectionFiles() },
            metaId: MOVIE_A,
            videoId: MOVIE_A,
            season: null,
            episode: null,
            addonStreams: [],
        });
        expect(offer.fileIdx).toBe(0);
        expect(offer.downloaded).toBe(8 * GB);
        expect(offer.meta.metaId).toBe(MOVIE_A);
    });

    test('series: an addon on this page outranks the sidecar recorded for this very episode', () => {
        // The sidecar says E03 is file 2; the addon serving this page says 3.
        const [offer] = matchCachedEpisode({
            entries: [packEntry({ meta: showMeta({ videoId: `${SHOW}:1:3`, episode: 3, fileIdx: 2 }) })],
            filesByHash: { [PACK]: packFiles() },
            ...viewing(3, { addonStreams: [{ infoHash: PACK, fileIdx: 3 }] }),
        });
        expect(offer.fileIdx).toBe(3);
    });

    test('series: a sidecar of ANOTHER title never picks the file', () => {
        // Recorded while playing another show's E03 at file 2; this page's
        // addon resolves this show's episode to file 4.
        const [offer] = matchCachedEpisode({
            entries: [packEntry({ meta: showMeta({ metaId: 'tt0000999', videoId: 'tt0000999:1:3', fileIdx: 2 }) })],
            filesByHash: { [PACK]: packFiles().map((file) => ({ ...file, selected: true, downloaded: file.length })) },
            ...viewing(3, { addonStreams: [{ infoHash: PACK, fileIdx: 4 }] }),
        });
        expect(offer.fileIdx).toBe(4);
        expect(offer.meta === undefined || offer.meta.metaId === SHOW).toBe(true);
    });
});
