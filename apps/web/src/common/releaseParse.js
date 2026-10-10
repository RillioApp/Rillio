// Copyright (C) 2017-2026 Smart code 203358507

// Reading scene names: the ONE parser for "what title / season / episode does
// this release name or this file path say it is". Two callers:
//
//   - parseReleaseName: a torrent's release name -> a searchable title (+ year,
//     + season/episode), for the cache metadata matcher (common/cacheMetadata).
//   - parseEpisode: one FILE inside a torrent -> its season/episode, so a
//     season pack can be matched to the episode being viewed file by file
//     (MetaDetails/StreamsList/cachedEpisodeMatch).
//
// Plain CommonJS so the jest suite can require it directly (this repo's jest
// has no TS transform); cacheMetadata.ts re-exports parseReleaseName.

// Everything after one of these tokens is release packaging, not the title.
const NOISE = /\b(2160p|1080p|720p|480p|4k|uhd|hdr10?\+?|dv|dolby|vision|hevc|x?26[45]|h ?26[45]|10bit|8bit|aac\d?|ac3|eac3|dts(-?hd)?|truehd|atmos|remux|bluray|blu-ray|bdrip|brrip|web-?dl|web-?rip|webrip|hdtv|dvdrip|amzn|nf|dsnp|hmax|atvp|repack|proper|extended|unrated|imax|multi|dual|subs?)\b/i;

// S01E03, S01.E03, "s01 e03", plus the further episodes of a combined file
// (S01E03E04, S01E03-E04, S01E03.E04, S01E03-05) in group 3. A bare "-NN"
// continuation must stand alone, so "S01E03-720p" is one episode.
const SEASON_EPISODE = /\bs(\d{1,2})[\s._-]?e(\d{1,3})((?:[\s-]?e\d{1,3}(?!\d)|-\d{1,3}(?![\da-z]))*)(?!\d)/i;
// 1x03 (never 1920x1080: the digits must stand alone on both sides).
const CROSS = /\b(\d{1,2})x(\d{2,3})\b/i;
// A season on its own: a pack's "S01" or a folder's "Season 1".
const SEASON_ONLY = /\b(?:s|season[\s-]?)(\d{1,2})\b/i;
// An episode on its own: "E07", "Ep 7", "Episode 07".
const EPISODE_ONLY = /\be(?:p|pisode)?[\s-]?(\d{1,3})\b/i;
// Anime-style absolute numbering: "Title - 03 [1080p]", "Title - 12v2 (x)",
// "Title - 05 - Episode name". The number must be followed by a bracket, a
// further " - " or the end, so "Concert - 5.1 Mix" and "Show - 1080p" are not
// episodes.
const DASH_NUMBER = /\s-\s(\d{1,3})(?:v\d+)?(?=\s*(?:[[(]|-\s|$))/i;
// "03 - Title" / "03": a bare number is only an episode inside a known season
// (a "Season 1" folder); on its own it is as likely a film called "300".
const LEADING_NUMBER = /^(\d{1,3})(?=\s|-|$)/;
const YEAR = /\b(19\d{2}|20\d{2})\b/;

const stripExtension = (name) => name.replace(/\.[a-z0-9]{2,4}$/i, '');
// Separators to spaces: scene names use dots and underscores for spaces.
const spaced = (name) => name.replace(/[._]+/g, ' ').replace(/\s+/g, ' ').trim();

/** Reduce a scene release name to a searchable title (+ year, + S/E). */
const parseReleaseName = (raw) => {
    const name = spaced(stripExtension(raw));

    const se = name.match(SEASON_EPISODE);
    const seasonOnly = se ? null : name.match(SEASON_ONLY);
    const season = se ? parseInt(se[1], 10) : seasonOnly ? parseInt(seasonOnly[1], 10) : null;
    const episode = se ? parseInt(se[2], 10) : null;

    // The title ends at whichever marker comes first: the season(/episode)
    // tag, the release year, or the first packaging token. Everything to the
    // left is the title, which is what a catalog search wants.
    const cuts = [se?.index, seasonOnly?.index, name.match(YEAR)?.index, name.match(NOISE)?.index]
        .filter((index) => typeof index === 'number' && index > 0);
    const cut = cuts.length > 0 ? Math.min(...cuts) : name.length;

    const title = name
        .slice(0, cut)
        // A trailing group tag or bracketed junk survives the cut sometimes.
        .replace(/[[({].*$/, '')
        .replace(/[-\s]+$/, '')
        .trim();

    return {
        title: title.length > 0 ? title : name,
        year: name.match(YEAR)?.[1] ?? null,
        season,
        episode,
    };
};

/**
 * The season/episode one file of a torrent holds, from its path inside the
 * torrent (folders included, either slash). Returns { season, episode,
 * lastEpisode } (season null for absolute numbering; lastEpisode > episode for
 * a combined file covering episode..lastEpisode, else equal), or null when the
 * path names no episode.
 */
const parseEpisode = (path) => {
    if (typeof path !== 'string' || path.length === 0) return null;
    const segments = path.split(/[\\/]+/).filter((segment) => segment.length > 0);
    if (segments.length === 0) return null;
    const rawFile = stripExtension(segments[segments.length - 1]);
    const file = spaced(rawFile);
    const folders = segments.slice(0, -1).map(spaced);

    const se = file.match(SEASON_EPISODE);
    if (se) {
        const episode = parseInt(se[2], 10);
        // The combined file runs to its highest named episode; a lower one
        // ("E04-E03") is not a range and is ignored.
        const further = (se[3].match(/\d+/g) ?? []).map((n) => parseInt(n, 10));
        const lastEpisode = Math.max(episode, ...further);
        return { season: parseInt(se[1], 10), episode, lastEpisode };
    }
    const cross = file.match(CROSS);
    if (cross) {
        const episode = parseInt(cross[2], 10);
        return { season: parseInt(cross[1], 10), episode, lastEpisode: episode };
    }

    // The season the file sits in: its own name first, then the nearest folder.
    const seasonSource = [file, ...folders.reverse()].map((part) => part.match(SEASON_ONLY)).find(Boolean);
    const season = seasonSource ? parseInt(seasonSource[1], 10) : null;

    const episodeOnly = file.match(EPISODE_ONLY);
    // Anime names use spaces and underscores but keep dots meaningful
    // ("5.1"), so this one reads the name with only underscores spaced.
    const dash = rawFile.replace(/_+/g, ' ').match(DASH_NUMBER);
    const leading = season !== null ? file.match(LEADING_NUMBER) : null;
    const episodeMatch = episodeOnly ?? dash ?? leading;
    if (!episodeMatch) return null;
    const episode = parseInt(episodeMatch[1], 10);
    return { season, episode, lastEpisode: episode };
};

module.exports = { parseReleaseName, parseEpisode };
