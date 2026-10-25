// Copyright (C) 2017-2026 Smart code 203358507

// What a release name says about the COPY: resolution, source, streaming
// service, HDR kind, codec and release group. The Cache window's release line
// is built from it, which is what tells two downloads of the same episode
// apart ("2160p · ATVP WEB-DL · DV / HDR · FLUX" vs "1080p · WEB · H.264 ·
// SuccessfulCrab"). And cleanName: a person-readable name for a torrent no
// title was matched to.
//
// Title / season / episode reading stays in releaseParse (the one scene-name
// parser); this module only adds the packaging facets. Plain CommonJS so the
// jest suite can require it directly.

const { parseReleaseName, parseEpisode, stripPrefixes } = require('./releaseParse');

// Read on a spaced copy of the name ("H.265" -> "H 265", "WEB-DL" stays), so
// word boundaries work on dot-separated scene names.
const spacedForTokens = (name) => ` ${name.replace(/\.[a-z0-9]{2,4}$/i, '').replace(/[._()[\]]+/g, ' ')} `;

const RESOLUTIONS = [
    [/\b(?:2160p|4k|uhd)\b/i, '2160p'],
    [/\b1440p\b/i, '1440p'],
    [/\b1080[pi]\b/i, '1080p'],
    [/\b720p\b/i, '720p'],
    [/\b(?:576|480)p\b/i, '480p'],
];

const SOURCES = [
    [/\bremux\b/i, (text) => (/\b(?:blu-?ray|bd)\b/i.test(text) ? 'BluRay REMUX' : 'REMUX')],
    [/\bweb[ -]?dl\b/i, () => 'WEB-DL'],
    [/\bweb[ -]?rip\b/i, () => 'WEBRip'],
    [/\b(?:bd|br)[ -]?rip\b/i, () => 'BDRip'],
    [/\bblu-?ray\b/i, () => 'BluRay'],
    [/\bhdtv\b/i, () => 'HDTV'],
    [/\bdvd-?rip\b/i, () => 'DVDRip'],
    [/\bweb\b/i, () => 'WEB'],
];

// The streaming service a WEB release was taken from, as scene names spell it.
const SERVICES = ['ATVP', 'AMZN', 'NF', 'DSNP', 'HMAX', 'MAX', 'HULU', 'PCOK', 'PMTP', 'CR', 'iT', 'STAN', 'CRAV'];
const SERVICE = new RegExp(`\\b(${SERVICES.join('|')})\\b`);

const CODECS = [
    [/\b(?:x ?265|h ?265|hevc)\b/i, 'H.265'],
    [/\b(?:x ?264|h ?264|avc)\b/i, 'H.264'],
    [/\bav1\b/i, 'AV1'],
    [/\bvp9\b/i, 'VP9'],
    [/\b(?:xvid|divx)\b/i, 'XviD'],
];

const hdrKinds = (text) => {
    const kinds = [];
    if (/\b(?:dv|dovi|dolby ?vision)\b/i.test(text)) kinds.push('DV');
    if (/\bhdr10(?:\+|plus)/i.test(text)) kinds.push('HDR10+');
    else if (/\bhdr10\b/i.test(text)) kinds.push('HDR10');
    else if (/\bhdr\b/i.test(text)) kinds.push('HDR');
    return kinds;
};

const hdrLabelOf = (kinds) => {
    if (kinds.length === 0) return null;
    if (kinds.length === 1) return kinds[0] === 'DV' ? 'Dolby Vision' : kinds[0];
    return kinds.join(' / ');
};

// Words that end a scene name but are packaging, never a group.
const NOT_A_GROUP = /^(?:dl|rip|web|hdtv|x26[45]|h26[45]|hevc|avc|aac|ac3|dts|atmos|remux|bluray|\d+p|\d+bit|subs?|multi|dual|eng?)$/i;
// Bracketed tails that are a tracker's mark or a checksum, not the group.
const TRAILING_TAGS = /(?:\s*\[(?:[0-9a-f]{8}|tgx|rarbg|eztv(?:\.re)?|ettv|yts(?:\.\w+)?)\])+\s*$/i;

const groupOf = (raw) => {
    const name = raw.replace(/\.[a-z0-9]{2,4}$/i, '').trim();
    // Anime: the group leads, in brackets ("[SubsPlease] Title - 12").
    const leading = name.match(/^\s*\[([^\]]+)\]/);
    if (leading && !/\.\w{2,6}$/.test(leading[1].trim())) return leading[1].trim();
    const bare = name.replace(TRAILING_TAGS, '');
    // Scene: "...x264-GROUP" (a group may contain dots: "MeM.GP").
    const dashed = bare.match(/-([A-Za-z0-9][A-Za-z0-9.]{1,24})$/);
    if (dashed && !NOT_A_GROUP.test(dashed[1]) && /[A-Za-z]/.test(dashed[1])) return dashed[1];
    // Encoder-style: the group is the last word of the last parenthesis,
    // "(... Atmos 5.1 Ghost)" or a parenthesis of its own "(elladajarek)".
    const paren = bare.match(/\(([^()]*)\)\s*$/);
    if (paren) {
        const words = paren[1].trim().split(/\s+/);
        const last = words[words.length - 1];
        if (/^[A-Za-z][A-Za-z0-9]{1,24}$/.test(last) && !NOT_A_GROUP.test(last)) {
            const alone = words.length === 1;
            const afterPackaging = words.length > 1 && RESOLUTIONS.some(([re]) => re.test(paren[1]));
            if (alone || afterPackaging) return last;
        }
    }
    return null;
};

/** The packaging facets of a release name. Absent facets are null / []. */
const parseReleaseInfo = (raw) => {
    const name = typeof raw === 'string' ? raw : '';
    const text = spacedForTokens(name);
    const resolution = (RESOLUTIONS.find(([re]) => re.test(text)) ?? [null, null])[1];
    const sourceHit = SOURCES.find(([re]) => re.test(text));
    const service = text.match(SERVICE)?.[1] ?? null;
    const hdr = hdrKinds(text);
    return {
        resolution,
        source: sourceHit ? sourceHit[1](text) : null,
        service,
        hdr,
        hdrLabel: hdrLabelOf(hdr),
        codec: (CODECS.find(([re]) => re.test(text)) ?? [null, null])[1],
        group: groupOf(name),
    };
};

/**
 * The release line, as parts in display order: resolution (strong), service +
 * source, HDR kind, the codec only when nothing else says what kind of copy
 * this is (no HDR), then the group. Empty when the name says none of it.
 */
const releaseLine = (info) => {
    const parts = [];
    if (info.resolution) parts.push({ text: info.resolution, strong: true });
    const source = [info.service, info.source].filter(Boolean).join(' ');
    if (source.length > 0) parts.push({ text: source });
    if (info.hdrLabel) parts.push({ text: info.hdrLabel });
    else if (info.codec) parts.push({ text: info.codec });
    if (info.group) parts.push({ text: info.group });
    return parts;
};

/**
 * A readable name for a torrent no title was matched to: the title the name
 * carries (prefixes, packaging and dots gone), its year, and the season /
 * episode it names (an anime release's absolute number as `episode` with no
 * season). Falls back to the raw name when there is nothing to clean.
 */
const cleanName = (raw) => {
    const parsed = parseReleaseName(raw);
    const episode = parsed.episode === null ? parseEpisode(stripPrefixes(raw)) : null;
    return {
        title: parsed.title,
        year: parsed.year,
        season: parsed.season,
        episode: parsed.episode ?? (episode !== null && episode.season === null ? episode.episode : null),
    };
};

module.exports = { parseReleaseInfo, releaseLine, cleanName };
