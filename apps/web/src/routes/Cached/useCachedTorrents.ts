import { useCallback, useEffect, useRef, useState } from 'react';
import { useProfile } from 'rillio/common';
import useToast from 'rillio/common/Toast/useToast';
import { notifyCacheChanged } from 'rillio/common/cacheEvents';
import { serverFetch } from 'rillio/common/serverFetch';
import type { CacheMeta } from 'rillio/common/cacheMetadata';

export type CacheEntry = {
    infoHash: string,
    name: string,
    downloaded: number,
    total: number,
    // 'initializing' | 'live' | 'paused' | 'error', or 'waiting' for a torrent
    // that is in the cache but not in the engine yet (see `issue`).
    state: string,
    error?: string,
    // The server's ONE verdict on the entry's health: absent when it is fine,
    // otherwise what is wrong (`kind` for code, `reason` for a person). The UI
    // tags the row and offers "Fetch again" off this alone; it never derives
    // health from `state` itself.
    issue?: { kind: string, reason: string },
    pinned: boolean,
    // The player marked this stream watched: with streaming mode on, an
    // un-pinned watched entry cleans up automatically about an hour later.
    watched: boolean,
    // The addon metadata this torrent was matched to (see common/cacheMetadata),
    // absent until it is identified.
    meta?: CacheMeta,
    fileCount: number,
    // The single selected file's index when the entry is one playable file,
    // absent for multi-file selections. Powers the row's play button.
    fileIdx?: number,
    // When the torrent entered the cache, unix epoch ms (server-stamped on add,
    // backfilled from file times for caches that predate the stamp). Optional
    // because an older server build simply doesn't send it; the date sort
    // treats absent as oldest.
    addedAt?: number,
};

