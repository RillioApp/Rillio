// What each Cache window row and the hero say: which title, which release,
// how much is on disk, what was watched and when. Built on the synthetic cache
// the owner's confusing case reduces to: three downloads of one show at once
// (a season pack and two copies of one episode), a movie, and torrents no
// title was matched to.

const {
    splitFiles, describeEntry, episodeRows, itemView, packStrip, packSummary,
    diskText, viewText, copyNotes, relativeTime, exactTime, clockTime, recencyOf, primaryAction, rowFacts, etaText, lastPlayOf,
} = require('../src/routes/Cached/cacheRowModel');

const NOW = Date.parse('2026-10-25T14:00:00');
const HOUR = 3600 * 1000;
const DAY = 24 * HOUR;
const MB = 1024 * 1024;

const SEVERANCE = {
    metaId: 'tt11280740', type: 'series', name: 'Severance', poster: null, background: null,
    logo: null, year: '2022-', videoId: 'tt11280740:2:3', season: 2, episode: 3,
    metaTransportUrl: 'https://v3-cinemeta.strem.io/manifest.json', streamTransportUrl: null,
};
const VIDEOS = [
    { id: 'tt11280740:1:1', season: 1, episode: 1, title: 'Good News About Hell' },
    { id: 'tt11280740:2:1', season: 2, episode: 1, title: 'Hello, Ms. Cobel' },
    { id: 'tt11280740:2:2', season: 2, episode: 2, title: 'Goodbye, Mrs. Selvig' },
    { id: 'tt11280740:2:3', season: 2, episode: 3, title: 'Who Is Alive?' },
    { id: 'tt11280740:2:4', season: 2, episode: 4, title: 'Woe\'s Hollow' },
    { id: 'tt11280740:2:5', season: 2, episode: 5, title: 'Trojan\'s Horse' },
    { id: 'tt11280740:0:1', season: 0, episode: 1, title: 'Special' },
];

const packEntry = {
    infoHash: 'pack', name: 'Severance.S02.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb',
    downloaded: 33 * MB, total: 42 * MB, state: 'paused', pinned: false, watched: false,
    fileCount: 3, addedAt: NOW - 3 * DAY, meta: { ...SEVERANCE, fileIdx: 2 },
};
const packFiles = [
    { index: 0, path: 'Severance.S02E01.Hello.Ms.Cobel.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb.mkv', length: 14 * MB, downloaded: 14 * MB, selected: true, video: true },
    { index: 1, path: 'Severance.S02E02.Goodbye.Mrs.Selvig.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb.mkv', length: 13 * MB, downloaded: 13 * MB, selected: true, video: true },
    { index: 2, path: 'Severance.S02E03.Who.Is.Alive.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb.mkv', length: 15 * MB, downloaded: 6 * MB, selected: true, video: true },
    { index: 3, path: 'Severance.S02E04.Woes.Hollow.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb.mkv', length: 14 * MB, downloaded: 0, selected: false, video: true },
    { index: 4, path: 'Severance.S02E05.Trojans.Horse.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb.mkv', length: 14 * MB, downloaded: 0, selected: false, video: true },
    { index: 5, path: 'NTb.nfo', length: 4096, downloaded: 4096, selected: false, video: false },
];
const uhdEntry = {
    infoHash: 'uhd', name: 'Severance.S02E03.Who.Is.Alive.2160p.ATVP.WEB-DL.DDP5.1.DV.HDR.H.265-FLUX.mkv',
    downloaded: 18 * MB, total: 30 * MB, state: 'live', pinned: false, watched: false,
    fileCount: 1, fileIdx: 0, addedAt: NOW - 2 * HOUR, meta: { ...SEVERANCE, fileIdx: 0 },
};
const webEntry = {
    infoHash: 'web', name: 'Severance.S02E03.1080p.WEB.H264-SuccessfulCrab.mkv',
    downloaded: 12 * MB, total: 12 * MB, state: 'paused', pinned: false, watched: true,
    fileCount: 1, fileIdx: 0, addedAt: NOW - DAY, meta: { ...SEVERANCE, fileIdx: 0 },
};
const duneEntry = {
    infoHash: 'dune', name: 'Dune.Part.Two.2024.2160p.UHD.BluRay.REMUX.DV.HDR10.HEVC.TrueHD.7.1.Atmos-FGT.mkv',
    downloaded: 40 * MB, total: 40 * MB, state: 'paused', pinned: true, watched: false, fileCount: 1, fileIdx: 0,
    addedAt: NOW - 5 * DAY,
    meta: { metaId: 'tt15239678', type: 'movie', name: 'Dune: Part Two', year: '2024', videoId: null, season: null, episode: null, fileIdx: 0 },
};
const orphanEntry = {
    infoHash: 'orphan', name: 'www.Torrenting.com - Some.Indie.Thing.2019.WEBRip.x264-ION10.mp4',
    downloaded: 2 * MB, total: 9 * MB, state: 'paused', pinned: false, watched: false, fileCount: 1, fileIdx: 0, addedAt: NOW - 40 * 60 * 1000,
};
const animeEntry = {
    infoHash: 'anime', name: '[SubsPlease] Sousou no Frieren - 12 (1080p) [5E0A1C2B].mkv',
    downloaded: 10 * MB, total: 10 * MB, state: 'paused', pinned: false, watched: true, watchedAt: NOW - 2 * DAY,
    fileCount: 1, fileIdx: 0, addedAt: NOW - 7 * DAY,
};

