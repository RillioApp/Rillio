// Copyright (C) 2017-2026 Smart code 203358507

// Which cached FILE is the title being viewed ("On this device").
//
// Matched per file, not per torrent. A season pack holds many episodes, and
// its metadata sidecar only records the episode that happened to be played
// first (useCacheMetadata writes the videoId and fileIdx of that session). The
// old per-torrent rule ("meta.videoId must equal the viewed videoId") hid a
// pack holding S01E03 complete on S1E3's page because it had been recorded as
// S1E2, and a pack with no videoId showed on every episode with the WHOLE
// selection's progress and no way to play the right file.
//
// Two questions, answered from evidence only:
//
//   1. Is this torrent this title? (tiedEntries) Its sidecar says so (the
//      player's eager write, or the release-name matcher), or an addon on this
//      very page lists a stream with the same infoHash - an addon offering the
//      torrent for this episode is proof of what it is. File names alone never
//      tie a torrent to a title: a wrong title is worse than none.
//   2. Which of its files is the viewed episode? (series only) In order of
//      authority: the addon stream's own fileIdx (the addon resolved THIS
//      episode to that file), the sidecar's recorded file when it was recorded
//      for this very episode, then the file names (common/releaseParse).
//
// The offer carries THAT file's progress, and only exists when some of it is
// on disk or downloading: a file of the pack that was never selected is not
// "on this device", and the addon list below already offers to stream it.
//
// Movies are one per torrent as before, but pick their file by the same
// authority (a collection pack holds several movies) and show that file's
// bytes when its listing is available. Pure and dependency-free so the jest suite can require it directly
// (this repo's jest has no TS transform; same pattern as Player/preloadOffer);
// useCachedStreams does the fetching around it.

const { parseEpisode } = require('../../../common/releaseParse');

const lower = (value) => (typeof value === 'string' ? value.toLowerCase() : null);

// The addon streams on this page that point at this torrent.
const streamsFor = (addonStreams, infoHash) =>
    addonStreams.filter((stream) => stream && lower(stream.infoHash) === lower(infoHash));

const metaIsTitle = (entry, metaId) => Boolean(entry.meta) && entry.meta.metaId === metaId;

/** The cached torrents that are provably this title (see question 1). */
const tiedEntries = (entries, metaId, addonStreams) => entries.filter((entry) =>
    metaIsTitle(entry, metaId) || streamsFor(addonStreams, entry.infoHash).length > 0);

// Whether a parsed file is (or, for a combined file, covers) the viewed
// episode. Absolute numbering (no season in the path) equals per-season
// numbering only in a show's first season, or on an addon that numbers
// without seasons.
const isViewedEpisode = (parsed, season, episode) => {
    if (parsed === null || typeof episode !== 'number') return false;
    const last = typeof parsed.lastEpisode === 'number' ? parsed.lastEpisode : parsed.episode;
    if (episode < parsed.episode || episode > last) return false;
    const viewedSeason = typeof season === 'number' ? season : null;
    if (parsed.season === null) return viewedSeason === null || viewedSeason === 1;
    return viewedSeason === null ? parsed.season === 1 : parsed.season === viewedSeason;
};

const basename = (path) => path.split(/[\\/]/).pop();

// The deep-link href of an addon stream for exactly this file, if any: the
// link the core built for that stream, with its own transport urls.
const hrefFor = (streams, fileIdx) => {
    const stream = streams.find((candidate) => candidate.fileIdx === fileIdx &&
        candidate.deepLinks && typeof candidate.deepLinks.player === 'string');
    return stream ? stream.deepLinks.player : null;
};

// A movie's file, by the same authority as an episode's: an addon stream on
// this page (it resolved THIS movie), then the sidecar only when it names this
// title (a collection pack cached while playing another movie records THAT
// movie's file), then the server's playable file for the selection.
// The deep-link meta for an offer whose torrent has no sidecar of this title:
// built from the page itself (this title, this episode, the addon that served
// the page's meta, and the addon that listed the torrent), the same shape the
// player's eager write records. Without the page's meta addon there is no full
// link to build (the bare /player/:stream remains, as before).
const pageMeta = (view, streams, fileIdx) => {
    const { page, metaId, videoId, season, episode } = view;
    if (!page || typeof page.metaTransportUrl !== 'string' || page.metaTransportUrl.length === 0) return undefined;
    // The addon that supplied THIS file: one listing the torrent with exactly
    // this fileIdx, else one listing it with no index (its stream is "the
    // torrent"), never one listing it for a different file of the pack.
    const listers = streams.filter((stream) => typeof stream.addonTransportUrl === 'string');
    const lister = listers.find((stream) => typeof fileIdx === 'number' && stream.fileIdx === fileIdx) ??
        listers.find((stream) => typeof stream.fileIdx !== 'number');
    const episodeView = typeof videoId === 'string' && videoId.length > 0 && videoId !== metaId;
    return {
        metaId,
        type: page.type,
        name: typeof page.name === 'string' ? page.name : '',
        poster: null,
        background: null,
        logo: null,
        year: null,
        videoId: episodeView ? videoId : null,
        season: episodeView && typeof season === 'number' ? season : null,
        episode: episodeView && typeof episode === 'number' ? episode : null,
        metaTransportUrl: page.metaTransportUrl,
        streamTransportUrl: lister ? lister.addonTransportUrl : null,
        fileIdx: typeof fileIdx === 'number' ? fileIdx : null,
    };
};

