// Copyright (C) 2017-2026 Smart code 203358507

// Trickplay data source: asks the shell's shadow-mpv (src-tauri thumbs.rs) for
// a thumbnail at a time position. The shell answers from its cache instantly or
// returns null while its worker generates - so this polls gently (a few
// retries per position) and keeps its own per-bucket map for synchronous hits
// while the cursor sweeps back over ground already covered.
//
// Shell-only by construction: outside Tauri there is no shadow player, the
// hook stays inert and the seek bar simply shows no image.

import { useCallback, useEffect, useRef, useState } from 'react';
import { getTauri } from 'rillio/common/Platform/shell/isShell';

// Must match thumbs.rs BUCKET_SECONDS.
const BUCKET_SECONDS = 2;
const RETRY_MS = 100;
// A not-yet-generated bucket is retried this many times while hovered (a live
// torrent region that is not downloaded gives up until the next hover).
const MAX_TRIES = 30;

const bucketOf = (timeSec: number) => Math.round(timeSec / BUCKET_SECONDS);

// How many DEVICE pixels a width in rem covers right now. The shell captures
// the frame at exactly this width, so the image is never stretched: the root
// font size steps with the window width (tailwind.css) and the pixel ratio
// with the screen and the zoom, hence read live instead of assumed.
const devicePixels = (rem: number) => {
    const rootFontPx = parseFloat(getComputedStyle(document.documentElement).fontSize);
    return Math.ceil(rem * rootFontPx * window.devicePixelRatio);
};

// `displayWidthRem` is the width the caller draws the thumbnail at.
const usePlayerThumb = (streamUrl: string | null, hoverTimeMs: number | null, displayWidthRem: number): string | null => {
    const tauri = getTauri();
    const enabled = tauri?.core?.invoke !== undefined && typeof streamUrl === 'string';
    // Keyed by width AND bucket: after a move to a denser screen the frames
    // captured for the old size must not be shown stretched.
    const cache = useRef<Map<string, string>>(new Map());
    const [, bump] = useState(0);
    const widthPx = enabled ? devicePixels(displayWidthRem) : null;

    // A new stream invalidates every cached frame, and the shadow instance is
    // torn down when the player leaves (cheap: it holds a decoder + a
    // connection to the streaming server).
    useEffect(() => {
        cache.current = new Map();
        const t = getTauri();
        // Warm the shadow up front: the FIRST hover otherwise pays the whole
        // shadow spawn (dll load + loadfile + first keyframe seek, ~1s) before
        // any preview can appear. Asking for bucket 0 now makes the shell spawn
        // the shadow while playback is starting, so the first real hover only
        // pays a warm seek+capture. The frame itself lands in the shell cache.
        if (typeof streamUrl === 'string' && t?.core?.invoke) {
            t.core.invoke('player_thumb', { url: streamUrl, timeSec: 0, widthPx: devicePixels(displayWidthRem) })
                .catch(() => { /* shell-only */ });
        }
        return () => {
            t?.core?.invoke?.('player_thumb_stop').catch(() => { /* shell-only */ });
        };
    }, [streamUrl, displayWidthRem]);

    const bucket = enabled && hoverTimeMs !== null ? bucketOf(hoverTimeMs / 1000) : null;
    const key = bucket !== null && widthPx !== null ? `${widthPx}:${bucket}` : null;

    const request = useCallback((wantedBucket: number, wantedWidthPx: number) => {
        const t = getTauri();
        if (!t?.core?.invoke || typeof streamUrl !== 'string') return Promise.resolve(null);
        return t.core.invoke('player_thumb', {
            url: streamUrl,
            timeSec: wantedBucket * BUCKET_SECONDS,
            widthPx: wantedWidthPx,
        }) as Promise<string | null>;
    }, [streamUrl]);

    useEffect(() => {
        if (bucket === null || widthPx === null || key === null || cache.current.has(key)) return;
        let cancelled = false;
        let tries = 0;
        const ask = () => {
            if (cancelled) return;
            tries += 1;
            request(bucket, widthPx)
                .then((dataUrl) => {
                    if (cancelled) return;
                    if (typeof dataUrl === 'string') {
                        cache.current.set(key, dataUrl);
                        bump((n) => n + 1);
                    } else if (tries < MAX_TRIES) {
                        setTimeout(ask, RETRY_MS);
                    }
                })
                .catch(() => { /* no shell / no shadow: no image */ });
        };
        ask();
        return () => { cancelled = true; };
    }, [bucket, widthPx, key, request]);

    return key !== null ? cache.current.get(key) ?? null : null;
};

export default usePlayerThumb;
