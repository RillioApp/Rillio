// Copyright (C) 2017-2026 Smart code 203358507

// What the Cache window says about each download, decided from evidence only.
//
//   - WHAT it is (describeEntry): the title with "S02E03 · Episode title", a
//     season pack as a pack ("Season 2 pack · 5 episodes": recognised from its
//     files or its name, never labelled by the episode that happened to be
//     played first, which is all its sidecar records), a movie with its year,
//     or, for a torrent no title was matched to, a cleaned name.
//   - WHICH copy (the release line, common/releaseInfo).
//   - What is ON DISK (diskText), per file for a pack (splitFiles: the video
//     files that name an episode are the episode rows, everything else stays
//     an "other file").
//   - What was WATCHED (episodeRows / itemView) from the user's library: the
//     watched bitfield decoded against the title's own episode list, plus the
//     time offset, which exists for ONE video only (the library's current
//     one), so only that episode can ever be "in progress". A file that cannot
//     be tied to an episode of the title, an episode list that could not be
//     fetched, or a title the library does not know: no state, never "not
//     started".
//   - WHEN (recencyOf): played, when the library's last video is this row's
//     and it was played after the download was added; added otherwise.
//
// Pure and dependency-free apart from the parsers, so the jest suite can
// require it directly (tests/cacheRowModel.spec.js).

const { parseEpisode, parseReleaseName } = require('../../common/releaseParse');
const { parseReleaseInfo, releaseLine, cleanName } = require('../../common/releaseInfo');
const { decodeWatched, orderVideoIds } = require('../../common/watchedBitfield');

const pad = (n) => String(n).padStart(2, '0');
const isNumber = (value) => typeof value === 'number' && Number.isFinite(value);
const basename = (path) => String(path).split(/[\\/]/).pop();

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/**
 * A torrent's files split into episode rows and other files. An episode row
 * is a video file whose path names an episode (common/releaseParse); when two
 * name the same one (an episode and its sample) the bigger is the episode and
 * the other stays an other file. Everything else (.nfo, subtitles, images,
 * extras, a movie's own file) is an other file. Both keep torrent order.
 */
const splitFiles = (files) => {
    const list = Array.isArray(files) ? files : [];
    const candidates = list
        .filter((file) => file.video)
        .map((file) => ({ file, parsed: parseEpisode(file.path) }))
        .filter((candidate) => candidate.parsed !== null);
    const best = new Map();
    candidates.forEach((candidate) => {
        const key = `${candidate.parsed.season}:${candidate.parsed.episode}`;
        const current = best.get(key);
        if (current === undefined || candidate.file.length > current.file.length) best.set(key, candidate);
    });
    const chosen = new Set([...best.values()].map((candidate) => candidate.file.index));
    const episodes = candidates
        .filter((candidate) => chosen.has(candidate.file.index))
        .map(({ file, parsed }) => ({ file, season: parsed.season, episode: parsed.episode, lastEpisode: parsed.lastEpisode }))
        // Absolute numbering sorts as season 1, which is what it equals.
        .sort((a, b) => ((a.season ?? 1) - (b.season ?? 1)) || (a.episode - b.episode));
    const other = list.filter((file) => !chosen.has(file.index));
    return { episodes, other };
};

const episodeCount = (episodes) => new Set(episodes.flatMap((row) => {
    const keys = [];
    for (let e = row.episode; e <= row.lastEpisode; e += 1) keys.push(`${row.season}:${e}`);
    return keys;
})).size;

// ---------------------------------------------------------------------------
// What the entry is
// ---------------------------------------------------------------------------

const episodeTag = (season, episode) => (isNumber(season) ? `S${pad(season)}E${pad(episode)}` : `Episode ${episode}`);

const videoFor = (videos, season, episode) => {
    if (!Array.isArray(videos) || !isNumber(episode)) return null;
    // Absolute numbering (no season in the name) equals per-season numbering
    // only in season 1 (see MetaDetails' isViewedEpisode).
    const wanted = isNumber(season) ? season : 1;
    return videos.find((video) => video && video.season === wanted && video.episode === episode) ?? null;
};

