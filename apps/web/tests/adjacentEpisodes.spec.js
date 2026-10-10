// The streams view's previous/next episode arrows step through the title's
// episode list in season-then-episode order, across season boundaries, with
// specials left out unless the current episode is one.

const { adjacentEpisodes, episodeLabel } = require('../src/routes/MetaDetails/adjacentEpisodes');

const ep = (season, episode, title = `Episode ${episode}`, id = `tt1:${season}:${episode}`) => ({
    id,
    season,
    episode,
    title,
    deepLinks: { metaDetailsStreams: `#/metadetails/series/tt1/${encodeURIComponent(id)}` },
});

const ids = ({ previous, next }) => [previous && previous.id, next && next.id];

describe('adjacentEpisodes', () => {
    // Deliberately out of order, as addons do send them.
    const videos = [
        ep(2, 1), ep(1, 3, 'Nixon\'s Women'), ep(1, 1), ep(0, 1, 'Behind the scenes'),
        ep(1, 2, 'He Built the Saturn V'), ep(2, 2), ep(0, 2),
    ];

    it('returns the neighbours of an ordinary episode, with labels and deep links', () => {
        const { previous, next } = adjacentEpisodes('series', videos, 'tt1:1:2');
        expect(previous).toEqual({
            id: 'tt1:1:1', season: 1, episode: 1, title: 'Episode 1', label: 'S1E1 Episode 1',
            link: '#/metadetails/series/tt1/tt1%3A1%3A1',
        });
        expect(next.id).toBe('tt1:1:3');
        expect(next.label).toBe('S1E3 Nixon\'s Women');
    });

    it('has no previous at the first episode and no next at the last', () => {
        expect(ids(adjacentEpisodes('series', videos, 'tt1:1:1'))).toEqual([null, 'tt1:1:2']);
        expect(ids(adjacentEpisodes('series', videos, 'tt1:2:2'))).toEqual(['tt1:2:1', null]);
    });

    it('crosses season boundaries both ways', () => {
        expect(ids(adjacentEpisodes('series', videos, 'tt1:1:3'))).toEqual(['tt1:1:2', 'tt1:2:1']);
        expect(ids(adjacentEpisodes('series', videos, 'tt1:2:1'))).toEqual(['tt1:1:3', 'tt1:2:2']);
    });

    it('skips specials from a regular episode, includes them from a special', () => {
        // S1E1 is the first regular episode: the specials before it are not offered.
        expect(adjacentEpisodes('series', videos, 'tt1:1:1').previous).toBeNull();
        expect(ids(adjacentEpisodes('series', videos, 'tt1:0:1'))).toEqual([null, 'tt1:0:2']);
        // The last special leads into the first regular episode.
        expect(ids(adjacentEpisodes('series', videos, 'tt1:0:2'))).toEqual(['tt1:0:1', 'tt1:1:1']);
    });

    it('leaves out videos with missing or unusable numbers without crashing', () => {
        const messy = [
            ep(1, 1),
            { id: 'tt1:1:x', season: 1, title: 'No episode number', deepLinks: {} },
            { id: 'tt1:nan', season: Number.NaN, episode: 2 },
            { id: 'tt1:str', season: '1', episode: '2' },
            null,
            'garbage',
            { season: 1, episode: 2 },
            ep(1, 3),
        ];
        expect(ids(adjacentEpisodes('series', messy, 'tt1:1:1'))).toEqual([null, 'tt1:1:3']);
        expect(ids(adjacentEpisodes('series', messy, 'tt1:1:3'))).toEqual(['tt1:1:1', null]);
        // The current video itself has no number: nothing to step through.
        expect(adjacentEpisodes('series', messy, 'tt1:1:x')).toEqual({ previous: null, next: null });
    });

    it('handles duplicate ids and duplicate episode numbers', () => {
        const dupes = [
            ep(1, 1), ep(1, 1), // the same video listed twice
            ep(1, 2, 'Cut A', 'tt1:1:2a'), ep(1, 2, 'Cut B', 'tt1:1:2b'), // two videos, one number
            ep(1, 3),
        ];
        expect(ids(adjacentEpisodes('series', dupes, 'tt1:1:1'))).toEqual([null, 'tt1:1:2a']);
        expect(ids(adjacentEpisodes('series', dupes, 'tt1:1:2a'))).toEqual(['tt1:1:1', 'tt1:1:2b']);
        expect(ids(adjacentEpisodes('series', dupes, 'tt1:1:2b'))).toEqual(['tt1:1:2a', 'tt1:1:3']);
    });

    it('shows nothing for a movie, an unknown id, or no episode list', () => {
        const movie = [{ id: 'tt9', title: 'A Movie', deepLinks: { metaDetailsStreams: '#/x' } }];
        expect(adjacentEpisodes('series', movie, 'tt9')).toEqual({ previous: null, next: null });
        expect(adjacentEpisodes('series', videos, 'tt1:9:9')).toEqual({ previous: null, next: null });
        expect(adjacentEpisodes('series', [], 'tt1:1:1')).toEqual({ previous: null, next: null });
        expect(adjacentEpisodes('series', undefined, 'tt1:1:1')).toEqual({ previous: null, next: null });
        expect(adjacentEpisodes('series', videos, null)).toEqual({ previous: null, next: null });
    });

    it('shows nothing for a movie or channel meta, even with numbered videos', () => {
        // The same numbered list that yields arrows for a series.
        expect(adjacentEpisodes('movie', videos, 'tt1:1:2')).toEqual({ previous: null, next: null });
        expect(adjacentEpisodes('channel', videos, 'tt1:1:2')).toEqual({ previous: null, next: null });
        expect(adjacentEpisodes(undefined, videos, 'tt1:1:2')).toEqual({ previous: null, next: null });
        expect(ids(adjacentEpisodes('series', videos, 'tt1:1:2'))).toEqual(['tt1:1:1', 'tt1:1:3']);
    });

    it('gives a null link when a neighbour carries no deep link', () => {
        const bare = [ep(1, 1), { id: 'tt1:1:2', season: 1, episode: 2 }];
        const { next } = adjacentEpisodes('series', bare, 'tt1:1:1');
        expect(next.link).toBeNull();
        expect(next.label).toBe('S1E2');
    });

    it('formats labels like the streams header', () => {
        expect(episodeLabel({ season: 1, episode: 3, title: 'Nixon\'s Women' })).toBe('S1E3 Nixon\'s Women');
        expect(episodeLabel({ season: 2, episode: 10, title: '' })).toBe('S2E10');
    });
});
