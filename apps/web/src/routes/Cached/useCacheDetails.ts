// Copyright (C) 2017-2026 Smart code 203358507

// The data the Cache window's rows need beyond /cache/list, each fetched once
// and kept current:
//
//   - useLibraryMap: the library's viewing state per title (ctx.library:
//     watched bitfield, current video, time offset, last watched), from the
//     core's one shared `ctx` model.
//   - useTitleVideos: each identified series' episode list, from the addon the
//     sidecar names (metaTransportUrl), cached for the session. A failure is
//     "unknown" (null), never an empty list: the rows then say nothing about
//     viewing rather than "not started".
//   - useFileLists: every running torrent's file list (/cache/files), re-read
//     when the torrent's numbers move, so packs can be told apart from single
//     episodes and every file can be managed from the row.

import { useCallback, useEffect, useRef, useState } from 'react';
import { useCore } from 'rillio/core';
import { serverFetch } from 'rillio/common/serverFetch';
import type { CacheEntry } from './useCachedTorrents';
import type { TorrentFile } from './TorrentFiles';

const details = require('./cacheDetailsState') as {
    bookFor: (book: FileBook | null, server: string) => FileBook,
    beginRequest: (book: FileBook, infoHash: string) => { book: FileBook, generation: number },
    applyResponse: (book: FileBook, server: string, infoHash: string, generation: number, list: TorrentFile[]) => FileBook,
    prune: (book: FileBook, hashes: string[]) => FileBook,
    fileSignature: (entry: CacheEntry) => string,
    videoKey: (meta: CacheEntry['meta']) => string | null,
    keepWanted: <T>(lists: Record<string, T>, keys: string[]) => Record<string, T>,
    retryInMs: (cached: { videos: unknown, at: number }, now: number) => number | null,
};

type FileBook = {
    server: string,
    generations: Record<string, number>,
    lists: Record<string, TorrentFile[]>,
    signatures: Record<string, string>,
};

export type LibraryEntry = {
    removed: boolean,
    watched: boolean,
    temp: boolean,
    lastWatched: string | null,
    videoId: string | null,
    timeOffset: number,
    duration: number,
    watchedField: string | null,
};

export type MetaVideo = {
    id: string,
    title?: string,
    name?: string,
    season?: number,
    episode?: number,
    released?: string,
};

/** metaId -> the library's viewing state, or {} until the ctx is read. */
export const useLibraryMap = (): Record<string, LibraryEntry> => {
    const core = useCore();
    const [library, setLibrary] = useState<Record<string, LibraryEntry>>({});
    useEffect(() => {
        let cancelled = false;
        const read = (models?: unknown) => {
            if (Array.isArray(models) && !models.includes('ctx')) return;
            core.transport.getState('ctx')
                .then((ctx) => {
                    const map = (ctx as { library?: Record<string, LibraryEntry> } | null)?.library;
                    if (!cancelled && map && typeof map === 'object') setLibrary(map);
                })
                .catch((error: unknown) => console.error('Cached: reading the library failed', error));
        };
        core.on('state', read);
        read();
        return () => {
            cancelled = true;
            core.off('state', read);
        };
    }, [core]);
    return library;
};

// Session cache of episode lists, keyed by addon + type + id. `null` is a
// failed fetch, retried after RETRY_MS (an addon that was down may be back).
const FETCH_TIMEOUT_MS = 10000;
const videoCache = new Map<string, { videos: MetaVideo[] | null, at: number }>();
const inFlight = new Map<string, Promise<MetaVideo[] | null>>();

const metaUrl = (transportUrl: string, type: string, id: string): string =>
    `${transportUrl.replace(/\/manifest\.json$/, '')}/meta/${encodeURIComponent(type)}/${encodeURIComponent(id)}.json`;

const fetchVideos = (key: string, url: string): Promise<MetaVideo[] | null> => {
    const running = inFlight.get(key);
    if (running !== undefined) return running;
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), FETCH_TIMEOUT_MS);
    const request = fetch(url, { signal: abort.signal })
        .then((resp) => (resp.ok ? resp.json() : Promise.reject(new Error(`${url} responded ${resp.status}`))))
        .then((body: { meta?: { videos?: unknown } }) => {
            const videos = body?.meta?.videos;
            if (!Array.isArray(videos)) throw new Error(`${url} has no video list`);
            return videos.filter((video): video is MetaVideo => !!video && typeof (video as MetaVideo).id === 'string');
        })
        .catch((error: unknown) => {
            console.warn('Cached: the episode list could not be fetched; viewing state stays unknown', url, error);
            return null;
        })
        .then((videos) => {
            videoCache.set(key, { videos, at: Date.now() });
            inFlight.delete(key);
            return videos;
        })
        .finally(() => clearTimeout(timer));
    inFlight.set(key, request);
    return request;
};

