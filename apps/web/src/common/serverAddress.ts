// Copyright (C) 2017-2026 Smart code 203358507

// Where the local streaming server ACTUALLY is.
//
// The desktop shell binds the server on whatever port it can get (11470 first,
// an ephemeral port when that is refused), but the profile setting stays
// SYMBOLIC: the default http://127.0.0.1:11470/. A dynamic port must never be
// persisted - the core's ServerUrlsBucket accumulates every url it is ever
// given and the default string is treated as undeletable, so writing a boot's
// port into settings would litter the list forever and could leave a dead url
// selected on the next launch.
//
// So the real address is resolved HERE, at the edge, and applied when a request
// is issued (common/serverFetch) or a link is handed to another process
// (external players). Outside the shell there is no handshake at all: whatever
// the setting says IS the address.

import { DEFAULT_STREAMING_SERVER_URL } from 'rillio/common/CONSTANTS';
import { getTauri, isShell } from 'rillio/common/Platform/shell/isShell';

// The symbolic origin, i.e. DEFAULT_STREAMING_SERVER_URL minus its trailing
// slash. Every url the core builds from the default setting starts with this.
export const DEFAULT_SERVER_ORIGIN = DEFAULT_STREAMING_SERVER_URL.replace(/\/+$/, '');

// The shell answers null until the server has actually bound, so the handshake
// is a poll: fast at first (the bind usually lands within a few hundred ms of
// window creation), then backing off to a slow steady beat.
const POLL_MIN_DELAY_MS = 100;
const POLL_MAX_DELAY_MS = 2000;
const POLL_COMPLAIN_AFTER_MS = 30000;

const withTrailingSlash = (url: string): string => (url.endsWith('/') ? url : `${url}/`);
const withoutTrailingSlash = (url: string): string => url.replace(/\/+$/, '');

// null until the shell reports an address (and forever outside the shell,
// where getServerUrl falls back to the symbolic default).
let actualUrl: string | null = null;
let handshake: Promise<string> | null = null;

type Listener = (url: string) => void;
const listeners = new Set<Listener>();

const resolveActualUrl = (url: string): void => {
    actualUrl = withTrailingSlash(url);
    const resolved = actualUrl;
    listeners.forEach((listener) => {
        try {
            listener(resolved);
        } catch (error) {
            console.error('serverAddress: a subscriber threw', error);
        }
    });
};

const startHandshake = (): Promise<string> => new Promise((resolve) => {
    const startedAt = Date.now();
    let delay = POLL_MIN_DELAY_MS;
    let complained = false;

    const retry = (): void => {
        if (!complained && Date.now() - startedAt >= POLL_COMPLAIN_AFTER_MS) {
            complained = true;
            // FAIL LOUD: at this point every server-bound request is parked
            // waiting for an address that is not coming. Keep polling (a server
            // that recovers should still be picked up) but say so.
            console.error('serverAddress: the shell has not reported a streaming server address; requests are waiting');
        }
        delay = Math.min(Math.round(delay * 1.5), POLL_MAX_DELAY_MS);
        setTimeout(attempt, delay);
    };

    const attempt = (): void => {
        const invoke = getTauri()?.core?.invoke;
        if (typeof invoke !== 'function') {
            // The Tauri API object can attach a tick after the first scripts
            // run; that is a retry, not a failure.
            retry();
            return;
        }
        invoke('streaming_server_url')
            .then((url: unknown) => {
                // The command answers null until the listener is bound.
                if (typeof url !== 'string' || url.length === 0) {
                    retry();
                    return;
                }
                resolveActualUrl(url);
                resolve(actualUrl as string);
            })
            .catch((error: unknown) => {
                console.error('serverAddress: streaming_server_url failed', error);
                retry();
            });
    };

    attempt();
});

// Starts the poll the first time it is needed in the shell. isShell() is
// re-evaluated on every call rather than captured, so a probe that ran before
// the Tauri globals attached cannot pin us to the browser path.
const ensureHandshake = (): Promise<string> | null => {
    if (!isShell()) return null;
    if (handshake === null) handshake = startHandshake();
    return handshake;
};

/**
 * The address to build streaming-server urls against: the shell's real one once
 * the handshake has resolved, the symbolic default until then (and always, in a
 * browser). Always carries a trailing slash, so `new URL(path, getServerUrl())`
 * keeps the whole origin.
 */
export const getServerUrl = (): string => actualUrl ?? DEFAULT_STREAMING_SERVER_URL;

/** True when getServerUrl() is the real address rather than a placeholder. */
export const isServerUrlReady = (): boolean => actualUrl !== null || !isShell();

/**
 * Called with the real address once the shell reports it. Returns the
 * unsubscribe. Subscribers are NOT called for an address that already resolved
 * before they subscribed - read getServerUrl()/isServerUrlReady() for that.
 */