// Season 2's E01 and E02 watched: over VIDEOS in the core's order (S00E01,
// S01E01, S02E01, ...) that is bits 2 and 3, anchored on S02E02 at length 4.
const WATCHED_E1_E2 = 'tt11280740:2:2:4:eJzjAQAADQAN';
const libraryWith = (over) => ({
    removed: false, watched: true, temp: false, lastWatched: new Date(NOW - DAY).toISOString(),
    videoId: 'tt11280740:2:3', timeOffset: 28 * 60 * 1000, duration: 54 * 60 * 1000, watchedField: null, ...over,
});

describe('splitFiles: episodes vs other files', () => {
    test('every video that names an episode is an episode row; the .nfo is another file', () => {
        const { episodes, other } = splitFiles(packFiles);
        expect(episodes.map((row) => [row.season, row.episode, row.file.index])).toEqual([
            [2, 1, 0], [2, 2, 1], [2, 3, 2], [2, 4, 3], [2, 5, 4],
        ]);
        expect(other.map((file) => file.index)).toEqual([5]);
    });

    test('a sample with the same SxxEyy is another file, the big one is the episode', () => {
        const { episodes, other } = splitFiles([
            { index: 0, path: 'Show.S01E01.1080p.mkv', length: 900 * MB, downloaded: 0, selected: true, video: true },
            { index: 1, path: 'Sample/Show.S01E01.sample.mkv', length: 20 * MB, downloaded: 0, selected: false, video: true },
        ]);
        expect(episodes.map((row) => row.file.index)).toEqual([0]);
        expect(other.map((file) => file.index)).toEqual([1]);
    });

    test('extras, subtitles and images with no episode number are other files', () => {
        const { episodes, other } = splitFiles([
            { index: 0, path: 'Movie.2024.2160p.mkv', length: 9000 * MB, downloaded: 0, selected: true, video: true },
            { index: 1, path: 'Extras/Behind the Scenes.mkv', length: 300 * MB, downloaded: 0, selected: false, video: true },
            { index: 2, path: 'Subs/English.srt', length: 1, downloaded: 0, selected: false, video: false },
            { index: 3, path: 'poster.jpg', length: 1, downloaded: 0, selected: false, video: false },
        ]);
        expect(episodes).toEqual([]);
        expect(other.map((file) => file.index)).toEqual([0, 1, 2, 3]);
    });

    test('a combined file covers its whole range; anime absolute numbers have no season', () => {
        const { episodes } = splitFiles([
            { index: 0, path: 'Show.S01E01E02.mkv', length: 1, downloaded: 0, selected: true, video: true },
            { index: 1, path: '[Group] Show - 03 [1080p].mkv', length: 1, downloaded: 0, selected: true, video: true },
        ]);
        expect(episodes.map((row) => [row.season, row.episode, row.lastEpisode])).toEqual([[1, 1, 2], [null, 3, 3]]);
    });
});