const videoTitle = (video) => {
    if (video === null) return null;
    const title = typeof video.title === 'string' && video.title.length > 0 ? video.title : video.name;
    return typeof title === 'string' && title.length > 0 ? title : null;
};

const packLabel = (episodes) => {
    const seasons = [...new Set(episodes.map((row) => row.season).filter(isNumber))].sort((a, b) => a - b);
    if (seasons.length === 1) return `Season ${seasons[0]} pack`;
    if (seasons.length > 1) return `Seasons ${seasons[0]} to ${seasons[seasons.length - 1]} pack`;
    return 'Pack';
};

const isPackName = (name) => {
    const parsed = parseReleaseName(name || '');
    return parsed.season !== null && parsed.episode === null;
};

/**
 * What the row is. Returns { kind: 'pack' | 'episode' | 'movie' | 'title' |
 * 'unidentified', title, subtitle, unmatched, season, episode, packLabel,
 * release } where release is the release line's parts.
 */
const describeEntry = ({ entry, files, videos }) => {
    const meta = entry.meta;
    const release = releaseLine(parseReleaseInfo(entry.name));
    const { episodes } = splitFiles(files);
    const isSeries = meta === undefined || meta.type !== 'movie';
    // The file list decides when it says something: two or more episodes is a
    // pack, exactly one is not. An empty or unreadable list (no episode names)
    // is no evidence, so the release name's season tag stands.
    const shown = Array.isArray(files) ? episodeCount(episodes) : 0;
    const pack = isSeries && (shown >= 2 || (shown === 0 && isPackName(entry.name)));
    const base = { release, season: null, episode: null, packLabel: null, unmatched: meta === undefined };

    if (pack) {
        const title = meta !== undefined ? meta.name : cleanName(entry.name).title;
        const label = shown >= 2 ?
            packLabel(episodes) :
            (() => {
                const season = parseReleaseName(entry.name).season;
                return isNumber(season) ? `Season ${season} pack` : 'Pack';
            })();
        return { ...base, kind: 'pack', title, packLabel: label, subtitle: shown >= 2 ? `${label} · ${shown} episodes` : label };
    }

    if (meta === undefined) {
        const clean = cleanName(entry.name);
        const subtitle = clean.episode !== null ? episodeTag(clean.season, clean.episode) : clean.year;
        return { ...base, kind: 'unidentified', title: clean.title, subtitle, season: clean.season, episode: clean.episode };
    }

    if (meta.type === 'movie') {
        const year = typeof meta.year === 'string' ? (meta.year.match(/\d{4}/) || [null])[0] : null;
        return { ...base, kind: 'movie', title: meta.name, subtitle: year };
    }

    // One episode: the file's own name is what the copy holds; the sidecar's
    // numbers (the player's record) stand in when the name says nothing.
    const parsed = parseEpisode(entry.name);
    const season = parsed !== null && isNumber(parsed.season) ? parsed.season : (isNumber(meta.season) ? meta.season : null);
    const episode = parsed !== null ? parsed.episode : (isNumber(meta.episode) ? meta.episode : null);
    if (!isNumber(episode)) return { ...base, kind: 'title', title: meta.name, subtitle: null };
    const name = videoTitle(videoFor(videos, season, episode));
    const tag = episodeTag(season, episode);
    return { ...base, kind: 'episode', title: meta.name, subtitle: name !== null ? `${tag} · ${name}` : tag, season, episode };
};

// ---------------------------------------------------------------------------
// What was watched
// ---------------------------------------------------------------------------

// A time offset this small is the core's placeholder for "moved on to this
// video" (advance_to_video sets 1 ms), not playback.
const MIN_OFFSET_MS = 1000;