const movieOffer = (entry, files, view) => {
    const { metaId, videoId, addonStreams } = view;
    const streams = streamsFor(addonStreams, entry.infoHash);
    const ownMeta = metaIsTitle(entry, metaId) &&
        (entry.meta.videoId === null || entry.meta.videoId === undefined || entry.meta.videoId === videoId) ?
        entry.meta : undefined;
    const fromStream = streams.find((stream) => typeof stream.fileIdx === 'number');
    const fileIdx = fromStream ? fromStream.fileIdx :
        ownMeta !== undefined && typeof ownMeta.fileIdx === 'number' ? ownMeta.fileIdx :
            typeof entry.fileIdx === 'number' ? entry.fileIdx : undefined;
    // That file's own bytes when the listing has it; the selection's
    // otherwise (a single-movie torrent, where they are the same thing).
    const file = Array.isArray(files) && fileIdx !== undefined ? files.find((candidate) => candidate.index === fileIdx) : undefined;
    return {
        infoHash: entry.infoHash,
        name: file ? basename(file.path) : entry.name,
        fileIdx,
        downloaded: file ? file.downloaded : entry.downloaded,
        total: file ? file.length : entry.total,
        state: entry.state,
        meta: ownMeta ?? pageMeta(view, streams, fileIdx),
        playHref: fileIdx === undefined ? null : hrefFor(streams, fileIdx),
    };
};

const episodeOffer = (entry, files, view) => {
    const { metaId, videoId, season, episode, addonStreams } = view;
    const streams = streamsFor(addonStreams, entry.infoHash);
    const fileAt = (index) => (typeof index === 'number' ? files.find((file) => file.index === index) : undefined);
    const ownMeta = metaIsTitle(entry, metaId) ? entry.meta : undefined;
    const recordedHere = ownMeta !== undefined && ownMeta.videoId === videoId;

    const byStream = streams.map((stream) => fileAt(stream.fileIdx)).find(Boolean);
    const byRecord = recordedHere ? fileAt(ownMeta.fileIdx) ?? fileAt(entry.fileIdx) : undefined;
    const byName = files
        .filter((file) => file.video && isViewedEpisode(parseEpisode(file.path), season, episode))
        // A sample clip can carry the same SxxEyy as the episode: the
        // episode is the big one.
        .sort((a, b) => b.length - a.length)[0];
    const file = byStream ?? byRecord ?? byName;
    if (file === undefined) return null;
    if (!file.selected && !(file.downloaded > 0)) return null;

    return {
        infoHash: entry.infoHash,
        name: basename(file.path),
        fileIdx: file.index,
        downloaded: file.downloaded,
        total: file.length,
        state: entry.state,
        // Re-aimed at the viewed episode, so the player deep link opens this
        // episode (and its library progress), not the one first recorded.
        meta: ownMeta === undefined ? pageMeta(view, streams, file.index) : {
            ...ownMeta,
            videoId,
            season: typeof season === 'number' ? season : null,
            episode: typeof episode === 'number' ? episode : null,
            fileIdx: file.index,
        },
        playHref: hrefFor(streams, file.index),
    };
};

/**
 * The "On this device" offers for the title on screen.
 *
 * entries: /cache/list rows. filesByHash: { [infoHash]: /cache/files rows }
 * for the tied torrents (on a series page a torrent missing here is not
 * offered; a movie falls back to the torrent's selection numbers).
 * metaId/videoId: what is on screen (videoId === metaId for a movie).
 * season/episode: the viewed episode's numbers, when known. addonStreams: the
 * addon streams on this page (for this episode), each tagged with the
 * `addonTransportUrl` of the addon that listed it. page: { type, name,
 * metaTransportUrl } of the title on screen, for torrents with no sidecar of
 * this title.
 *
 * Returns { infoHash, name, fileIdx, downloaded, total, state, meta, playHref }
 * per offer: fileIdx is the file to play, downloaded/total are that file's
 * (the torrent's selection for a movie), meta builds the player deep link,
 * and playHref, when set, is an addon's own deep link for exactly that file.
 */
const matchCachedEpisode = ({ entries, filesByHash, metaId, videoId, season, episode, addonStreams, page }) => {
    const streams = Array.isArray(addonStreams) ? addonStreams : [];
    const tied = tiedEntries(entries, metaId, streams);
    const episodeView = typeof videoId === 'string' && videoId.length > 0 && videoId !== metaId;
    const view = { metaId, videoId, season, episode, addonStreams: streams, page };
    if (!episodeView) {
        return tied.map((entry) => movieOffer(entry, filesByHash[entry.infoHash], view));
    }
    return tied
        .map((entry) => {
            const files = filesByHash[entry.infoHash];
            return Array.isArray(files) ? episodeOffer(entry, files, view) : null;
        })
        .filter((offer) => offer !== null);
};

module.exports = { matchCachedEpisode, tiedEntries, isViewedEpisode };
