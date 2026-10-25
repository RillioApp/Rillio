// What a release name says about the copy itself (resolution, source, HDR
// kind, codec, group) and a clean human name for torrents no title was
// matched to. The Cache window's release line and unidentified rows.

const { parseReleaseInfo, cleanName, releaseLine } = require('../src/common/releaseInfo');

describe('parseReleaseInfo', () => {
    const cases = [
        ['Severance.S02E03.Who.Is.Alive.2160p.ATVP.WEB-DL.DDP5.1.DV.HDR.H.265-FLUX.mkv',
            { resolution: '2160p', source: 'WEB-DL', service: 'ATVP', hdr: ['DV', 'HDR'], codec: 'H.265', group: 'FLUX' }],
        ['Severance.S02E03.1080p.WEB.H264-SuccessfulCrab.mkv',
            { resolution: '1080p', source: 'WEB', service: null, hdr: [], codec: 'H.264', group: 'SuccessfulCrab' }],
        ['Severance.S02.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb',
            { resolution: '1080p', source: 'WEB-DL', service: 'ATVP', hdr: [], codec: 'H.264', group: 'NTb' }],
        ['Severance (2022) Season 1 S01 (1080p ATVP WEB-DL x265 HEVC 10bit EAC3 Atmos 5.1 Ghost)',
            { resolution: '1080p', source: 'WEB-DL', service: 'ATVP', hdr: [], codec: 'H.265', group: 'Ghost' }],
        ['Dune.Part.Two.2024.2160p.UHD.BluRay.REMUX.DV.HDR10.HEVC.TrueHD.7.1.Atmos-FGT',
            { resolution: '2160p', source: 'BluRay REMUX', service: null, hdr: ['DV', 'HDR10'], codec: 'H.265', group: 'FGT' }],
        ['Oppenheimer.2023.1080p.BluRay.x264-SPARKS.mkv',
            { resolution: '1080p', source: 'BluRay', service: null, hdr: [], codec: 'H.264', group: 'SPARKS' }],
        ['The.Bear.S03E01.Tomorrow.720p.HDTV.x264-SYNCOPY[TGx]',
            { resolution: '720p', source: 'HDTV', service: null, hdr: [], codec: 'H.264', group: 'SYNCOPY' }],
        ['[SubsPlease] Frieren - 12 (1080p) [ABCD1234].mkv',
            { resolution: '1080p', source: null, service: null, hdr: [], codec: null, group: 'SubsPlease' }],
        ['www.Torrenting.com - Some.Indie.Thing.2019.WEBRip.x264-ION10',
            { resolution: null, source: 'WEBRip', service: null, hdr: [], codec: 'H.264', group: 'ION10' }],
        ['Download 3f9a1c2b',
            { resolution: null, source: null, service: null, hdr: [], codec: null, group: null }],
        ['Shogun.2024.S01E05.HDR10+.2160p.DSNP.WEB-DL.DDP5.1.Atmos.H.265-NTb.mkv',
            { resolution: '2160p', source: 'WEB-DL', service: 'DSNP', hdr: ['HDR10+'], codec: 'H.265', group: 'NTb' }],
        ['The Gentlemen (2024) (Season 1) (S01E01-E08) (1080p NF WEB-DL HDR-DV x265 HEVC) (Audio English-MULTiSubs) (elladajarek)',
            { resolution: '1080p', source: 'WEB-DL', service: 'NF', hdr: ['DV', 'HDR'], codec: 'H.265', group: 'elladajarek' }],
        ['Lanterns.S01E06.Bad.Optics.2160p.WEB-DL.DV.H.265-MeM.GP.mkv',
            { resolution: '2160p', source: 'WEB-DL', service: null, hdr: ['DV'], codec: 'H.265', group: 'MeM.GP' }],
        ['Some.Show.S01E01.4K.WEBRip.AV1.Opus-Group',
            { resolution: '2160p', source: 'WEBRip', service: null, hdr: [], codec: 'AV1', group: 'Group' }],
    ];
    cases.forEach(([name, expected]) => {
        test(name, () => {
            const info = parseReleaseInfo(name);
            expect({
                resolution: info.resolution,
                source: info.source,
                service: info.service,
                hdr: info.hdr,
                codec: info.codec,
                group: info.group,
            }).toEqual(expected);
        });
    });

    test('a "-DL" or "-Rip" tail is the source, never a group', () => {
        expect(parseReleaseInfo('Movie.2020.1080p.WEB-DL').group).toBeNull();
        expect(parseReleaseInfo('Movie.2020.720p.BD-Rip').group).toBeNull();
    });

    test('a two-letter word before the extension is not mistaken for a group', () => {
        expect(parseReleaseInfo('Some Home Movie.mp4').group).toBeNull();
    });
});