describe('describeEntry: what the row is', () => {
    // The bug: the pack's sidecar recorded the episode played first (S02E03),
    // so the pack was labelled "Severance S02E03", same as both single copies.
    test('a season pack is labelled as a pack, not as the episode played first', () => {
        expect(describeEntry({ entry: packEntry, files: packFiles, videos: VIDEOS })).toMatchObject({
            kind: 'pack', title: 'Severance', subtitle: 'Season 2 pack · 5 episodes', unmatched: false,
        });
    });

    test('a pack is recognised from its name before its file list has loaded', () => {
        expect(describeEntry({ entry: packEntry, files: null, videos: null })).toMatchObject({
            kind: 'pack', subtitle: 'Season 2 pack',
        });
    });

    // An empty or unreadable file list is no evidence: the pack name stands,
    // and the sidecar's first-played episode never labels the whole pack.
    test('a pack stays a pack when its file list is empty or names no episodes', () => {
        for (const files of [
            [],
            [
                { index: 0, path: 'Severance Season 2/Disc A.mkv', length: 9, downloaded: 0, selected: true, video: true },
                { index: 1, path: 'Severance Season 2/Disc B.mkv', length: 9, downloaded: 0, selected: false, video: true },
            ],
        ]) {
            expect(describeEntry({ entry: packEntry, files, videos: VIDEOS })).toMatchObject({ kind: 'pack', subtitle: 'Season 2 pack' });
        }
    });

    test('a file list that positively shows ONE episode overrides a season-like name', () => {
        const files = [
            { index: 0, path: 'Severance.S02E03.Who.Is.Alive.1080p.mkv', length: 9, downloaded: 9, selected: true, video: true },
            { index: 1, path: 'NTb.nfo', length: 1, downloaded: 1, selected: true, video: false },
        ];
        expect(describeEntry({ entry: { ...packEntry, fileIdx: 0 }, files, videos: VIDEOS })).toMatchObject({ kind: 'episode', subtitle: 'S02E03 · Who Is Alive?' });
    });

    test('an episode carries its number and title', () => {
        expect(describeEntry({ entry: uhdEntry, files: null, videos: VIDEOS })).toMatchObject({
            kind: 'episode', title: 'Severance', subtitle: 'S02E03 · Who Is Alive?', season: 2, episode: 3,
        });
    });

    test('the episode number shows even before the episode titles have loaded', () => {
        expect(describeEntry({ entry: uhdEntry, files: null, videos: null }).subtitle).toBe('S02E03');
    });

    test('a movie carries its year', () => {
        expect(describeEntry({ entry: duneEntry, files: null, videos: null })).toMatchObject({
            kind: 'movie', title: 'Dune: Part Two', subtitle: '2024',
        });
    });

    test('an unidentified torrent gets a cleaned name and says it is not matched', () => {
        expect(describeEntry({ entry: orphanEntry, files: null, videos: null })).toMatchObject({
            kind: 'unidentified', title: 'Some Indie Thing', subtitle: '2019', unmatched: true,
        });
        expect(describeEntry({ entry: animeEntry, files: null, videos: null })).toMatchObject({
            kind: 'unidentified', title: 'Sousou no Frieren', subtitle: 'Episode 12', unmatched: true,
        });
    });

    test('two releases of one episode read the same title but differ by release', () => {
        const a = describeEntry({ entry: uhdEntry, files: null, videos: VIDEOS });
        const b = describeEntry({ entry: webEntry, files: null, videos: VIDEOS });
        expect(a.subtitle).toBe(b.subtitle);
        expect(a.release).not.toEqual(b.release);
        expect(a.release.map((part) => part.text)).toEqual(['2160p', 'ATVP WEB-DL', 'DV / HDR', 'FLUX']);
        expect(b.release.map((part) => part.text)).toEqual(['1080p', 'WEB', 'H.264', 'SuccessfulCrab']);
    });

    test('the release line of a pack reads the torrent name', () => {
        expect(describeEntry({ entry: packEntry, files: packFiles, videos: null }).release.map((part) => part.text))
            .toEqual(['1080p', 'ATVP WEB-DL', 'H.264', 'NTb']);
    });
});