// The viewing state of one video, or null when it cannot be known.
// watchedFlags: decoded bitfield aligned to orderedIds, or null (unknown).
const videoState = (videoId, library, watchedFlags, orderedIds) => {
    if (library === undefined || library === null || typeof videoId !== 'string') return null;
    const index = watchedFlags !== null ? orderedIds.indexOf(videoId) : -1;
    const watched = index >= 0 ? watchedFlags[index] : null;
    if (watched === true) return { state: 'watched' };
    if (library.videoId === videoId && isNumber(library.timeOffset) && isNumber(library.duration) &&
        library.timeOffset >= MIN_OFFSET_MS && library.duration > 0) {
        return { state: 'partial', offset: library.timeOffset, duration: library.duration };
    }
    if (watched === false) return { state: 'unwatched' };
    return null;
};

const librarySetup = (videos, library) => {
    const orderedIds = orderVideoIds(videos);
    const watchedFlags = library !== undefined && library !== null && Array.isArray(videos) ?
        decodeWatched(library.watchedField, orderedIds)
        :
        null;
    return { orderedIds, watchedFlags };
};

const episodeLabel = (row, multiSeason) => {
    const head = multiSeason || !isNumber(row.season) ? null : row.season;
    const one = (e) => (head !== null || !isNumber(row.season) ? `E${pad(e)}` : `S${pad(row.season)}E${pad(e)}`);
    if (!isNumber(row.season)) return row.lastEpisode > row.episode ? `${row.episode} to ${row.lastEpisode}` : String(row.episode);
    return row.lastEpisode > row.episode ? `${one(row.episode)} to ${one(row.lastEpisode)}` : one(row.episode);
};

/**
 * The episode rows of a pack with their titles and viewing state:
 * { label, title, videoId, file, season, episode, lastEpisode, view } where
 * view is { state: 'watched' | 'partial' | 'unwatched', offset?, duration? }
 * or null when it cannot be known. A combined file is watched when every
 * episode it covers is.
 */
const episodeRows = ({ episodes, videos, library }) => {
    const { orderedIds, watchedFlags } = librarySetup(videos, library);
    const multiSeason = new Set(episodes.map((row) => row.season)).size > 1;
    return episodes.map((row) => {
        const covered = [];
        for (let e = row.episode; e <= row.lastEpisode; e += 1) covered.push(videoFor(videos, row.season, e));
        const first = covered[0];
        const states = covered.map((video) => (video === null ? null : videoState(video.id, library, watchedFlags, orderedIds)));
        // A combined file: watched when every episode it covers is, partly
        // watched when some are, not watched only when none is; any unknown
        // episode leaves it unknown (an in-progress one still shows).
        let view = null;
        const partial = states.find((state) => state !== null && state.state === 'partial');
        if (partial) {
            view = partial;
        } else if (states.every((state) => state !== null)) {
            const watched = states.filter((state) => state.state === 'watched').length;
            view = watched === states.length ? { state: 'watched' } :
                watched > 0 ? { state: 'partly', watched, of: states.length } : { state: 'unwatched' };
        }
        return {
            label: episodeLabel(row, multiSeason),
            title: videoTitle(first),
            videoId: first !== null ? first.id : null,
            file: row.file,
            season: row.season,
            episode: row.episode,
            lastEpisode: row.lastEpisode,
            view,
        };
    });
};

// The player's own watched mark on the cache entry (it reports a stream
// watched at ~90%), with its time when the server sends it.
const cacheWatched = (entry) => {
    if (!entry.watched) return null;
    return isNumber(entry.watchedAt) ? { state: 'watched', source: 'cache', at: entry.watchedAt } : { state: 'watched', source: 'cache' };
};

/**
 * The viewing state of a single-item row (one episode, a movie), or null.
 * An unidentified torrent only knows the player's own watched mark on the
 * cache entry (source 'cache'); its absence says nothing.
 */
