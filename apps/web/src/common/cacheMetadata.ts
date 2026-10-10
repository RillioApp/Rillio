// Copyright (C) 2017-2026 Smart code 203358507

// What media a cached torrent actually is.
//
// A torrent only knows its release name ("Some.Movie.2026.1080p.HEVC-GRP"), so
// the Cache page used to show scene filenames and could only recover a real
// title for the handful of items still in continue-watching. This module fills
// the gap from both ends:
//
//   1. EAGER (the reliable path): when playback starts from a real title in the
//      app, we already hold the full metadata - save it against the infoHash
//      right there.
//   2. MATCHED (the fallback): for a torrent that arrived without context (a
//      pasted magnet, a leftover from an old install), parse the release name
//      and search the INSTALLED ADDONS for it, exactly as the app's own search
//      does, then save the best match.
//
// Storage is the streaming server's sidecar (POST /cache/meta, returned on
// /cache/list), not localStorage: the cache is a server-side concept, and the
// mapping should survive a profile switch and a reinstall of the web bundle.
//
// The addon search is issued DIRECTLY over the addon protocol rather than
// through the core's CatalogsWithExtra model, because that model is a singleton
// shared with the Search page - driving it from a background matcher would
// stomp on whatever the user is searching for.

import { serverFetch } from 'rillio/common/serverFetch';

export type CacheMeta = {
    metaId: string,
    type: string,
    name: string,
    poster: string | null,
    background: string | null,
    logo: string | null,
    year: string | null,
    // Series only: the specific episode this torrent holds, so the Cache page
    // can label it and deep link to the right video.
    videoId?: string | null,
    season?: number | null,
    episode?: number | null,
    // The addons this title resolves through. Kept so playing from the cache can
    // build the FULL player deep link (`/player/:stream/:streamTransportUrl?
    // /:metaTransportUrl?/:type?/:id?/:videoId?`) and therefore load exactly the
    // way a catalog play does: real metadata, library progress, next episode.
    // Without them the player only ever gets a bare stream.
    metaTransportUrl?: string | null,
    streamTransportUrl?: string | null,
    // Which file inside the torrent this title/episode is. A season pack has no
    // single playable file (`fileIdx` on the cache entry is absent by design),
    // but on ONE episode's page we know exactly which one is wanted - this is
    // how that page can still offer Play.
    fileIdx?: number | null,
};

// The full player deep link for an identified cached torrent (a bare
// `/player/:stream` when only the stream is known). Lives in a CommonJS
// sibling so the jest suite can assert the links built from it.
const { playerDeepLink } = require('./playerDeepLink') as {
    playerDeepLink: (encodedStream: string, meta: CacheMeta | undefined) => string,
};
export { playerDeepLink };

export const saveCacheMeta = (serverUrl: string, infoHash: string, meta: CacheMeta | null): Promise<void> =>
    serverFetch(new URL('cache/meta', serverUrl), {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ infoHash, meta }),
    })
        .then((resp) => {
            if (!resp.ok) throw new Error(`cache/meta responded ${resp.status}`);
        });

// ---------------------------------------------------------------------------
// Release-name parsing
// ---------------------------------------------------------------------------

export type ParsedRelease = {
    title: string,
    year: string | null,
    season: number | null,
    episode: number | null,
};

// The one scene-name parser lives in a CommonJS sibling so the jest suite can
// require it directly (tests/releaseParse.spec.js); the streams list uses the
// same module to read season/episode from each file of a season pack.
const { parseReleaseName } = require('./releaseParse') as {
    parseReleaseName: (raw: string) => ParsedRelease,
};
export { parseReleaseName };

// ---------------------------------------------------------------------------
// Addon search
// ---------------------------------------------------------------------------

export type AddonLike = {
    transportUrl?: unknown,
    manifest?: {
        resources?: unknown[],
        catalogs?: { type?: string, id?: string, extra?: { name?: string }[], extraSupported?: string[] }[],
    },
};

type SearchableCatalog = { base: string, type: string, id: string, transportUrl: string };