describe('the HDR label', () => {
    test('Dolby Vision alone is spelled out', () => {
        expect(parseReleaseInfo('X.2160p.DV.mkv').hdrLabel).toBe('Dolby Vision');
        expect(parseReleaseInfo('X.2160p.DoVi.mkv').hdrLabel).toBe('Dolby Vision');
    });
    test('Dolby Vision with an HDR10 fallback names both, DV first', () => {
        expect(parseReleaseInfo('X.2160p.HDR10.DV.mkv').hdrLabel).toBe('DV / HDR10');
    });
    test('HDR10+ is told apart from HDR10', () => {
        expect(parseReleaseInfo('X.2160p.HDR10+.mkv').hdrLabel).toBe('HDR10+');
        expect(parseReleaseInfo('X.2160p.HDR10Plus.mkv').hdrLabel).toBe('HDR10+');
        expect(parseReleaseInfo('X.2160p.HDR10.mkv').hdrLabel).toBe('HDR10');
    });
    test('no HDR is no label', () => {
        expect(parseReleaseInfo('X.1080p.mkv').hdrLabel).toBeNull();
    });
});

describe('releaseLine', () => {
    test('resolution first (emphasised), then service + source, HDR kind, codec only when useful, group', () => {
        expect(releaseLine(parseReleaseInfo('Severance.S02E03.Who.Is.Alive.2160p.ATVP.WEB-DL.DDP5.1.DV.HDR.H.265-FLUX.mkv'))).toEqual([
            { text: '2160p', strong: true },
            { text: 'ATVP WEB-DL' },
            { text: 'DV / HDR' },
            { text: 'FLUX' },
        ]);
    });
    test('the codec shows when it is what tells two copies apart (no HDR, no source)', () => {
        expect(releaseLine(parseReleaseInfo('Severance.S02E03.1080p.WEB.H264-SuccessfulCrab.mkv'))).toEqual([
            { text: '1080p', strong: true },
            { text: 'WEB' },
            { text: 'H.264' },
            { text: 'SuccessfulCrab' },
        ]);
    });
    test('nothing known is an empty line', () => {
        expect(releaseLine(parseReleaseInfo('Download 3f9a1c2b'))).toEqual([]);
    });
});

describe('cleanName', () => {
    test('a site prefix is dropped and dots become spaces', () => {
        expect(cleanName('www.Torrenting.com - Some.Indie.Thing.2019.WEBRip.x264-ION10.mp4')).toEqual({
            title: 'Some Indie Thing', year: '2019', season: null, episode: null,
        });
    });
    test('a leading [group] is dropped and an absolute episode number is read', () => {
        expect(cleanName('[SubsPlease] Sousou no Frieren - 12 (1080p) [5E0A1C2B].mkv')).toEqual({
            title: 'Sousou no Frieren', year: null, season: null, episode: 12,
        });
    });
    test('a scene episode keeps its season and episode', () => {
        expect(cleanName('The.Bear.S03E01.Tomorrow.720p.HDTV.x264-SYNCOPY[TGx]')).toEqual({
            title: 'The Bear', year: null, season: 3, episode: 1,
        });
    });
    test('a bracketed site prefix is dropped too', () => {
        expect(cleanName('[ www.Speed.cd ] - Oppenheimer.2023.1080p.BluRay.x264-SPARKS').title).toBe('Oppenheimer');
    });
    test('a name with nothing to clean stays itself', () => {
        expect(cleanName('Download 3f9a1c2b')).toEqual({ title: 'Download 3f9a1c2b', year: null, season: null, episode: null });
    });
});
