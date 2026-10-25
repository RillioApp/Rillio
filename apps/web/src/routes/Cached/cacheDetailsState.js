// Copyright (C) 2017-2026 Smart code 203358507

// The bookkeeping behind the Cache window's data hooks (useCacheDetails),
// pure so the jest suite can require it directly:
//
//   - file lists: per streaming server and infohash, with a request
//     generation so an older response can never overwrite a newer one, and
//     pruning so a removed (or re-added) hash starts over;
//   - episode lists: keyed by the addon, type and id they came from (two
//     addons can list a title's episodes differently), with failures retried
//     once RETRY_MS has passed;
//   - which card is open, dropped when its download leaves the list.

const RETRY_MS = 5 * 60 * 1000;

// ---------------------------------------------------------------------------
// File lists
// ---------------------------------------------------------------------------

/** An empty book for `server`. */
const emptyBook = (server) => ({ server, generations: {}, lists: {}, signatures: {} });

/** The book for `server`: the same one, or a fresh one if the server changed. */
const bookFor = (book, server) => (book !== null && book.server === server ? book : emptyBook(server));

/**
 * Starts a request for `infoHash`: returns { book, generation } where only a
 * response carrying this generation may apply.
 */
const beginRequest = (book, infoHash) => {
    const generation = (book.generations[infoHash] || 0) + 1;
    return { book: { ...book, generations: { ...book.generations, [infoHash]: generation } }, generation };
};

/** Applies a response if it is the latest request for its hash on this server. */
const applyResponse = (book, server, infoHash, generation, list) => {
    if (book.server !== server || book.generations[infoHash] !== generation) return book;
    return { ...book, lists: { ...book.lists, [infoHash]: list } };
};

/** Forgets everything about hashes no longer in the list (a re-add refetches). */
const prune = (book, hashes) => {
    const keep = (map) => Object.fromEntries(Object.entries(map).filter(([hash]) => hashes.includes(hash)));
    return { ...book, generations: keep(book.generations), lists: keep(book.lists), signatures: keep(book.signatures) };
};

/** What makes a torrent's file list worth re-reading: its numbers moved. */
const fileSignature = (entry) => `${entry.downloaded}|${entry.total}|${entry.fileCount}|${entry.state}`;

// ---------------------------------------------------------------------------
// Episode lists
// ---------------------------------------------------------------------------

/** The key an episode list is fetched and looked up by, or null. */
const videoKey = (meta) => (meta && meta.type !== 'movie' && typeof meta.metaTransportUrl === 'string' && meta.metaTransportUrl.length > 0 ?
    `${meta.metaTransportUrl}|${meta.type}|${meta.metaId}`
    :
    null);

/** Only the lists still wanted (a title whose addon changed drops the old one). */
const keepWanted = (lists, keys) => Object.fromEntries(Object.entries(lists).filter(([key]) => keys.includes(key)));

/**
 * How long until a cached fetch should be retried: null for a success (never),
 * 0 or more milliseconds for a failure ({ videos: null, at }).
 */
const retryInMs = (cached, now) => (cached.videos !== null ? null : Math.max(0, cached.at + RETRY_MS - now));

// ---------------------------------------------------------------------------
// The open card
// ---------------------------------------------------------------------------

/** The open card's hash while its download is still listed, else null. */
const stillOpen = (openCard, entries) => (openCard !== null && Array.isArray(entries) && entries.some((entry) => entry.infoHash === openCard) ? openCard : null);

module.exports = {
    RETRY_MS, emptyBook, bookFor, beginRequest, applyResponse, prune, fileSignature,
    videoKey, keepWanted, retryInMs, stillOpen,
};