const itemView = ({ entry, described, videos, library }) => {
    const meta = entry.meta;
    if (described.kind === 'unidentified' || meta === undefined) {
        return cacheWatched(entry);
    }
    if (described.kind === 'movie') {
        if (library === undefined || library === null) return cacheWatched(entry);
        if (library.watched === true) return { state: 'watched' };
        // "Not watched" needs the playback fields to say so: an entry from
        // before they were sent (or a partial one) says nothing.
        if (!isNumber(library.timeOffset) || !isNumber(library.duration) || library.watched !== false) return cacheWatched(entry);
        if (library.timeOffset >= MIN_OFFSET_MS && library.duration > 0) {
            return { state: 'partial', offset: library.timeOffset, duration: library.duration };
        }
        return { state: 'unwatched' };
    }
    if (described.kind !== 'episode') return null;
    const video = videoFor(videos, described.season, described.episode);
    // Before the episode list has loaded, the sidecar's video id stands in
    // when it names this very episode (enough for "in progress").
    const videoId = video !== null ? video.id :
        meta.season === described.season && meta.episode === described.episode && typeof meta.videoId === 'string' ? meta.videoId : null;
    const { orderedIds, watchedFlags } = librarySetup(videos, library);
    const state = videoState(videoId, library, watchedFlags, orderedIds);
    if (state === null) return cacheWatched(entry);
    return state;
};

/** The video ids a row covers (for recency): its episodes, or the movie. */
const rowVideoIds = ({ entry, described, rows, videos }) => {
    if (described.kind === 'pack') return (rows || []).map((row) => row.videoId).filter(Boolean);
    if (entry.meta === undefined) return [];
    if (described.kind === 'movie') return [entry.meta.metaId, entry.meta.videoId].filter(Boolean);
    if (described.kind === 'episode') {
        const video = videoFor(videos, described.season, described.episode);
        if (video !== null) return [video.id];
        return entry.meta.season === described.season && entry.meta.episode === described.episode && entry.meta.videoId ? [entry.meta.videoId] : [];
    }
    return [];
};

// ---------------------------------------------------------------------------
// The pack strip
// ---------------------------------------------------------------------------

// On disk means bytes on disk. Being selected (queued for download, or
// streaming) is not; that is `playable`, what Play may start.
const onDisk = (file) => file.downloaded > 0;
const playable = (file) => file.selected === true;

/**
 * One bar per episode: 'watched' | 'partial' (with fraction) | 'unwatched' for
 * a downloaded episode whose state is known, 'disk' for a downloaded one whose
 * state is not, 'missing' for one not downloaded (and not watched either).
 */
const packStrip = (rows) => rows.map((row) => {
    const here = onDisk(row.file);
    const view = row.view;
    if (view !== null && view.state === 'watched') return { label: row.label, state: 'watched' };
    if (view !== null && view.state === 'partial') return { label: row.label, state: 'partial', fraction: Math.min(1, view.offset / view.duration) };
    if (view !== null && view.state === 'partly') return { label: row.label, state: 'partly', fraction: view.watched / view.of };
    if (!here) return { label: row.label, state: 'missing' };
    if (view !== null) return { label: row.label, state: 'unwatched' };
    return { label: row.label, state: 'disk' };
});

// Whether row b directly follows row a: the next episode of the same season.
const follows = (a, b) => a.season === b.season && b.episode === a.lastEpisode + 1;

const ranges = (labels, rows) => {
    // Runs of consecutive episodes of one season collapse to "E01 to E04";
    // a gap (an unknown, unwatched or absent episode) breaks the run.
    const out = [];
    let start = null;
    let prev = null;
    rows.forEach((row, index) => {
        if (labels.has(index) && start !== null && prev === index - 1 && follows(rows[prev], row)) {
            prev = index;
            return;
        }
        if (start !== null) out.push([start, prev]);
        start = labels.has(index) ? index : null;
        prev = start;
    });
    if (start !== null) out.push([start, prev]);
    return out.map(([a, b]) => (a === b ? rows[a].label : `${rows[a].label} to ${rows[b].label}`));
};