// The Cached page's data layer: polls the local streaming server's cache list
// while mounted (downloads progress live), and exposes pin/delete mutations.
// Talks to the server directly (same as the media player does) because the
// cache is a server-side concept the core model doesn't know about.
const useCachedTorrents = () => {
    const profile = useProfile();
    const toast = useToast();
    const serverUrl = profile.settings.streamingServerUrl;
    const [entries, setEntries] = useState<CacheEntry[] | null>(null);
    // Why the list could not be loaded, or null while it can (or has not been
    // tried). The Cache window shows it: "not reachable" alone told nobody
    // whether the server was still starting, had failed to start, or answered
    // with an error.
    const [error, setError] = useState<string | null>(null);
    const loggedError = useRef<string | null>(null);
    // Deleted rows disappear immediately (the next poll confirms), so the UI
    // never shows a "deleted" item bouncing back for a second.
    const hidden = useRef<Set<string>>(new Set());
    // Bumped by every mutation. The poll below runs on its own 3s timer, so a
    // request issued BEFORE a mutation can land AFTER it, carrying a snapshot
    // that predates the change - applying it would flip the row straight back
    // (this is what made Pause look like it "sometimes does not work": the
    // optimistic 'paused' got clobbered by an in-flight poll's 'live', and the
    // second click then un-paused). A response is only applied if no mutation
    // happened while it was in flight.
    const mutationSeq = useRef(0);

    const refresh = useCallback(() => {
        if (typeof serverUrl !== 'string') {
            // Returning quietly here left the window on "Loading..." for good.
            setError('No streaming server is set in the profile settings.');
            return;
        }
        const seq = mutationSeq.current;
        serverFetch(new URL('cache/list', serverUrl))
            .then((resp) => {
                if (!resp.ok) throw new Error(`cache/list responded ${resp.status}`);
                return resp.json();
            })
            .then((list: CacheEntry[]) => {
                if (seq !== mutationSeq.current) return;
                setEntries(list.filter((entry) => !hidden.current.has(entry.infoHash)));
                setError(null);
                loggedError.current = null;
            })
            .catch((reason: unknown) => {
                const message = reason instanceof Error ? reason.message : String(reason);
                // Once per distinct failure, not once per 3s poll.
                if (loggedError.current !== message) {
                    loggedError.current = message;
                    console.error('useCachedTorrents: cache/list failed', reason);
                }
                if (seq !== mutationSeq.current) return;
                setError(message);
            });
    }, [serverUrl]);

    // Mutations are POSTs that the server only acknowledges after the engine has
    // actually applied them, so re-polling once the POST settles converges the row
    // immediately instead of leaving the optimistic guess up for the next 3s tick.
    // A non-2xx is a real failure (an unknown infoHash is a 404) and must not read
    // as success: say so and re-poll to show the true state, never a silent revert.
    const mutate = useCallback((path: string, body: unknown, failureTitle: string, onFailure?: () => void) => {
        mutationSeq.current += 1;
        return serverFetch(new URL(path, serverUrl as string), {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify(body),
        })
            .then((resp) => {
                if (!resp.ok) throw new Error(`${path} responded ${resp.status}`);
            })
            .catch((error) => {
                console.error(`useCachedTorrents: ${path} failed`, error);
                toast.show({ type: 'error', title: failureTitle, message: 'The streaming service did not accept the change.' });
                // Runs BEFORE the refresh below, so an undo (un-hiding a row whose
                // delete failed) is already in place when the fresh list is applied.
                if (onFailure !== undefined) onFailure();
            })
            .then(() => {
                refresh();
                // Settled either way: on success the state changed, on failure the
                // optimistic guess is being rolled back. Both make anything else
                // watching the cache (the top-nav dot) wrong until it re-reads.
                notifyCacheChanged();
            });
    }, [serverUrl, refresh, toast]);

    useEffect(() => {
        refresh();
        const interval = setInterval(refresh, 3000);
        return () => clearInterval(interval);
    }, [refresh]);

    const setPinned = useCallback((infoHash: string, pinned: boolean) => {
        if (typeof serverUrl !== 'string') return;
        setEntries((current) => current !== null ?
            current.map((entry) => entry.infoHash === infoHash ? { ...entry, pinned } : entry)
            :
            current
        );
        mutate('cache/pin', { infoHash, pinned }, pinned ? 'Could not pin this download' : 'Could not unpin this download');
    }, [serverUrl, mutate]);

    const setPaused = useCallback((infoHash: string, paused: boolean) => {
        if (typeof serverUrl !== 'string') return;
        setEntries((current) => current !== null ?
            current.map((entry) => entry.infoHash === infoHash ? { ...entry, state: paused ? 'paused' : 'live' } : entry)
            :
            current
        );
        mutate('cache/pause', { infoHash, paused }, paused ? 'Could not pause this download' : 'Could not resume this download');
    }, [serverUrl, mutate]);

    // File-browser mutation: add a file in the torrent to the download
    // selection, or drop it. No optimistic update here - the browser re-fetches
    // its own list when this settles, and a half-applied selection is exactly
    // the state we must not draw.
    const setFileSelected = useCallback((infoHash: string, fileIdx: number, selected: boolean) => {
        if (typeof serverUrl !== 'string') return Promise.resolve();
        return mutate(
            'cache/select',
            { infoHash, fileIdx, selected },
            selected ? 'Could not download this file' : 'Could not stop downloading this file',
        );
    }, [serverUrl, mutate]);

    // "Fetch again" on a row with an issue: the server tries now to get what
    // the torrent is missing and answers when the attempt is over (for lost
    // details that can take a few minutes: it waits, bounded, for a peer).
    // The row shows it is under way; the result is said either way.
    const [refetching, setRefetching] = useState<string[]>([]);
    const refetch = useCallback((infoHash: string) => {
        if (typeof serverUrl !== 'string') return;
        setRefetching((current) => current.includes(infoHash) ? current : [...current, infoHash]);
        serverFetch(new URL('cache/refetch', serverUrl), {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ infoHash }),
        })
            .then(async (resp) => {
                if (!resp.ok) {
                    const body = await resp.json().catch(() => null) as { error?: unknown } | null;
                    throw new Error(typeof body?.error === 'string' ? body.error : `cache/refetch responded ${resp.status}`);
                }
                toast.show({ type: 'success', title: 'Fetched again', message: 'It is back in the cache.', timeout: 4000 });
            })
            .catch((error: unknown) => {
                console.error('useCachedTorrents: cache/refetch failed', error);
                toast.show({
                    type: 'error',
                    title: 'Could not fetch it again',
                    message: error instanceof Error ? error.message : String(error),
                    timeout: 10000,
                });
            })
            .then(() => {
                setRefetching((current) => current.filter((hash) => hash !== infoHash));
                mutationSeq.current += 1;
                refresh();
                notifyCacheChanged();
            });
    }, [serverUrl, refresh, toast]);

    const remove = useCallback((infoHash: string) => {
        if (typeof serverUrl !== 'string') return;
        hidden.current.add(infoHash);
        setEntries((current) => current !== null ?
            current.filter((entry) => entry.infoHash !== infoHash)
            :
            current
        );
        mutate('cache/delete', { infoHash }, 'Could not delete this download', () => {
            hidden.current.delete(infoHash);
        });
    }, [serverUrl, mutate]);

    return { entries, failed: error !== null, error, refresh, setPinned, setPaused, setFileSelected, remove, refetch, refetching };
};

export default useCachedTorrents;
