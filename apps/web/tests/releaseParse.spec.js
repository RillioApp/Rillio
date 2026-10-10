// Release-name and file-path parsing: the ONE place a scene name or a file
// inside a torrent is read for its title / season / episode. Both the cache
// metadata matcher (parseReleaseName) and the streams list's "On this device"
// (parseEpisode, per file of a season pack) use it.

const { parseReleaseName, parseEpisode } = require('../src/common/releaseParse');

describe('parseEpisode', () => {
    const cases = [
        // Scene SxxEyy, with and without the "For All" the owner's pack lacks.
        ['For.All.Mankind.S01E03.Nixons.Women.2160p.ATVP.WEB-DL.DDP5.1.Atmos.DoVi.HEVC.by.DVT.mp4', 1, 3],
        ['Mankind.S01E03.Nixons.Women.2160p.ATVP.WEB-DL.DDP5.1.Atmos.DoVi.HEVC.by.DVT.mp4', 1, 3],
        ['For.All.Mankind.S01.2160p.ATVP.WEB-DL-DVT/Mankind.S01E04.He.Built.the.Saturn.V.2160p.mp4', 1, 4],
        ['show s02 e10 720p.mkv', 2, 10],
        // 1x03.
        ['show.1x03.hdtv.mkv', 1, 3],
        ['Show - 2x11 - Title.avi', 2, 11],
        // Season from a folder, episode from the file.
        ['For All Mankind/Season 1/03 - Nixons Women.mkv', 1, 3],
        ['Season 02/Episode 5.mkv', 2, 5],
        ['Show.S02.1080p/Show.E07.1080p.mkv', 2, 7],
        ['Show (2019)\\Season.3\\Ep 12.mkv', 3, 12],
        // Anime absolute numbering: no season.
        ['[SubsPlease] Frieren - 03 (1080p) [ABCD1234].mkv', null, 3],
        ['[Group] Some Show - 12v2 [1080p].mkv', null, 12],
        ['Some_Show_-_07_[720p].mkv', null, 7],
        ['Show - 05 - The Title [1080p].mkv', null, 5],
    ];
    test.each(cases)('%s -> S%s E%s', (path, season, episode) => {
        expect(parseEpisode(path)).toEqual({ season, episode, lastEpisode: episode });
    });

    // A combined file covers every episode it names: it must match on each
    // of those pages, not only the first.
    const multi = [
        ['Show.S01E03E04.1080p.mkv', 1, 3, 4],
        ['Show.S01E03-E04.1080p.mkv', 1, 3, 4],
        ['Show.S01E03.E04.1080p.mkv', 1, 3, 4],
        ['Show S01E03-05 Title.mkv', 1, 3, 5],
        ['Show.S01E03E04E05.mkv', 1, 3, 5],
    ];
    test.each(multi)('%s -> S%s E%s-E%s', (path, season, episode, lastEpisode) => {
        expect(parseEpisode(path)).toEqual({ season, episode, lastEpisode });
    });

    // What follows an episode tag is not automatically more episodes.
    test.each([
        ['Show.S01E03-720p.mkv', 1, 3],
        ['Show.S01E03.1080p.x265.mkv', 1, 3],
        ['Show.S01E04-E03.mkv', 1, 4],
    ])('%s -> S%s E%s only', (path, season, episode) => {
        expect(parseEpisode(path)).toEqual({ season, episode, lastEpisode: episode });
    });

    const negatives = [
        // Resolutions, codecs, audio channels and years are not episodes.
        'Movie.2019.1080p.x265-GRP.mkv',
        'Movie 1920x1080.mkv',
        '720p.mkv',
        '1080p.mkv',
        'Movie.DDP5.1.Atmos.mkv',
        'Movie.H.264.AAC2.0.mkv',
        'Show - 1080p.mkv',
        'Concert - 5.1 Mix.mkv',
        'Movie.E-AC3.mkv',
        'sample.mkv',
        // A bare leading number is only an episode inside a season folder:
        // "300" here is a film title.
        '300.2006.1080p.BluRay.mkv',
        '',
    ];
    test.each(negatives)('%s -> null', (path) => {
        expect(parseEpisode(path)).toBeNull();
    });
});

describe('parseReleaseName', () => {
    test('a movie release: title and year', () => {
        expect(parseReleaseName('Some.Movie.2026.1080p.HEVC-GRP')).toEqual({
            title: 'Some Movie', year: '2026', season: null, episode: null,
        });
    });

    test('an episode release keeps season and episode', () => {
        expect(parseReleaseName('For.All.Mankind.S01E03.Nixons.Women.2160p.ATVP.WEB-DL.mp4')).toEqual({
            title: 'For All Mankind', year: null, season: 1, episode: 3,
        });
    });

    // A season pack's name carries only a season tag. It used to survive into
    // the title ("For All Mankind S01"), which then never equalled any catalog
    // name, so whole-season packs were never identified.
    test('a season pack: the season tag ends the title', () => {
        expect(parseReleaseName('For.All.Mankind.S01.2160p.ATVP.WEB-DL.DDP5.1.Atmos.DoVi.HEVC-DVT')).toEqual({
            title: 'For All Mankind', year: null, season: 1, episode: null,
        });
        expect(parseReleaseName('For All Mankind Season 2 Complete 1080p')).toEqual({
            title: 'For All Mankind', year: null, season: 2, episode: null,
        });
    });
});
