// Copyright (C) 2017-2026 Smart code 203358507

// Which videos of a title the library marks watched: a decoder for the core's
// serialized watched field (crates/watched-bitfield), so the Cache window can
// say, per episode of a season pack, what was watched.
//
// The field is `{anchorVideoId}:{anchorLength}:{base64(zlib(bits))}`, the
// anchor being the last watched video and its 1-based position when the bits
// were written. Bit i is byte i/8, bit i%8 (least significant first). The bits
// are positions in the title's video list ORDERED the core's way (see
// orderVideoIds); the list can have grown or shifted since (a new episode, a
// special prepended), which the anchor corrects for: the anchor video's
// position in today's list says how far to shift.
//
// One deliberate difference from the core: when the anchor video is not in
// today's list the core answers "nothing watched"; that is not knowledge, so
// this returns null (unknown) and the UI says nothing about the title.
//
// Checked against strings the core's encoder produced
// (tests/fixtures/watchedBitfield.json, crates/watched-bitfield/tests/js_fixtures.rs).
// Plain CommonJS so the jest suite can require it directly.

const { unzlibSync } = require('fflate');

const base64Bytes = (text) => {
    const binary = atob(text);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
    return bytes;
};

// `{id}:{len}:{buf}`, where the id itself may contain colons (pop from the right).
const parseWatchedField = (serialized) => {
    if (typeof serialized !== 'string') return null;
    const parts = serialized.split(':');
    if (parts.length < 3) return null;
    const buffer = parts.pop();
    const lengthText = parts.pop();
    if (!/^\d+$/.test(lengthText)) return null;
    let bits;
    try {
        bits = unzlibSync(base64Bytes(buffer));
    } catch {
        return null;
    }
    return { anchorVideo: parts.join(':'), anchorLength: parseInt(lengthText, 10), bits };
};

const bitAt = (bits, index) => {
    const byte = Math.floor(index / 8);
    return byte >= 0 && byte < bits.length ? ((bits[byte] >> (index % 8)) & 1) === 1 : false;
};

/**
 * Watched flags for `videoIds` (already in the core's order), or null when
 * they cannot be known: a missing field, a malformed one, an empty list, or an
 * anchor video that is not in the list.
 *
 * A missing field is unknown, not "nothing watched". core-web sends
 * `watchedField: null` when the core holds no bitfield, and a library entry
 * from before the field existed has none at all; neither proves that no
 * episode was watched (the old entry may even say `watched: true`), and a
 * false "Not started" is worse than saying nothing.
 */
const decodeWatched = (serialized, videoIds) => {
    if (!Array.isArray(videoIds) || videoIds.length === 0) return null;
    if (serialized === null || serialized === undefined) return null;
    const field = parseWatchedField(serialized);
    if (field === null) return null;
    const anchorIndex = videoIds.indexOf(field.anchorVideo);
    if (anchorIndex === -1) return null;
    const offset = field.anchorLength - anchorIndex - 1;
    // The bitfield's own length is its byte count * 8 (the core's
    // new_with_values with no length), so nothing past it reads as watched.
    const length = field.bits.length * 8;
    return videoIds.map((_, i) => {
        const previous = i + offset;
        return previous >= 0 && previous < length ? bitAt(field.bits, previous) : false;
    });
};

// The core reads season and episode as ONE optional `series_info` (both or
// neither), so a video with only one of them sorts as having neither.
const isNumber = (value) => typeof value === 'number' && Number.isFinite(value);
const seriesKey = (video, field) => (isNumber(video.season) && isNumber(video.episode) ? video[field] : -Infinity);
const releasedKey = (video) => {
    const time = typeof video.released === 'string' ? Date.parse(video.released) : NaN;
    return Number.isFinite(time) ? time : -Infinity;
};
const compareKeys = (a, b) => (a === b ? 0 : a < b ? -1 : 1);

/**
 * A title's video ids in the order the core numbers its watched bits: by
 * season, then episode, then release date, a missing value sorting first
 * (LibraryItemState::watched_bitfield). Videos are the addon meta's `videos`.
 */
const orderVideoIds = (videos) => (Array.isArray(videos) ? videos : [])
    .filter((video) => video && typeof video.id === 'string')
    .map((video, index) => ({ video, index }))
    .sort((a, b) =>
        compareKeys(seriesKey(a.video, 'season'), seriesKey(b.video, 'season')) ||
        compareKeys(seriesKey(a.video, 'episode'), seriesKey(b.video, 'episode')) ||
        compareKeys(releasedKey(a.video), releasedKey(b.video)) ||
        a.index - b.index)
    .map(({ video }) => video.id);

module.exports = { decodeWatched, orderVideoIds, parseWatchedField };