describe('episodeRows: per-episode viewing state of a pack', () => {
    const { episodes } = splitFiles(packFiles);

    test('watched, in progress (only the library\'s current episode), not started', () => {
        const library = libraryWith({ watchedField: WATCHED_E1_E2 });
        const rows = episodeRows({ episodes, videos: VIDEOS, library });
        expect(rows.map((row) => [row.label, row.title, row.view && row.view.state])).toEqual([
            ['E01', 'Hello, Ms. Cobel', 'watched'],
            ['E02', 'Goodbye, Mrs. Selvig', 'watched'],
            ['E03', 'Who Is Alive?', 'partial'],
            ['E04', 'Woe\'s Hollow', 'unwatched'],
            ['E05', 'Trojan\'s Horse', 'unwatched'],
        ]);
        expect(rows[2].view).toMatchObject({ offset: 28 * 60 * 1000, duration: 54 * 60 * 1000 });
    });

    test('a title that is not in the library has no viewing state at all', () => {
        const rows = episodeRows({ episodes, videos: VIDEOS, library: undefined });
        expect(rows.every((row) => row.view === null)).toBe(true);
    });

    test('episode list unavailable (addon down): unknown, never "not started"', () => {
        const rows = episodeRows({ episodes, videos: null, library: libraryWith({}) });
        expect(rows.every((row) => row.view === null)).toBe(true);
    });

    test('an anchor the list does not hold: watched is unknown, the current episode is still in progress', () => {
        const library = libraryWith({ watchedField: 'tt9:1:4:4:eJxjBwAACAAI' });
        const rows = episodeRows({ episodes, videos: VIDEOS, library });
        expect(rows.map((row) => row.view && row.view.state)).toEqual([null, null, 'partial', null, null]);
    });

    test('a file that cannot be tied to an episode of the title says nothing', () => {
        const rows = episodeRows({
            episodes: splitFiles([{ index: 0, path: 'Severance.S07E01.mkv', length: 1, downloaded: 1, selected: true, video: true }]).episodes,
            videos: VIDEOS,
            library: libraryWith({}),
        });
        expect(rows[0].view).toBeNull();
        expect(rows[0].title).toBeNull();
    });

    test('absolute numbering ties to season 1; S00 ties to the specials', () => {
        const rows = episodeRows({
            episodes: splitFiles([
                { index: 0, path: '[G] Severance - 01 [1080p].mkv', length: 1, downloaded: 1, selected: true, video: true },
                { index: 1, path: 'Severance.S00E01.mkv', length: 1, downloaded: 1, selected: true, video: true },
            ]).episodes,
            videos: VIDEOS,
            library: libraryWith({ videoId: 'x', timeOffset: 0 }),
        });
        // Rows sort by season: the special first.
        expect(rows.map((row) => row.title)).toEqual(['Special', 'Good News About Hell']);
    });

    test('ids of another scheme tie by the meta\'s own season and episode', () => {
        const rows = episodeRows({
            episodes: splitFiles([{ index: 0, path: '[G] Frieren - 12 [1080p].mkv', length: 1, downloaded: 1, selected: true, video: true }]).episodes,
            videos: [{ id: 'kitsu:46474:12', season: 1, episode: 12, title: 'A Real Hero' }],
            library: libraryWith({ videoId: 'kitsu:46474:12', timeOffset: 0, watchedField: null }),
        });
        expect(rows[0]).toMatchObject({ title: 'A Real Hero', videoId: 'kitsu:46474:12' });
        // No bitfield: tied, but its viewing state is unknown.
        expect(rows[0].view).toBeNull();
    });

    // Absent or partial data says nothing.
    test('a library entry without a bitfield (null or old shape) never reads as "not started"', () => {
        for (const library of [
            libraryWith({ watchedField: null, videoId: 'x', timeOffset: 0 }),
            { removed: false, watched: true },
        ]) {
            const rows = episodeRows({ episodes, videos: VIDEOS, library });
            expect(rows.map((row) => row.view)).toEqual([null, null, null, null, null]);
        }
    });

    test('a bitfield without playback fields: watched marks hold, nothing is "in progress"', () => {
        const rows = episodeRows({ episodes, videos: VIDEOS, library: { removed: false, watched: true, videoId: 'tt11280740:2:3', watchedField: WATCHED_E1_E2 } });
        expect(rows.map((row) => row.view && row.view.state)).toEqual(['watched', 'watched', 'unwatched', 'unwatched', 'unwatched']);
    });
});

