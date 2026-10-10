// Copyright (C) 2017-2026 Smart code 203358507

// The route an "On this device" card opens, from its offer (see
// cachedEpisodeMatch.js). Plain CommonJS so jest can build real links end to
// end (tests/cachedOfferLink.spec.js); CachedStreams passes the core's
// encoder (core.transport.encodeStream) and navigates to the result.

const { playerDeepLink } = require('../../../common/playerDeepLink');

const toPath = (link) => (link.startsWith('#') ? link.slice(1) : link);

/**
 * Resolves to the player route for `offer`:
 *   - an addon listed exactly this file for this page: its own deep link,
 *     the one a click on that addon stream would follow;
 *   - otherwise a stream for the offer's RESOLVED file (fileIdx), encoded by
 *     `encodeStream`, on the full route built from the offer's meta (this
 *     title and episode), or the bare player route when there is none.
 * Rejects when the encoder yields no string.
 */
const cachedOfferLink = (offer, encodeStream) => {
    if (typeof offer.playHref === 'string') {
        return Promise.resolve(toPath(offer.playHref));
    }
    return Promise.resolve(encodeStream({
        name: offer.name,
        description: '',
        infoHash: offer.infoHash,
        fileIdx: offer.fileIdx,
    })).then((encoded) => {
        if (typeof encoded !== 'string') {
            throw new Error(`the core could not encode a stream for ${offer.infoHash}`);
        }
        return playerDeepLink(encoded, offer.meta);
    });
};

module.exports = { cachedOfferLink };