const progressWord = (fraction) => {
    if (fraction < 0.35) return 'started';
    if (fraction <= 0.65) return 'halfway';
    return 'nearly done';
};

/** "E01 to E02 watched, E03 halfway", or null when nothing is known. */
const packSummary = (rows) => {
    if (!rows.some((row) => row.view !== null)) return null;
    const watched = new Set(rows.map((row, i) => (row.view !== null && row.view.state === 'watched' ? i : -1)).filter((i) => i >= 0));
    const parts = [];
    const watchedRanges = ranges(watched, rows);
    if (watchedRanges.length > 0) parts.push(`${watchedRanges.join(', ')} watched`);
    const partly = rows.filter((row) => row.view !== null && row.view.state === 'partly').map((row) => row.label);
    if (partly.length > 0) parts.push(`${partly.join(', ')} partly watched`);
    const partial = rows.find((row) => row.view !== null && row.view.state === 'partial');
    if (partial) parts.push(`${partial.label} ${progressWord(partial.view.offset / partial.view.duration)}`);
    if (parts.length > 0) return parts.join(', ');
    // A negative summary needs every episode known.
    return rows.every((row) => row.view !== null) ? 'Nothing watched yet' : null;
};

// ---------------------------------------------------------------------------
// Sizes and times
// ---------------------------------------------------------------------------

const KB = 1024;
const MB = KB * 1024;
const GB = MB * 1024;
const sizeNumber = (bytes, unit) => {
    const value = bytes / unit;
    return value >= 100 ? String(Math.round(value)) : value.toFixed(1).replace(/\.0$/, '');
};
const unitOf = (bytes) => (bytes >= GB ? [GB, 'GB'] : bytes >= MB ? [MB, 'MB'] : [KB, 'KB']);

/** "6.1 GB", "344 MB", "12 KB". */
const formatSize = (bytes) => {
    const [unit, name] = unitOf(Math.max(0, bytes));
    return `${sizeNumber(Math.max(0, bytes), unit)} ${name}`;
};

/** "2.9 of 3.1 GB" (one unit, the total's), "18 of 30 MB". */
const formatOf = (done, total) => {
    const [unit, name] = unitOf(total);
    return `${sizeNumber(done, unit)} of ${sizeNumber(total, unit)} ${name}`;
};

/**
 * The on-disk line, or null when there is nothing honest to say. episodes:
 * splitFiles(...).episodes for a pack, else null. In order: a row with an
 * issue (or one waiting to be restored) states its issue instead of byte
 * counts it does not have; a torrent preparing has no have-count yet (packs
 * included); no total is no measurement.
 */
const diskText = (entry, episodes) => {
    if (entry.issue !== undefined || entry.state === 'waiting' || entry.state === 'error') return null;
    if (entry.state === 'initializing') return entry.total > 0 ? `Preparing · ${formatSize(entry.total)}` : 'Preparing';
    if (!(entry.total > 0)) return null;
    if (Array.isArray(episodes) && episodes.length >= 2) {
        const here = episodes.filter((row) => onDisk(row.file)).length;
        return `${here} of ${episodes.length} on disk · ${formatSize(entry.downloaded)}`;
    }
    if (entry.downloaded >= entry.total) return `Complete · ${formatSize(entry.downloaded)}`;
    const amount = formatOf(entry.downloaded, entry.total);
    if (entry.state === 'paused') return `Paused · ${amount}`;
    return `Downloading · ${amount}`;
};

/** "7:12", "3:05:09". */
const clockTime = (ms) => {
    const total = Math.max(0, Math.floor(ms / 1000));
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const s = total % 60;
    return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
};

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const dayMonth = (date) => `${date.getDate()} ${MONTHS[date.getMonth()]}`;

/** "23 Oct, 21:14" (local time). */
const exactTime = (ms) => {
    const date = new Date(ms);
    return `${dayMonth(date)}, ${pad(date.getHours())}:${pad(date.getMinutes())}`;
};