describe('the pack strip and its summary', () => {
    const { episodes } = splitFiles(packFiles);
    const rows = episodeRows({ episodes, videos: VIDEOS, library: libraryWith({ watchedField: WATCHED_E1_E2 }) });

    test('filled, partly filled, dark, dashed', () => {
        expect(packStrip(rows).map((bar) => bar.state)).toEqual(['watched', 'watched', 'partial', 'missing', 'missing']);
        expect(packStrip(rows)[2].fraction).toBeCloseTo(28 / 54);
    });

    test('without viewing state the strip only says what is on disk', () => {
        const unknown = episodeRows({ episodes, videos: null, library: undefined });
        expect(packStrip(unknown).map((bar) => bar.state)).toEqual(['disk', 'disk', 'disk', 'missing', 'missing']);
        expect(packSummary(unknown)).toBeNull();
    });

    test('a short summary', () => {
        expect(packSummary(rows)).toBe('E01 to E02 watched, E03 halfway');
    });

    const row = (season, episode, state) => ({
        label: `E${String(episode).padStart(2, '0')}`, season, episode, lastEpisode: episode,
        file: { index: episode, path: '', length: 1, downloaded: 1, selected: true, video: true },
        view: state === null ? null : { state },
    });

    test('ranges merge only consecutive episodes of one season', () => {
        // E02 unknown sits between: "E01 to E03" would claim it.
        expect(packSummary([row(1, 1, 'watched'), row(1, 2, null), row(1, 3, 'watched')])).toBe('E01, E03 watched');
        // E02 missing from the pack entirely.
        expect(packSummary([row(1, 1, 'watched'), row(1, 3, 'watched')])).toBe('E01, E03 watched');
        // Adjacent rows across a season boundary.
        expect(packSummary([row(1, 10, 'watched'), row(2, 1, 'watched')])).toBe('E10, E01 watched');
        expect(packSummary([row(1, 1, 'watched'), row(1, 2, 'watched'), row(1, 3, 'watched')])).toBe('E01 to E03 watched');
    });

    test('"Nothing watched yet" only when every episode is known', () => {
        expect(packSummary([row(1, 1, 'unwatched'), row(1, 2, null)])).toBeNull();
        expect(packSummary([row(1, 1, 'unwatched'), row(1, 2, 'unwatched')])).toBe('Nothing watched yet');
    });

    test('a combined file with one episode watched and one not is partly watched', () => {
        const combined = splitFiles([{ index: 0, path: 'Severance.S02E01E02.mkv', length: 9, downloaded: 9, selected: true, video: true }]).episodes;
        // Only S02E01 watched (bit 2 of the core's order), anchored on it.
        const library = libraryWith({ watchedField: 'tt11280740:2:1:3:eJxjAQAABQAF', videoId: 'x', timeOffset: 0 });
        const [only] = episodeRows({ episodes: combined, videos: VIDEOS, library });
        expect(only.view).toMatchObject({ state: 'partly' });
        expect(packStrip([only])[0]).toMatchObject({ state: 'partly', fraction: 0.5 });
    });
});

describe('itemView: one episode or a movie', () => {
    test('the episode the library is in the middle of', () => {
        const view = itemView({ entry: uhdEntry, described: describeEntry({ entry: uhdEntry, files: null, videos: VIDEOS }), videos: VIDEOS, library: libraryWith({}) });
        expect(view).toMatchObject({ state: 'partial', offset: 28 * 60 * 1000, duration: 54 * 60 * 1000 });
    });

    test('a movie watched to the end', () => {
        const view = itemView({
            entry: duneEntry, described: describeEntry({ entry: duneEntry, files: null, videos: null }), videos: null,
            library: { removed: false, watched: true, temp: false, lastWatched: null, videoId: 'tt15239678', timeOffset: 0, duration: 0, watchedField: null },
        });
        expect(view.state).toBe('watched');
    });

    test('a movie entry without playback fields says nothing rather than "not started"', () => {
        const described = describeEntry({ entry: duneEntry, files: null, videos: null });
        expect(itemView({ entry: duneEntry, described, videos: null, library: { removed: false, watched: false } })).toBeNull();
        expect(itemView({ entry: duneEntry, described, videos: null, library: { removed: false, watched: true } })).toEqual({ state: 'watched' });
        expect(itemView({
            entry: duneEntry, described, videos: null,
            library: { removed: false, watched: false, timeOffset: 0, duration: 0, videoId: 'tt15239678', watchedField: null, lastWatched: null, temp: false },
        })).toEqual({ state: 'unwatched' });
    });

    test('an unidentified torrent only knows the player\'s watched mark', () => {
        const described = describeEntry({ entry: animeEntry, files: null, videos: null });
        expect(itemView({ entry: animeEntry, described, videos: null, library: undefined })).toEqual({ state: 'watched', source: 'cache', at: NOW - 2 * DAY });
        // ...and says when (the server's watchedAt), not when it was added.
        expect(viewText({ view: itemView({ entry: animeEntry, described, videos: null, library: undefined }), recency: { kind: 'added', at: animeEntry.addedAt } }, NOW)).toBe('Watched 2 days ago');
        const orphan = describeEntry({ entry: orphanEntry, files: null, videos: null });
        expect(itemView({ entry: orphanEntry, described: orphan, videos: null, library: undefined })).toBeNull();
    });
});