/** Installed catalogs that accept a `search` extra, for the types we care about. */
const searchableCatalogs = (addons: AddonLike[], type: string): SearchableCatalog[] =>
    addons.flatMap((addon) => {
        const transportUrl = addon.transportUrl;
        if (typeof transportUrl !== 'string') return [];
        const base = transportUrl.replace(/\/manifest\.json$/, '');
        return (addon.manifest?.catalogs ?? [])
            .filter((catalog) => catalog.type === type && typeof catalog.id === 'string')
            .filter((catalog) => {
                const supported = catalog.extraSupported ?? (catalog.extra ?? []).map((extra) => extra?.name);
                return Array.isArray(supported) && supported.includes('search');
            })
            .map((catalog) => ({ base, type, id: catalog.id as string, transportUrl }));
    });

type MetaPreviewLike = {
    id?: unknown,
    type?: unknown,
    name?: unknown,
    poster?: unknown,
    background?: unknown,
    logo?: unknown,
    releaseInfo?: unknown,
};

const str = (value: unknown): string | null => (typeof value === 'string' && value.length > 0 ? value : null);

// Comparable form of a title: case, punctuation and articles are all things
// scene names and catalogs disagree about.
const normalizeTitle = (title: string): string =>
    title.toLowerCase().replace(/&/g, 'and').replace(/[^a-z0-9]+/g, ' ').replace(/\s+/g, ' ').trim();

const REQUEST_TIMEOUT_MS = 8000;

/** A search hit, tagged with the addon that returned it (see CacheMeta's
 *  transport urls: that addon is also where the title's meta comes from). */
type SearchHit = { meta: MetaPreviewLike, transportUrl: string };

const fetchCatalogSearch = (catalog: SearchableCatalog, query: string): Promise<SearchHit[]> => {
    const url = `${catalog.base}/catalog/${encodeURIComponent(catalog.type)}/${encodeURIComponent(catalog.id)}/search=${encodeURIComponent(query)}.json`;
    // A dead or slow addon must not hang the matcher: every addon in the
    // profile is contacted, and one of them being down is routine.
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), REQUEST_TIMEOUT_MS);
    return fetch(url, { signal: abort.signal })
        .then((resp) => (resp.ok ? resp.json() : Promise.reject(new Error(String(resp.status)))))
        .then((body: { metas?: MetaPreviewLike[] }) => (body?.metas ?? []).map((meta) => ({ meta, transportUrl: catalog.transportUrl })))
        .catch((): SearchHit[] => [])
        .finally(() => clearTimeout(timer));
};

/**
 * Find the addon metadata for a parsed release name. Returns null when nothing
 * matches well enough - a WRONG title on a cached item is worse than a scene
 * filename, so the bar is an exact normalised title match, with the year used
 * to break ties (and to reject a same-name remake when we know the year).
 */
export const matchRelease = async (addons: AddonLike[], parsed: ParsedRelease): Promise<CacheMeta | null> => {
    if (parsed.title.length < 2) return null;
    // A season/episode tag means series; otherwise try movie first, then series
    // (a series pack often carries no S/E in the folder name).
    const types = parsed.season !== null ? ['series', 'movie'] : ['movie', 'series'];
    const wanted = normalizeTitle(parsed.title);

    for (const type of types) {
        const catalogs = searchableCatalogs(addons, type);
        if (catalogs.length === 0) continue;
        const results = (await Promise.all(catalogs.map((catalog) => fetchCatalogSearch(catalog, parsed.title)))).flat();
        const candidates = results.filter((r) => str(r.meta.id) !== null && normalizeTitle(String(r.meta.name ?? '')) === wanted);
        if (candidates.length === 0) continue;
        // Prefer the candidate whose year matches the release's, when both are known.
        const hit = (parsed.year !== null ?
            candidates.find((c) => str(c.meta.releaseInfo)?.startsWith(parsed.year as string))
            :
            undefined) ?? candidates[0];
        const best = hit.meta;
        return {
            metaId: String(best.id),
            type: str(best.type) ?? type,
            name: String(best.name ?? parsed.title),
            poster: str(best.poster),
            background: str(best.background),
            logo: str(best.logo),
            year: str(best.releaseInfo),
            season: parsed.season,
            episode: parsed.episode,
            videoId: parsed.season !== null && parsed.episode !== null ?
                `${String(best.id)}:${parsed.season}:${parsed.episode}`
                :
                null,
            // The addon that knew this title also serves its meta, so playing
            // from the cache can load it natively.
            metaTransportUrl: hit.transportUrl,
            streamTransportUrl: null,
        };
    }
    return null;
};