const startOfDay = (ms) => {
    const date = new Date(ms);
    date.setHours(0, 0, 0, 0);
    return date.getTime();
};

/** "just now", "12 min ago", "2 h ago", "yesterday", "3 days ago", "2 weeks ago", "3 Aug". */
const relativeTime = (now, ms) => {
    const seconds = Math.max(0, (now - ms) / 1000);
    if (seconds < 60) return 'just now';
    if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
    const days = Math.round((startOfDay(now) - startOfDay(ms)) / 86400000);
    if (days === 0) return `${Math.floor(seconds / 3600)} h ago`;
    if (days === 1) return 'yesterday';
    if (days < 7) return `${days} days ago`;
    if (days < 30) {
        const weeks = Math.floor(days / 7);
        return weeks === 1 ? '1 week ago' : `${weeks} weeks ago`;
    }
    return dayMonth(new Date(ms));
};

const timeOf = (value) => {
    if (typeof value === 'number') return value;
    const parsed = typeof value === 'string' ? Date.parse(value) : NaN;
    return Number.isFinite(parsed) ? parsed : null;
};

/**
 * What makes THIS copy recent: when it was added, or when the player marked
 * this very torrent watched (`watchedAt`), whichever is later. Only
 * torrent-level evidence: the library cannot tell which copy of an episode was
 * played, so its time never ranks or dates a copy (see lastActivityOf).
 * Returns { kind: 'added' | 'watched', at }.
 */
const recencyOf = ({ entry }) => {
    const added = isNumber(entry.addedAt) ? entry.addedAt : 0;
    if (entry.watched && isNumber(entry.watchedAt) && entry.watchedAt > added) return { kind: 'watched', at: entry.watchedAt };
    return { kind: 'added', at: entry.addedAt };
};

/**
 * The library's last activity on one of the row's videos: { at, label } or
 * null. "Activity", not "play": the core's last_watched also moves when a
 * notification is dismissed or an episode is marked watched (raised to its
 * release date), and nothing tells those apart from playback. So it never
 * dates a viewing line; the details show it as "Last activity". And
 * advance_to_video (finishing an episode past the credits, or marking it
 * watched) moves video_id on to the NEXT episode with time_offset = 1: that
 * placeholder is no activity on the next episode at all (an offset of 0 is a
 * title watched to its end, which is). labels maps a video id to its episode
 * label (packs).
 */
const lastActivityOf = ({ videoIds, library, labels }) => {
    if (library === undefined || library === null || typeof library.videoId !== 'string') return null;
    if (!videoIds.includes(library.videoId)) return null;
    if (isNumber(library.timeOffset) && library.timeOffset > 0 && library.timeOffset < MIN_OFFSET_MS) return null;
    const at = timeOf(library.lastWatched);
    if (at === null) return null;
    return { at, label: labels && labels[library.videoId] ? labels[library.videoId] : null };
};

/**
 * The viewing + recency line under the on-disk line. view: the row's viewing
 * state, said about the episode or movie (noun: 'Episode' | 'Movie'); the time
 * on it is always this copy's own (recencyOf: added, or watched here).
 */
const viewText = ({ view, recency, noun }, now) => {
    const rel = (at) => relativeTime(now, at);
    const subject = noun || 'Title';
    const copy = recency && isNumber(recency.at) && recency.at > 0 ?
        (recency.kind === 'watched' ? `Watched ${rel(recency.at)}` : `Added ${rel(recency.at)}`)
        :
        null;
    const withCopy = (text) => (copy !== null ? `${text} · ${copy}` : text);
    if (view === null || view === undefined) return copy;
    if (view.state === 'watched') {
        // The player marked THIS torrent watched: about this copy.
        if (view.source === 'cache') return isNumber(view.at) ? `Watched ${rel(view.at)}` : 'Watched';
        return withCopy(`${subject} watched`);
    }
    if (view.state === 'partial') return withCopy(`${subject} stopped at ${clockTime(view.offset)} of ${clockTime(view.duration)}`);
    if (view.state === 'partly') return withCopy('Partly watched');
    // "Not watched", never "not started": a false bit only means not marked
    // watched, and the library keeps an offset for one video per title.
    return copy !== null ? `${copy} · Not watched` : 'Not watched';
};
/** "28 of 54 min" for the episode table. */
const minutesOf = (view) => `${Math.round(view.offset / 60000)} of ${Math.round(view.duration / 60000)} min`;

