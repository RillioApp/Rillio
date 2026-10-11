// The link a click on an "On this device" card follows, built end to end from
// a real offer: the stream object handed to the core's encoder must carry the
// RESOLVED file (the episode's, not the torrent's first or largest), and the
// route must carry the offer's meta (this title and episode), so the player
// opens that file as a catalog play would.

const { matchCachedEpisode } = require('../src/routes/MetaDetails/StreamsList/cachedEpisodeMatch');
const { cachedOfferLink } = require('../src/routes/MetaDetails/StreamsList/cachedOfferLink');

const SHOW = 'tt7772588';
const PACK = 'a'.repeat(40);
const GB = 1024 ** 3;
const CINEMETA = 'https://v3-cinemeta.strem.io/manifest.json';
const TORRENTIO = 'https://torrentio.example/manifest.json';
const DIR = 'For.All.Mankind.S01.2160p.ATVP.WEB-DL.DDP5.1.Atmos.DoVi.HEVC-DVT';
const files = [1, 2, 3, 4].map((n, index) => ({
    index,
    path: `${DIR}/Mankind.S01E0${n}.2160p.mp4`,
    length: 8 * GB,
    downloaded: n === 1 ? 0 : 8 * GB,
    selected: n !== 1,
    video: true,
}));
const recordedAsE2 = {
    metaId: SHOW, type: 'series', name: 'For All Mankind', poster: null, background: null, logo: null, year: null,
    videoId: `${SHOW}:1:2`, season: 1, episode: 2, fileIdx: 1,
    metaTransportUrl: CINEMETA, streamTransportUrl: TORRENTIO,
};
const entry = (meta) => ({ infoHash: PACK, name: DIR, downloaded: 24 * GB, total: 24 * GB, state: 'live', fileCount: 3, meta });

// Stands in for core.transport.encodeStream: records what it was given and
// encodes it reversibly, so the test reads the stream back out of the link.
const fakeEncoder = () => {
    const seen = [];
    const encode = (stream) => {
        seen.push(stream);
        return Promise.resolve(Buffer.from(JSON.stringify(stream)).toString('base64'));
    };
    return { encode, seen };
};
const segments = (link) => link.split('/').map(decodeURIComponent);
const decodeStream = (link) => JSON.parse(Buffer.from(segments(link)[2], 'base64').toString());

const offerFor = (meta, addonStreams = [], page = undefined) => matchCachedEpisode({
    entries: [entry(meta)],
    filesByHash: { [PACK]: files },
    metaId: SHOW,
    videoId: `${SHOW}:1:3`,
    season: 1,
    episode: 3,
    addonStreams,
    page,
})[0];

test('the reported pack (recorded as E02) on E03: the encoded stream is file 2, the route is E03', async () => {
    const offer = offerFor(recordedAsE2);
    const { encode, seen } = fakeEncoder();
    const link = await cachedOfferLink(offer, encode);
    expect(seen).toEqual([{ name: 'Mankind.S01E03.2160p.mp4', description: '', infoHash: PACK, fileIdx: 2 }]);
    expect(decodeStream(link).fileIdx).toBe(2);
    expect(segments(link)).toEqual(['', 'player', expect.any(String), TORRENTIO, CINEMETA, 'series', SHOW, `${SHOW}:1:3`]);
});

test('no sidecar, addon lists the pack without an index: file found by name, route built from the page', async () => {
    const offer = offerFor(undefined, [{ infoHash: PACK, addonTransportUrl: TORRENTIO }], { type: 'series', name: 'For All Mankind', metaTransportUrl: CINEMETA });
    const { encode, seen } = fakeEncoder();
    const link = await cachedOfferLink(offer, encode);
    expect(seen[0].fileIdx).toBe(2);
    expect(segments(link).slice(3)).toEqual([TORRENTIO, CINEMETA, 'series', SHOW, `${SHOW}:1:3`]);
});

test('an addon link for exactly this file is followed as is, nothing encoded', async () => {
    const href = '#/player/addon-encoded/torrentio/cinemeta/series/tt7772588/tt7772588%3A1%3A3';
    const offer = offerFor(recordedAsE2, [{ infoHash: PACK, fileIdx: 2, deepLinks: { player: href } }]);
    const { encode, seen } = fakeEncoder();
    expect(await cachedOfferLink(offer, encode)).toBe(href.slice(1));
    expect(seen).toEqual([]);
});

test('an encoder that yields no string fails loudly', async () => {
    const offer = offerFor(recordedAsE2);
    await expect(cachedOfferLink(offer, () => Promise.resolve(null))).rejects.toThrow(/could not encode/);
});
