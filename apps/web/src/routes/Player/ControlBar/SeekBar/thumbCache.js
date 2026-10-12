// Copyright (C) 2017-2026 Smart code 203358507

// The seek bar's own copy of the trickplay thumbnails (data URLs by bucket),
// for synchronous hits while the cursor sweeps back over ground already
// covered. A rolling set: once THUMB_CACHE_MAX frames are held, each new one
// overwrites the least recently used (a read counts as a use), the same order
// the shell's cache evicts in (thumbs.rs ThumbCache).
//
// Frames are only good for the pixel width they were captured at. Asking for
// another width (the window moved to a denser screen, the zoom changed) drops
// every frame held, and a frame that arrives late for the old width is refused.
//
// Plain CommonJS so jest can require it (tests/thumbCache.spec.js);
// usePlayerThumb uses it.

// ~39 KB per frame as a data URL at the 640 px a 125%-scaled 1440p screen asks
// for (measured, 1080p film): ~10 MB here, ~63 MB at the very worst (every
// frame a full 1920 px capture, ~246 KB). About 25 s of continuous scrubbing
// at the ten captures a second the shell lands. Mirrors thumbs.rs
// CACHE_MAX_THUMBS.
const THUMB_CACHE_MAX = 256;

const createThumbCache = (max = THUMB_CACHE_MAX) => {
    // A Map iterates in insertion order, so re-inserting on every use keeps
    // the least recently used frame first.
    const frames = new Map();
    let width = null;

    const get = (widthPx, bucket) => {
        if (widthPx !== width) {
            frames.clear();
            width = widthPx;
            return null;
        }
        const dataUrl = frames.get(bucket);
        if (dataUrl === undefined) return null;
        frames.delete(bucket);
        frames.set(bucket, dataUrl);
        return dataUrl;
    };

    const set = (widthPx, bucket, dataUrl) => {
        if (widthPx !== width) return;
        if (frames.has(bucket)) {
            frames.delete(bucket);
        } else if (frames.size >= max) {
            frames.delete(frames.keys().next().value);
        }
        frames.set(bucket, dataUrl);
    };

    // A new stream: the frames go, the width in force stays (it belongs to
    // the screen, and a request already in flight must still be able to land).
    const clear = () => frames.clear();

    return { get, set, clear, size: () => frames.size };
};

module.exports = { createThumbCache, THUMB_CACHE_MAX };
