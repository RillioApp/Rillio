// Copyright (C) 2017-2026 Smart code 203358507

// The episodes either side of the one on screen, for the streams view's
// previous/next arrows. Plain JS so jest loads it directly (jest-covered in
// tests/adjacentEpisodes.spec.js); EpisodeArrows.tsx renders the result.
//
// Order is the title's whole episode list sorted by season then episode, so
// it crosses season boundaries (last of S1 -> first of S2). Specials (season
// 0) are left out unless the current episode is itself a special. A video
// without a usable season/episode number cannot be placed in that order and
// is left out; when the CURRENT video has none (a movie, a channel upload)
// there is nothing to step through and both sides are null.

const isEpisodeNumber = (value) => typeof value === 'number' && Number.isFinite(value);

const isEpisode = (video) => video !== null &&
    typeof video === 'object' &&
    typeof video.id === 'string' &&
    isEpisodeNumber(video.season) &&
    isEpisodeNumber(video.episode);

// "S1E3 Nixon's Women", the format the streams header already uses.
const episodeLabel = ({ season, episode, title }) =>
    `S${season}E${episode}${typeof title === 'string' && title.length > 0 ? ` ${title}` : ''}`;

const toTarget = (video) => ({
    id: video.id,
    season: video.season,
    episode: video.episode,
    title: typeof video.title === 'string' ? video.title : '',
    label: episodeLabel(video),
    // The same deep link the episode list follows (Video.tsx), so the arrow
    // lands on exactly the route a click in the list would.
    link: video.deepLinks !== null && typeof video.deepLinks === 'object' && typeof video.deepLinks.metaDetailsStreams === 'string' ?
        video.deepLinks.metaDetailsStreams
        :
        null,
});

const NONE = Object.freeze({ previous: null, next: null });

const adjacentEpisodes = (videos, currentVideoId) => {
    if (!Array.isArray(videos) || typeof currentVideoId !== 'string') {
        return NONE;
    }

    // One entry per id (an addon listing the same video twice is one episode),
    // keeping the first occurrence.
    const seen = new Set();
    const episodes = [];
    videos.forEach((video, index) => {
        if (isEpisode(video) && !seen.has(video.id)) {
            seen.add(video.id);
            episodes.push({ video, index });
        }
    });

    const current = episodes.find(({ video }) => video.id === currentVideoId);
    if (current === undefined) {
        return NONE;
    }

    const includeSpecials = current.video.season === 0;
    // Distinct ids sharing a season/episode number keep their list order (the
    // index tiebreak), so stepping through them is stable rather than skipping.
    const ordered = episodes
        .filter(({ video }) => includeSpecials || video.season !== 0)
        .sort((a, b) => (a.video.season - b.video.season) ||
            (a.video.episode - b.video.episode) ||
            (a.index - b.index));

    const position = ordered.indexOf(current);
    return {
        previous: position > 0 ? toTarget(ordered[position - 1].video) : null,
        next: position < ordered.length - 1 ? toTarget(ordered[position + 1].video) : null,
    };
};

module.exports = { adjacentEpisodes, episodeLabel };