export const subscribeServerUrl = (listener: Listener): (() => void) => {
    listeners.add(listener);
    ensureHandshake();
    return () => { listeners.delete(listener); };
};

/**
 * Resolves with the real address. Rejects after `timeoutMs` so a caller waiting
 * on a server that never bound gets a real failure instead of a hung promise;
 * the underlying poll keeps running, so a later call succeeds as soon as the
 * server is up. The rejection carries the shell's own reason when it has one
 * (a failed start, a start still running), so the UI can say what is wrong.
 */
export const whenServerUrlReady = (timeoutMs = POLL_COMPLAIN_AFTER_MS): Promise<string> => {
    if (actualUrl !== null) return Promise.resolve(actualUrl);
    const pending = ensureHandshake();
    if (pending === null) return Promise.resolve(getServerUrl());
    return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
            const unexplained = 'the streaming server has not started';
            const invoke = getTauri()?.core?.invoke;
            Promise.resolve(typeof invoke === 'function' ? invoke('streaming_server_problem') : null)
                .catch((error: unknown) => {
                    console.error('serverAddress: streaming_server_problem failed', error);
                    return null;
                })
                .then((problem: unknown) => reject(new Error(
                    typeof problem === 'string' && problem.length > 0 ? problem : unexplained,
                )));
        }, timeoutMs);
        pending.then((url) => {
            clearTimeout(timer);
            resolve(url);
        });
    });
};

// Does `url` live under `origin`? A bare startsWith is not enough: it would
// accept a longer host (`http://127.0.0.1:114700`) and the userinfo trick
// (`http://127.0.0.1:11470@evil.example/`). Only a path, query or fragment may
// follow the origin.
const startsWithServerOrigin = (url: string, origin: string): boolean => {
    if (!url.startsWith(origin)) return false;
    const rest = url.slice(origin.length);
    return rest.length === 0 || /^[/?#]/.test(rest);
};

/**
 * True when `url` targets the LOCAL in-process server: either the symbolic
 * default origin or the actual origin the shell resolved. This is the one
 * check for "may this request go over IPC / may this stream read the local
 * engine"; a user-configured remote server never qualifies.
 */
export const isLocalServerUrl = (url: string): boolean => {
    if (typeof url !== 'string') return false;
    if (startsWithServerOrigin(url, DEFAULT_SERVER_ORIGIN)) return true;
    return actualUrl !== null && startsWithServerOrigin(url, withoutTrailingSlash(actualUrl));
};

/**
 * Point a url built from the SYMBOLIC default at the real server. Anything else
 * (a user-configured remote server, an addon url, an already-real url) is
 * returned untouched.
 */
export const rewriteServerUrl = (url: string): string => {
    if (typeof url !== 'string' || actualUrl === null) return url;
    const actualOrigin = withoutTrailingSlash(actualUrl);
    if (actualOrigin === DEFAULT_SERVER_ORIGIN) return url;
    if (!startsWithServerOrigin(url, DEFAULT_SERVER_ORIGIN)) return url;
    return actualOrigin + url.slice(DEFAULT_SERVER_ORIGIN.length);
};

/**
 * The same rewrite for a link that leaves the app (an external player, a
 * download). Core's m3u deep link is a BASE64 data uri with the streaming url
 * inside it, so a prefix swap cannot reach it: decode, rewrite, re-encode.
 * Returns null for a missing link, so callers can keep their own null checks.
 */
export const rewriteExternalPlayerUrl = (url: string | null | undefined): string | null => {
    if (typeof url !== 'string' || url.length === 0) return null;
    if (actualUrl === null) return url;
    const actualOrigin = withoutTrailingSlash(actualUrl);
    if (actualOrigin === DEFAULT_SERVER_ORIGIN) return url;

    const base64Marker = ';base64,';
    const markerAt = url.startsWith('data:') ? url.indexOf(base64Marker) : -1;
    if (markerAt === -1) return rewriteServerUrl(url);

    const head = url.slice(0, markerAt + base64Marker.length);
    const encoded = url.slice(markerAt + base64Marker.length);
    try {
        const decoded = atob(encoded);
        if (!decoded.includes(DEFAULT_SERVER_ORIGIN)) return url;
        return head + btoa(decoded.split(DEFAULT_SERVER_ORIGIN).join(actualOrigin));
    } catch (error) {
        // A playlist we cannot decode is a playlist we must not corrupt.
        console.error('serverAddress: rewriting the external player playlist failed', error);
        return url;
    }
};

// Kick the poll off at import time rather than on the first request: the shell
// binds asynchronously after window creation, and the sooner we have the real
// address the fewer requests park waiting for it.
ensureHandshake();
