// Copyright (C) 2017-2026 Smart code 203358507

// Trickplay data source: asks the shell's shadow-mpv (src-tauri thumbs.rs) for
// a thumbnail at a time position. The shell answers with the frame, `pending`
// while it is being made (or its region is not downloaded yet), or `never` for
// a stream that gets no previews (thumbAnswer.js). So this asks again while
// the position is hovered and the answer is pending, for as long as that
// takes, and keeps its own per-bucket map for synchronous hits while the
// cursor sweeps back over ground already covered.
//
// Shell-only by construction: outside Tauri there is no shadow player, the
// hook stays inert and the seek bar simply shows no image.

import { useCallback, useEffect, useRef, useState } from 'react';
import { getTauri } from 'rillio/common/Platform/shell/isShell';

// Must match thumbs.rs BUCKET_SECONDS.
const BUCKET_SECONDS = 2;
// How often a pending position is asked again: the poll cadence only, never a
// deadline (the shell says when to stop).
const RETRY_MS = 100;

type ThumbAnswerRead = { frame: string | null, askAgain: boolean, never: string | null };
const { readThumbAnswer } = require('./thumbAnswer') as { readThumbAnswer: (answer: unknown) => ThumbAnswerRead };

// Captured pixels per drawn device pixel. Above 1 the page downscales the
// frame (the browser's own high-quality image filter), which hides the jpg's
// chroma subsampling and block edges that a 1:1 frame shows. Mirrored by
// thumbs.rs OVERSAMPLE, which only uses it to bound the request.
const OVERSAMPLE = 2;

const bucketOf = (timeSec: number) => Math.round(timeSec / BUCKET_SECONDS);

// How many pixels to capture for a thumbnail drawn `rem` wide right now: the
// DEVICE pixels it covers, times OVERSAMPLE. The root font size steps with the
// window width (tailwind.css) and the pixel ratio with the screen and the
// zoom, hence read live instead of assumed.
const capturePixels = (rem: number) => {
    const rootFontPx = parseFloat(getComputedStyle(document.documentElement).fontSize);
    return Math.ceil(rem * rootFontPx * window.devicePixelRatio * OVERSAMPLE);
};

type ThumbCache = {
    get: (widthPx: number, bucket: number) => string | null,
    set: (widthPx: number, bucket: number, dataUrl: string) => void,
    clear: () => void,
};
const { createThumbCache } = require('./thumbCache') as { createThumbCache: () => ThumbCache };

// `displayWidthRem` is the width the caller draws the thumbnail at.
const usePlayerThumb = (streamUrl: string | null, hoverTimeMs: number | null, displayWidthRem: number): string | null => {
    const tauri = getTauri();
    const enabled = tauri?.core?.invoke !== undefined && typeof streamUrl === 'string';
    // A bounded, width-aware map (see thumbCache.js): after a move to a denser
    // screen the frames captured for the old size must not be shown stretched.
    const cache = useRef<ThumbCache | null>(null);
    if (cache.current === null) cache.current = createThumbCache();
    const frames = cache.current;
    const [, bump] = useState(0);
    const widthPx = enabled ? capturePixels(displayWidthRem) : null;
    // Set once the shell says this stream never gets previews: nothing more
    // is asked for it.
    const never = useRef<string | null>(null);

    // A new stream invalidates every cached frame, and the shadow instance is
    // torn down when the player leaves (cheap: it holds a decoder + a
    // connection to the streaming server).
    useEffect(() => {
        frames.clear();
        never.current = null;
        const t = getTauri();
        // Warm the shadow up front: the FIRST hover otherwise pays the whole
        // shadow spawn (dll load + loadfile + first keyframe seek, ~1s) before
        // any preview can appear. Asking for bucket 0 now makes the shell spawn
        // the shadow while playback is starting, so the first real hover only
        // pays a warm seek+capture. The frame itself lands in the shell cache.
        if (typeof streamUrl === 'string' && t?.core?.invoke) {
            t.core.invoke('player_thumb', { url: streamUrl, timeSec: 0, widthPx: capturePixels(displayWidthRem) })
                .then((answer: unknown) => {
                    const read = readThumbAnswer(answer);
                    if (read.never !== null) never.current = read.never;
                })
                .catch((e: unknown) => console.warn('usePlayerThumb: warm-up', e));
        }
        return () => {
            t?.core?.invoke?.('player_thumb_stop').catch(() => { /* shell-only */ });
        };
    }, [streamUrl, displayWidthRem, frames]);

    const bucket = enabled && hoverTimeMs !== null ? bucketOf(hoverTimeMs / 1000) : null;
    // Reading marks the frame used, and is what tells the cache the width in
    // force (a new one empties it).
    const thumb = bucket !== null && widthPx !== null ? frames.get(widthPx, bucket) : null;

    const request = useCallback((wantedBucket: number, wantedWidthPx: number): Promise<ThumbAnswerRead> => {
        const t = getTauri();
        if (!t?.core?.invoke || typeof streamUrl !== 'string') {
            return Promise.resolve({ frame: null, askAgain: false, never: null });
        }
        return t.core.invoke('player_thumb', {
            url: streamUrl,
            timeSec: wantedBucket * BUCKET_SECONDS,
            widthPx: wantedWidthPx,
        }).then(readThumbAnswer);
    }, [streamUrl]);

    useEffect(() => {
        if (bucket === null || widthPx === null || never.current !== null || frames.get(widthPx, bucket) !== null) return;
        let cancelled = false;
        let timer: ReturnType<typeof setTimeout> | null = null;
        const ask = () => {
            if (cancelled) return;
            request(bucket, widthPx)
                .then((read) => {
                    if (cancelled) return;
                    if (read.frame !== null) {
                        frames.set(widthPx, bucket, read.frame);
                        bump((n) => n + 1);
                    } else if (read.never !== null) {
                        never.current = read.never;
                    } else if (read.askAgain) {
                        timer = setTimeout(ask, RETRY_MS);
                    }
                })
                // A failed invoke is not "pending": no image, and said.
                .catch((e: unknown) => console.warn('usePlayerThumb:', e));
        };
        ask();
        return () => {
            cancelled = true;
            if (timer !== null) clearTimeout(timer);
        };
    }, [bucket, widthPx, request, frames]);

    return thumb;
};

export default usePlayerThumb;