describe('the right-hand lines', () => {
    test('on disk', () => {
        expect(diskText(webEntry, null)).toBe('Complete · 12 MB');
        expect(diskText(uhdEntry, null)).toBe('Downloading · 18 of 30 MB');
        expect(diskText({ ...uhdEntry, state: 'paused' }, null)).toBe('Paused · 18 of 30 MB');
        expect(diskText({ ...uhdEntry, state: 'initializing' }, null)).toBe('Preparing · 30 MB');
        expect(diskText(packEntry, splitFiles(packFiles).episodes)).toBe('3 of 5 on disk · 33 MB');
    });

    // Selected is not downloaded, and unavailable counters say nothing.
    test('on disk: issues, preparing and missing counters come before byte counts', () => {
        const episodes = splitFiles(packFiles).episodes;
        expect(diskText({ ...packEntry, issue: { kind: 'engine-error', reason: 'x' } }, episodes)).toBeNull();
        expect(diskText({ ...packEntry, state: 'waiting', downloaded: 0, total: 0, issue: { kind: 'metadata-missing', reason: '' } }, null)).toBeNull();
        expect(diskText({ ...packEntry, state: 'initializing' }, episodes)).toBe('Preparing · 42 MB');
        expect(diskText({ ...uhdEntry, state: 'live', downloaded: 0, total: 0 }, null)).toBeNull();
    });

    test('a selected episode with no bytes yet is not on disk', () => {
        const files = packFiles.map((file) => (file.index === 3 ? { ...file, selected: true } : file));
        const rows = episodeRows({ episodes: splitFiles(files).episodes, videos: null, library: undefined });
        expect(packStrip(rows).map((bar) => bar.state)).toEqual(['disk', 'disk', 'disk', 'missing', 'missing']);
        expect(diskText(packEntry, splitFiles(files).episodes)).toBe('3 of 5 on disk · 33 MB');
    });

    test('viewing and recency: library facts are about the episode, recency about this copy', () => {
        const play = { at: NOW - DAY, label: null };
        expect(viewText({ view: { state: 'watched' }, recency: { kind: 'added', at: NOW - 3 * DAY }, play, noun: 'Episode' }, NOW))
            .toBe('Episode watched yesterday');
        expect(viewText({ view: { state: 'watched' }, recency: { kind: 'added', at: NOW - 3 * DAY }, play: null, noun: 'Movie' }, NOW))
            .toBe('Movie watched · Added 3 days ago');
        expect(viewText({ view: { state: 'partial', offset: 432000, duration: 1440000 }, recency: { kind: 'added', at: NOW - 5 * DAY }, play: { at: NOW - 3 * DAY }, noun: 'Episode' }, NOW))
            .toBe('Episode stopped at 7:12 of 24:00 3 days ago');
        expect(viewText({ view: { state: 'unwatched' }, recency: { kind: 'added', at: NOW - 40 * 60 * 1000 }, play: null, noun: 'Episode' }, NOW))
            .toBe('Added 40 min ago · Episode not started');
        expect(viewText({ view: null, recency: { kind: 'added', at: NOW - 5 * DAY }, play: null }, NOW)).toBe('Added 5 days ago');
        // The player marked THIS torrent watched: about this copy.
        expect(viewText({ view: { state: 'watched', source: 'cache', at: NOW - 20 * 60 * 1000 }, recency: { kind: 'watched', at: NOW - 20 * 60 * 1000 } }, NOW))
            .toBe('Watched 20 min ago');
        expect(viewText({ view: null, recency: { kind: 'added', at: NOW - 3 * DAY }, play: { at: NOW - DAY, label: 'E02' } }, NOW)).toBe('E02 last played yesterday');
    });

    // The library cannot tell which copy of an episode was played: its play
    // time never dates or ranks a copy. Only this torrent's own evidence does.
    test('recency is this copy\'s: added, or watched here (watchedAt)', () => {
        expect(recencyOf({ entry: uhdEntry })).toEqual({ kind: 'added', at: uhdEntry.addedAt });
        expect(recencyOf({ entry: { ...webEntry, watchedAt: NOW - 20 * 60 * 1000 } })).toEqual({ kind: 'watched', at: NOW - 20 * 60 * 1000 });
        // A watched mark older than the add (a re-add) does not count.
        expect(recencyOf({ entry: { ...webEntry, watchedAt: webEntry.addedAt - DAY } })).toEqual({ kind: 'added', at: webEntry.addedAt });
    });

    test('two copies of one episode: same episode fact, each its own recency', () => {
        const library = libraryWith({ watchedField: WATCHED_E1_E2 });
        const a = rowFacts({ entry: { ...uhdEntry, addedAt: NOW - 3 * DAY }, files: null, videos: VIDEOS, library, addonNames: {}, now: NOW });
        const b = rowFacts({ entry: { ...webEntry, watched: false, addedAt: NOW - 2 * DAY }, files: null, videos: VIDEOS, library, addonNames: {}, now: NOW });
        expect(a.viewLine).toBe('Episode stopped at 28:00 of 54:00 yesterday');
        expect(b.viewLine).toBe('Episode stopped at 28:00 of 54:00 yesterday');
        expect(a.recencyAt).toBe(NOW - 3 * DAY);
        expect(b.recencyAt).toBe(NOW - 2 * DAY);
        expect(a.details).not.toMatch(/This copy watched/);
    });

    test('lastPlayOf: only the library\'s own last video, with its label', () => {
        const library = libraryWith({});
        expect(lastPlayOf({ videoIds: ['tt11280740:2:3'], library })).toEqual({ at: NOW - DAY, label: null });
        expect(lastPlayOf({ videoIds: ['tt11280740:2:1'], library })).toBeNull();
        expect(lastPlayOf({ videoIds: ['tt11280740:2:3'], library, labels: { 'tt11280740:2:3': 'E03' } })).toEqual({ at: NOW - DAY, label: 'E03' });
    });
});

