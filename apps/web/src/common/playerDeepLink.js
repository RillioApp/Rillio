// Copyright (C) 2017-2026 Smart code 203358507

// The full player deep link for an identified cached torrent. Plain CommonJS
// so the jest suite can require it (tests/cachedEpisodeMatch.spec.js asserts
// the links the streams list builds); cacheMetadata.ts re-exports it typed.

/**
 * `/player/:stream/:streamTransportUrl?/:metaTransportUrl?/:type?/:id?/:videoId?`
 * for `meta` (a CacheMeta), or `/player/:stream` when we only know the stream.
 * `encodedStream` comes from core.transport.encodeStream.
 *
 * Mirrors the route's positional shape, so a missing streamTransportUrl still
 * has to occupy its slot - the meta addon stands in, which is what the core
 * itself does for a stream that came from a title's own addon.
 */
const playerDeepLink = (encodedStream, meta) => {
    const stream = encodeURIComponent(encodedStream);
    const metaUrl = meta ? meta.metaTransportUrl : undefined;
    if (!meta || typeof metaUrl !== 'string' || metaUrl.length === 0) {
        return `/player/${stream}`;
    }
    const streamUrl = meta.streamTransportUrl ?? metaUrl;
    const videoId = meta.videoId ?? meta.metaId;
    return `/player/${stream}/${encodeURIComponent(streamUrl)}/${encodeURIComponent(metaUrl)}` +
        `/${encodeURIComponent(meta.type)}/${encodeURIComponent(meta.metaId)}/${encodeURIComponent(videoId)}`;
};

module.exports = { playerDeepLink };
