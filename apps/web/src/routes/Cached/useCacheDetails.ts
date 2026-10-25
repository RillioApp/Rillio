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
const RETRY_MS = 5 * 60 * 1000;
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

/** metaId -> its episode list, null when it could not be fetched; absent while loading. */
export const useTitleVideos = (entries: CacheEntry[] | null): Record<string, MetaVideo[] | null> => {
    const [videos, setVideos] = useState<Record<string, MetaVideo[] | null>>({});
    const wanted = (entries ?? [])
        .map((entry) => entry.meta)
        .filter((meta): meta is NonNullable<CacheEntry['meta']> =>
            meta !== undefined && meta.type !== 'movie' && typeof meta.metaTransportUrl === 'string' && meta.metaTransportUrl.length > 0);
    const signature = wanted.map((meta) => `${meta.metaTransportUrl}|${meta.type}|${meta.metaId}`).sort().join(',');
    useEffect(() => {
        let cancelled = false;
        wanted.forEach((meta) => {
            const key = `${meta.metaTransportUrl}|${meta.type}|${meta.metaId}`;
            const cached = videoCache.get(key);
            if (cached !== undefined && (cached.videos !== null || Date.now() - cached.at < RETRY_MS)) {
                setVideos((current) => (current[meta.metaId] === cached.videos ? current : { ...current, [meta.metaId]: cached.videos }));
                return;
            }
            fetchVideos(key, metaUrl(meta.metaTransportUrl as string, meta.type, meta.metaId)).then((list) => {
                if (!cancelled) setVideos((current) => ({ ...current, [meta.metaId]: list }));
            });
        });
        return () => { cancelled = true; };
        // The signature carries everything the effect reads from `wanted`.
    }, [signature]);
    return videos;
};

/**
 * infoHash -> the torrent's whole file list (absent until read). Re-read when
 * an entry's numbers move (bytes, selection size, state); `reload` re-reads
 * one at once (after a selection change).
 */
export const useFileLists = (entries: CacheEntry[] | null, serverUrl: unknown) => {
    const [lists, setLists] = useState<Record<string, TorrentFile[]>>({});
    const seen = useRef<Record<string, string>>({});
    const load = useCallback((infoHash: string) => {
        if (typeof serverUrl !== 'string') return Promise.resolve();
        return serverFetch(new URL(`cache/files/${infoHash}`, serverUrl))
            .then((resp) => {
                if (!resp.ok) throw new Error(`cache/files responded ${resp.status}`);
                return resp.json();
            })
            .then((list: TorrentFile[]) => setLists((current) => ({ ...current, [infoHash]: list })))
            .catch((error: unknown) => {
                // The row falls back to what /cache/list says; a later change retries.
                console.error('Cached: listing a torrent failed', infoHash, error);
                delete seen.current[infoHash];
            });
    }, [serverUrl]);
    useEffect(() => {
        (entries ?? []).forEach((entry) => {
            // A torrent set aside has no file list to read.
            if (entry.state === 'waiting') return;
            const signature = `${entry.downloaded}|${entry.total}|${entry.fileCount}|${entry.state}`;
            if (seen.current[entry.infoHash] === signature) return;
            seen.current[entry.infoHash] = signature;
            load(entry.infoHash);
        });
    }, [entries, load]);
    return { lists, reload: load };
};