describe('copyNotes: a second download of the same thing says so', () => {
    const rows = [
        { infoHash: 'uhd', kind: 'episode', metaId: 'tt11280740', season: 2, episode: 3 },
        { infoHash: 'web', kind: 'episode', metaId: 'tt11280740', season: 2, episode: 3 },
        { infoHash: 'pack', kind: 'pack', metaId: 'tt11280740', covers: [[2, 1], [2, 2], [2, 3], [2, 4], [2, 5]], label: 'Season 2 pack' },
        { infoHash: 'dune', kind: 'movie', metaId: 'tt15239678' },
        { infoHash: 'web2', kind: 'episode', metaId: 'tt11280740', season: 2, episode: 3 },
        { infoHash: 'e5', kind: 'episode', metaId: 'tt11280740', season: 2, episode: 5 },
    ];
    test('adjacent, further up, inside a pack above', () => {
        expect(copyNotes(rows)).toEqual({
            web: 'Another copy of the download above',
            web2: 'Another copy is further up',
            e5: 'Also in the Season 2 pack above',
        });
    });
});

describe('primaryAction', () => {
    const { episodes } = splitFiles(packFiles);
    test('a pack resumes the episode in progress', () => {
        const rows = episodeRows({ episodes, videos: VIDEOS, library: libraryWith({ watchedField: WATCHED_E1_E2 }) });
        expect(primaryAction({ entry: packEntry, kind: 'pack', rows })).toEqual({ label: 'Resume E03', fileIdx: 2, row: rows[2] });
    });
    test('otherwise the first downloaded episode not watched yet', () => {
        const rows = episodeRows({ episodes, videos: VIDEOS, library: libraryWith({ videoId: 'x', timeOffset: 0, watchedField: WATCHED_E1_E2 }) });
        expect(primaryAction({ entry: packEntry, kind: 'pack', rows })).toMatchObject({ label: 'Play E03', fileIdx: 2 });
    });
    test('a single file plays or resumes', () => {
        expect(primaryAction({ entry: uhdEntry, kind: 'episode', view: { state: 'partial' } })).toEqual({ label: 'Resume', fileIdx: 0, row: null });
        expect(primaryAction({ entry: webEntry, kind: 'episode', view: null })).toEqual({ label: 'Play', fileIdx: 0, row: null });
    });
    test('nothing to play for a row with an issue', () => {
        expect(primaryAction({ entry: { ...webEntry, issue: { kind: 'data-missing', reason: '' } }, kind: 'episode', view: null })).toBeNull();
    });
});