/**
 * The episode list of every identified series, keyed by the addon, type and id
 * it came from (`videoKey`): null when it could not be fetched, absent while
 * loading. Keys no longer wanted are dropped. A failure is retried once
 * RETRY_MS has passed (a timer per failed key, cleared on change).
 */
export const useTitleVideos = (entries: CacheEntry[] | null): Record<string, MetaVideo[] | null> => {
    const [videos, setVideos] = useState<Record<string, MetaVideo[] | null>>({});
    // Bumped by a retry timer to re-run the fetch effect for an unchanged list.
    const [retryTick, setRetryTick] = useState(0);
    const wanted = (entries ?? [])
        .map((entry) => entry.meta)
        .filter((meta): meta is NonNullable<CacheEntry['meta']> => details.videoKey(meta) !== null);
    const keys = [...new Set(wanted.map((meta) => details.videoKey(meta) as string))].sort();
    const signature = keys.join(',');
    useEffect(() => {
        let cancelled = false;
        const timers: ReturnType<typeof setTimeout>[] = [];
        setVideos((current) => details.keepWanted(current, keys));
        const scheduleRetry = (cached: { videos: MetaVideo[] | null, at: number }) => {
            const delay = details.retryInMs(cached, Date.now());
            if (delay !== null) timers.push(setTimeout(() => setRetryTick((tick) => tick + 1), delay + 50));
        };
        wanted.forEach((meta) => {
            const key = details.videoKey(meta) as string;
            const cached = videoCache.get(key);
            if (cached !== undefined && (cached.videos !== null || (details.retryInMs(cached, Date.now()) as number) > 0)) {
                setVideos((current) => (current[key] === cached.videos ? current : { ...current, [key]: cached.videos }));
                scheduleRetry(cached);
                return;
            }
            // fetchVideos dedupes a request already in flight for this key.
            fetchVideos(key, metaUrl(meta.metaTransportUrl as string, meta.type, meta.metaId)).then((list) => {
                if (cancelled) return;
                setVideos((current) => ({ ...current, [key]: list }));
                const after = videoCache.get(key);
                if (after !== undefined) scheduleRetry(after);
            });
        });
        return () => {
            cancelled = true;
            timers.forEach((timer) => clearTimeout(timer));
        };
        // The signature carries everything the effect reads from `wanted`.
    }, [signature, retryTick]);
    return videos;
};

/**
 * infoHash -> the torrent's whole file list (absent until read), for the
 * current streaming server only. Re-read when an entry's numbers move (bytes,
 * selection size, state); `reload` re-reads one at once (after a selection
 * change). An older response never overwrites a newer one (a request
 * generation per hash), and a hash that leaves the list is forgotten, so a
 * re-add starts from scratch.
 */
export const useFileLists = (entries: CacheEntry[] | null, serverUrl: unknown) => {
    const server = typeof serverUrl === 'string' ? serverUrl : '';
    const book = useRef<FileBook | null>(null);
    const [lists, setLists] = useState<Record<string, TorrentFile[]>>({});
    const commit = useCallback((next: FileBook) => {
        book.current = next;
        setLists(next.lists);
    }, []);
    const load = useCallback((infoHash: string) => {
        if (server.length === 0) return Promise.resolve();
        const started = details.beginRequest(details.bookFor(book.current, server), infoHash);
        book.current = started.book;
        return serverFetch(new URL(`cache/files/${infoHash}`, server))
            .then((resp) => {
                if (!resp.ok) throw new Error(`cache/files responded ${resp.status}`);
                return resp.json();
            })
            .then((list: TorrentFile[]) => {
                commit(details.applyResponse(details.bookFor(book.current, server), server, infoHash, started.generation, list));
            })
            .catch((error: unknown) => {
                // The row falls back to what /cache/list says; the next change retries.
                console.error('Cached: listing a torrent failed', infoHash, error);
                const current = details.bookFor(book.current, server);
                const { [infoHash]: _forgotten, ...signatures } = current.signatures;
                book.current = { ...current, signatures };
            });
    }, [server, commit]);
    useEffect(() => {
        if (entries === null) return;
        let current = details.prune(details.bookFor(book.current, server), entries.map((entry) => entry.infoHash));
        const toLoad: string[] = [];
        entries.forEach((entry) => {
            // A torrent set aside has no file list to read.
            if (entry.state === 'waiting') return;
            const signature = details.fileSignature(entry);
            if (current.signatures[entry.infoHash] === signature) return;
            current = { ...current, signatures: { ...current.signatures, [entry.infoHash]: signature } };
            toLoad.push(entry.infoHash);
        });
        commit(current);
        toLoad.forEach((infoHash) => load(infoHash));
    }, [entries, server, load, commit]);
    return { lists, reload: load };
};
