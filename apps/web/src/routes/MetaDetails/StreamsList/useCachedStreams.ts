// Copyright (C) 2017-2026 Smart code 203358507

// "What of this title do I already have on disk?"
//
// The streams panel lists what ADDONS can offer, which is a strange thing to
// stare at when the file is already sitting in the cache, fully downloaded.
// This reads the local cache and returns the cached FILES that are the title
// being viewed. The matching itself (which torrents are this title, which file
// of a season pack is this episode) is pure and jest-covered in
// cachedEpisodeMatch.js; this hook only fetches for it: the cache list, plus
// each tied torrent's file listing on a series page.
//
// Cheap and quiet: one fetch per title (plus a refresh while a download is
// still running), and a missing/unreachable server just yields nothing.

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useProfile } from 'rillio/common';
import type { CacheMeta } from 'rillio/common/cacheMetadata';
import { serverFetch } from 'rillio/common/serverFetch';
import type { TorrentFile } from 'rillio/routes/Cached/TorrentFiles';

// One cached file offered on this page (see cachedEpisodeMatch.js).
export type CachedStream = {
    infoHash: string,
    name: string,
    // The file to play. Undefined when the torrent has no single playable
    // file we could identify (a movie pack with several features).
    fileIdx?: number,
    // That file's bytes (the torrent's selection for a movie).
    downloaded: number,
    total: number,
    state: string,
    // Aimed at the viewed episode; builds the full player deep link.
    meta?: CacheMeta,
    // An addon's own deep link for exactly this file, when one is listed.
    playHref: string | null,
};

type CacheListEntry = {
    infoHash: string,
    meta?: CacheMeta,
};

export type AddonStreamLike = {
    infoHash?: unknown,
    fileIdx?: unknown,
    deepLinks?: { player?: unknown },
    // The addon that listed this stream (its stream transport url).
    addonTransportUrl?: unknown,
};

// The title on screen, for deep links to torrents with no sidecar of it.
export type CachedStreamsPage = {
    type: string | null,
    name: string | null,
    metaTransportUrl: string | null,
};

const { matchCachedEpisode, tiedEntries } = require('./cachedEpisodeMatch') as {
    matchCachedEpisode: (args: {
        entries: CacheListEntry[],
        filesByHash: Record<string, TorrentFile[]>,
        metaId: string,
        videoId: string | null,
        season: number | null,
        episode: number | null,
        addonStreams: AddonStreamLike[],
        page: CachedStreamsPage,
    }) => CachedStream[],
    tiedEntries: (entries: CacheListEntry[], metaId: string, addonStreams: AddonStreamLike[]) => CacheListEntry[],
};

const { pollSequential } = require('./pollSequential') as {
    pollSequential: (load: (isCurrent: () => boolean) => Promise<void>, intervalMs: number) => () => void,
};

const POLL_MS = 3000;

type Args = {
    metaId: string | null,
    videoId: string | null,
    season: number | null,
    episode: number | null,
    // The addon streams listed on this page: an addon offering a cached
    // torrent for this episode ties that torrent to the title and names its
    // file.
    addonStreams: AddonStreamLike[],
    page: CachedStreamsPage,
};

const useCachedStreams = ({ metaId, videoId, season, episode, addonStreams, page }: Args): CachedStream[] => {
    const { type: pageType, name: pageName, metaTransportUrl } = page;
    const profile = useProfile();
    const serverUrl = profile.settings.streamingServerUrl;
    const [matches, setMatches] = useState<CachedStream[]>([]);

    // The core hands out a new streams array on every state update; restarting
    // the poll for each would hammer the server. Only the torrent identities
    // matter, so the poll restarts when THOSE change and reads the latest
    // array from a ref.
    const streamsRef = useRef(addonStreams);
    streamsRef.current = addonStreams;
    const evidenceKey = useMemo(() => addonStreams
        .filter((stream) => typeof stream.infoHash === 'string')
        .map((stream) => `${String(stream.infoHash).toLowerCase()}:${String(stream.fileIdx)}`)
        .sort()
        .join(','), [addonStreams]);

    // isCurrent() turns false once this poll is stopped (unmount, or another
    // title/episode): a load still in flight then drops its result.
    const load = useCallback((isCurrent: () => boolean): Promise<void> => {
        if (typeof serverUrl !== 'string' || typeof metaId !== 'string' || metaId.length === 0) {
            setMatches([]);
            return Promise.resolve();
        }
        const fetchJson = <T>(path: string): Promise<T> => serverFetch(new URL(path, serverUrl))
            .then((resp) => {
                if (!resp.ok) throw new Error(`${path} responded ${resp.status}`);
                return resp.json() as Promise<T>;
            });
        return fetchJson<CacheListEntry[]>('cache/list')
            .then(async (entries) => {
                // Stopped while the list was loading: do not fan out a file
                // listing per torrent for a page that is gone.
                if (!isCurrent()) return;
                const addonStreamsNow = streamsRef.current;
                const filesByHash: Record<string, TorrentFile[]> = {};
                // Which file of each tied torrent is this episode (or this
                // movie, in a collection) is a per-file question, and the
                // row shows that file's bytes. A listing that fails is
                // logged; the matcher then leaves an episode out rather than
                // guess.
                await Promise.all(tiedEntries(entries, metaId, addonStreamsNow).map((entry) =>
                    fetchJson<TorrentFile[]>(`cache/files/${entry.infoHash}`)
                        .then((files) => { filesByHash[entry.infoHash] = files; })
                        .catch((error) => console.error('useCachedStreams: listing the files of', entry.infoHash, 'failed', error))));
                // A load of the previous title/episode, or one finishing after
                // unmount, must not overwrite anything.
                if (!isCurrent()) return;
                setMatches(matchCachedEpisode({
                    entries,
                    filesByHash,
                    metaId,
                    videoId,
                    season,
                    episode,
                    addonStreams: addonStreamsNow,
                    page: { type: pageType, name: pageName, metaTransportUrl },
                }));
            })
            // No streaming server, or it is down: there is simply nothing cached
            // to show. Never surfaced to the user - the addon streams below are
            // unaffected.
            .catch(() => {
                if (isCurrent()) setMatches([]);
            });
        // evidenceKey stands in for addonStreams (read through the ref).
    }, [serverUrl, metaId, videoId, season, episode, evidenceKey, pageType, pageName, metaTransportUrl]);

    // Another title or episode: the previous one's cards go at once, rather
    // than sitting on this page until the first (possibly slow) load lands.
    // Keyed on the identity only: addon streams arriving must not blink them.
    useEffect(() => {
        setMatches((current) => (current.length === 0 ? current : []));
    }, [metaId, videoId]);

    // Sequential: the next load starts POLL_MS after the previous one
    // finished, so a slow server's results still land (see pollSequential).
    useEffect(() => pollSequential(load, POLL_MS), [load]);

    return matches;
};

export default useCachedStreams;
