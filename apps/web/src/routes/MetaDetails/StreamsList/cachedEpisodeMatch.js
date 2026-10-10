// Copyright (C) 2017-2026 Smart code 203358507

// Which cached torrents belong on the title being viewed ("On this device").
// Pure and dependency-free so the jest suite can require it directly (this
// repo's jest has no TS transform; same pattern as Player/preloadOffer);
// useCachedStreams does the fetching around it.

// The entry's own playable file, or - for a season pack, which has none - the
// file this specific episode was recorded as.
const fileIdxFor = (entry) =>
    typeof entry.fileIdx === 'number' ? entry.fileIdx :
        entry.meta && typeof entry.meta.fileIdx === 'number' ? entry.meta.fileIdx : undefined;

// entries: /cache/list rows. Returns one offer per cached torrent of this
// title: metaId for a movie, and the exact videoId for a series episode. An
// entry with no videoId (a whole-season pack we could not pin down) is shown
// on any episode.
const matchCachedEpisode = ({ entries, metaId, videoId }) => entries
    .filter((entry) => {
        if (!entry.meta || entry.meta.metaId !== metaId) return false;
        if (typeof videoId === 'string' && videoId.length > 0) {
            const entryVideo = entry.meta.videoId;
            return typeof entryVideo !== 'string' || entryVideo.length === 0 || entryVideo === videoId;
        }
        return true;
    })
    .map((entry) => ({
        infoHash: entry.infoHash,
        name: entry.name,
        fileIdx: fileIdxFor(entry),
        downloaded: entry.downloaded,
        total: entry.total,
        state: entry.state,
        meta: entry.meta,
        playHref: null,
    }));

const tiedEntries = (entries, metaId) => entries.filter((entry) => entry.meta && entry.meta.metaId === metaId);

module.exports = { matchCachedEpisode, tiedEntries };