describe('times', () => {
    test('relative', () => {
        expect(relativeTime(NOW, NOW - 20 * 1000)).toBe('just now');
        expect(relativeTime(NOW, NOW - 12 * 60 * 1000)).toBe('12 min ago');
        expect(relativeTime(NOW, NOW - 2 * HOUR)).toBe('2 h ago');
        expect(relativeTime(NOW, NOW - DAY)).toBe('yesterday');
        expect(relativeTime(NOW, NOW - 3 * DAY)).toBe('3 days ago');
        expect(relativeTime(NOW, NOW - 8 * DAY)).toBe('1 week ago');
        expect(relativeTime(NOW, NOW - 20 * DAY)).toBe('2 weeks ago');
        expect(relativeTime(NOW, Date.parse('2026-08-03T10:00:00'))).toBe('3 Aug');
    });
    test('exact and clock', () => {
        expect(exactTime(Date.parse('2026-10-23T21:14:00'))).toBe('23 Oct, 21:14');
        expect(clockTime(432000)).toBe('7:12');
        expect(clockTime(3 * HOUR + 5 * 60 * 1000 + 9000)).toBe('3:05:09');
    });
});

describe('rowFacts: everything one row shows', () => {
    test('the pack: label, strip, on disk, action, details, copy key', () => {
        const facts = rowFacts({
            entry: { ...packEntry, meta: { ...packEntry.meta, streamTransportUrl: 'https://torrentio/manifest.json' } },
            files: packFiles,
            videos: VIDEOS,
            library: libraryWith({ watchedField: WATCHED_E1_E2 }),
            addonNames: { 'https://torrentio/manifest.json': 'Torrentio' },
            now: NOW,
        });
        expect(facts.kind).toBe('pack');
        expect(facts.described.subtitle).toBe('Season 2 pack · 5 episodes');
        expect(facts.disk).toBe('3 of 5 on disk · 33 MB');
        expect(facts.strip.map((bar) => bar.state)).toEqual(['watched', 'watched', 'partial', 'missing', 'missing']);
        expect(facts.summary).toBe('E01 to E02 watched, E03 halfway');
        expect(facts.action.label).toBe('Resume E03');
        expect(facts.viewLine).toBe('E03 last played yesterday');
        expect(facts.details).toBe(`From Torrentio · Added ${exactTime(packEntry.addedAt)} · E03 last played ${exactTime(NOW - DAY)} · 33 of 70 MB on disk`);
        expect(facts.other.map((file) => file.path)).toEqual(['NTb.nfo']);
        expect(facts.copyKey.covers).toEqual([[2, 1], [2, 2], [2, 3], [2, 4], [2, 5]]);
        expect(facts.recencyAt).toBe(packEntry.addedAt);
    });

    test('an unidentified torrent: cleaned name, no viewing claims, added recency', () => {
        const facts = rowFacts({ entry: orphanEntry, files: null, videos: undefined, library: undefined, addonNames: {}, now: NOW });
        expect(facts.described).toMatchObject({ title: 'Some Indie Thing', unmatched: true });
        expect(facts.view).toBeNull();
        expect(facts.viewLine).toBe('Added 40 min ago');
        expect(facts.poster).toBeNull();
    });

    test('an identified movie gets metahub poster art', () => {
        const facts = rowFacts({ entry: duneEntry, files: null, videos: undefined, library: undefined, addonNames: {}, now: NOW });
        expect(facts.poster).toBe('https://images.metahub.space/poster/small/tt15239678/img');
        expect(facts.disk).toBe('Complete · 40 MB');
    });
});

describe('etaText', () => {
    test('plain words', () => {
        expect(etaText(30)).toBe('under a minute left');
        expect(etaText(4 * 60 + 10)).toBe('about 4 min left');
        expect(etaText(2 * 3600 + 5 * 60)).toBe('about 2 h 5 min left');
        expect(etaText(Infinity)).toBeNull();
        expect(etaText(0)).toBeNull();
    });
});