// ---------------------------------------------------------------------------
// Copies and actions
// ---------------------------------------------------------------------------

/**
 * Notes for rows that are another copy of something higher up, by infoHash.
 * rows (display order): { infoHash, kind, metaId, season?, episode?, covers?
 * ([[season, episode]...] for a pack), label? (a pack's label) }.
 */
const copyNotes = (rows) => {
    const notes = {};
    rows.forEach((row, index) => {
        if (row.kind !== 'episode' && row.kind !== 'movie') return;
        if (typeof row.metaId !== 'string') return;
        const earlier = rows.slice(0, index);
        const same = (other) => other.metaId === row.metaId && other.kind === row.kind &&
            (row.kind === 'movie' || (other.season === row.season && other.episode === row.episode));
        const firstIndex = earlier.findIndex(same);
        if (firstIndex >= 0) {
            notes[row.infoHash] = earlier.slice(firstIndex).some((other, i) => same(other) && firstIndex + i === index - 1) ?
                'Another copy of the download above'
                :
                'Another copy is further up';
            return;
        }
        if (row.kind === 'episode') {
            const pack = earlier.find((other) => other.kind === 'pack' && other.metaId === row.metaId &&
                (other.covers || []).some(([s, e]) => s === row.season && e === row.episode));
            if (pack) notes[row.infoHash] = `Also in the ${pack.label} above`;
        }
    });
    return notes;
};

/**
 * The row's main action: { label, fileIdx, row } or null. A pack resumes the
 * episode in progress, else plays the first downloaded episode not watched
 * yet, else the first downloaded one. A single file plays (or resumes). A
 * row with an issue plays nothing: what it would play is in doubt.
 */
const primaryAction = ({ entry, kind, rows, view }) => {
    if (entry.issue !== undefined) return null;
    if (kind === 'pack') {
        // Play streams while it downloads: any episode in the selection can play.
        const here = (rows || []).filter((row) => playable(row.file));
        const partial = here.find((row) => row.view !== null && row.view.state === 'partial');
        if (partial) return { label: `Resume ${partial.label}`, fileIdx: partial.file.index, row: partial };
        const next = here.find((row) => row.view === null || row.view.state !== 'watched') ?? here[0];
        return next ? { label: `Play ${next.label}`, fileIdx: next.file.index, row: next } : null;
    }
    if (!isNumber(entry.fileIdx)) return null;
    return { label: view && view.state === 'partial' ? 'Resume' : 'Play', fileIdx: entry.fileIdx, row: null };
};

/** "about 4 min left" for the hero. */
const etaText = (seconds) => {
    if (!Number.isFinite(seconds) || seconds <= 0) return null;
    if (seconds < 60) return 'under a minute left';
    const minutes = Math.round(seconds / 60);
    if (minutes < 60) return `about ${minutes} min left`;
    const hours = Math.floor(minutes / 60);
    const rest = minutes % 60;
    return rest > 0 ? `about ${hours} h ${rest} min left` : `about ${hours} h left`;
};

// metahub poster art for an IMDb id (the CDN the catalogs already use).
const posterOf = (meta) => {
    if (meta === undefined) return null;
    if (typeof meta.poster === 'string' && meta.poster.length > 0) return meta.poster;
    return /^tt\d+$/.test(meta.metaId) ? `https://images.metahub.space/poster/small/${meta.metaId}/img` : null;
};

/**
 * Everything one row shows, from its entry and what is known around it.
 * files: /cache/files rows or null (not read yet); videos: the title's
 * episode list, null (could not be fetched) or undefined (loading); library:
 * the title's ctx.library entry or undefined; addonNames: transport url ->
 * addon name. The copy note (copyNotes) needs the display order and is
 * attached by the caller.
 */
const rowFacts = ({ entry, files, videos, library, addonNames, now }) => {
    const videoList = Array.isArray(videos) ? videos : null;
    const described = describeEntry({ entry, files, videos: videoList });
    const split = splitFiles(files);
    const isPack = described.kind === 'pack';
    const rows = isPack && Array.isArray(files) ? episodeRows({ episodes: split.episodes, videos: videoList, library }) : null;
    const view = isPack ? null : itemView({ entry, described, videos: videoList, library });
    const labels = {};
    (rows || []).forEach((row) => { if (row.videoId) labels[row.videoId] = row.label; });
    const videoIds = rowVideoIds({ entry, described, rows, videos: videoList });
    const recency = recencyOf({ entry });
    const activity = lastActivityOf({ videoIds, library, labels });
    const noun = described.kind === 'movie' ? 'Movie' : 'Episode';
    const strip = rows !== null && rows.length >= 2 ? packStrip(rows) : null;
    const summary = rows !== null ? packSummary(rows) : null;
    const meta = entry.meta;

    const totalLength = Array.isArray(files) ? files.reduce((sum, file) => sum + file.length, 0) : entry.total;
    const totalHere = Array.isArray(files) ? files.reduce((sum, file) => sum + file.downloaded, 0) : entry.downloaded;
    const source = meta !== undefined && typeof meta.streamTransportUrl === 'string' ? addonNames[meta.streamTransportUrl] : undefined;
    const details = [
        source ? `From ${source}` : null,
        isNumber(entry.addedAt) && entry.addedAt > 0 ? `Added ${exactTime(entry.addedAt)}` : null,
        // Title-level, and only "activity" (see lastActivityOf).
        activity !== null ? (activity.label ? `Last activity on ${activity.label} ${exactTime(activity.at)}` : `Last activity ${exactTime(activity.at)}`) : null,
        // This copy: the player marked this very torrent watched.
        entry.watched && isNumber(entry.watchedAt) ? `This copy watched ${exactTime(entry.watchedAt)}` : null,
        totalLength > 0 ? `${totalHere >= totalLength ? formatSize(totalHere) : formatOf(totalHere, totalLength)} on disk` : null,
    ].filter(Boolean).join(' · ');

    const viewLine = viewText({ view, recency, noun }, now);

    return {
        entry,
        described,
        kind: described.kind,
        files: Array.isArray(files) ? files : null,
        other: split.other,
        rows,
        view,
        recency,
        strip,
        summary,
        disk: diskText(entry, isPack && Array.isArray(files) ? split.episodes : null),
        viewLine,
        watchedTone: view !== null && view.state === 'watched',
        action: primaryAction({ entry, kind: described.kind, rows, view }),
        poster: posterOf(meta),
        fullName: entry.name,
        details,
        // For copyNotes.
        copyKey: {
            infoHash: entry.infoHash,
            kind: described.kind,
            metaId: meta !== undefined ? meta.metaId : null,
            season: described.season,
            episode: described.episode,
            covers: split.episodes.flatMap((row) => {
                const out = [];
                for (let e = row.episode; e <= row.lastEpisode; e += 1) out.push([isNumber(row.season) ? row.season : 1, e]);
                return out;
            }),
            label: described.packLabel,
        },
        // "Recent" ranks by this copy's own evidence: added, or watched here.
        recencyAt: isNumber(recency.at) ? recency.at : 0,
    };
};

module.exports = {
    rowFacts, etaText,
    splitFiles, describeEntry, episodeRows, itemView, rowVideoIds, packStrip, packSummary,
    diskText, viewText, minutesOf, copyNotes, primaryAction, recencyOf, lastActivityOf,
    formatSize, formatOf, relativeTime, exactTime, clockTime, basename, onDisk,
};
