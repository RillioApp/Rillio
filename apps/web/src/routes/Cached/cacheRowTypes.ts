// Copyright (C) 2017-2026 Smart code 203358507

// Types for what cacheRowModel.js (plain CommonJS, for jest) returns.

import type { CacheEntry } from './useCachedTorrents';
import type { TorrentFile } from './TorrentFiles';

export type ReleasePart = { text: string, strong?: boolean };

export type ViewState = {
    state: 'watched' | 'partial' | 'unwatched',
    offset?: number,
    duration?: number,
    source?: 'cache',
};

export type Described = {
    kind: 'pack' | 'episode' | 'movie' | 'title' | 'unidentified',
    title: string,
    subtitle: string | null,
    unmatched: boolean,
    season: number | null,
    episode: number | null,
    packLabel: string | null,
    release: ReleasePart[],
};

export type EpisodeRowModel = {
    label: string,
    title: string | null,
    videoId: string | null,
    file: TorrentFile,
    season: number | null,
    episode: number,
    lastEpisode: number,
    view: ViewState | null,
};

export type StripBar = {
    label: string,
    state: 'watched' | 'partial' | 'unwatched' | 'disk' | 'missing',
    fraction?: number,
};

export type Recency = { kind: 'played' | 'added', at: number, label?: string | null };

export type RowAction = { label: string, fileIdx: number, row: EpisodeRowModel | null };

export type CopyKey = {
    infoHash: string,
    kind: Described['kind'],
    metaId: string | null,
    season: number | null,
    episode: number | null,
    covers: [number, number][],
    label: string | null,
};

export type RowFacts = {
    entry: CacheEntry,
    described: Described,
    kind: Described['kind'],
    files: TorrentFile[] | null,
    other: TorrentFile[],
    rows: EpisodeRowModel[] | null,
    view: ViewState | null,
    recency: Recency,
    strip: StripBar[] | null,
    summary: string | null,
    disk: string,
    viewLine: string | null,
    watchedTone: boolean,
    action: RowAction | null,
    poster: string | null,
    fullName: string,
    details: string,
    copyKey: CopyKey,
    recencyAt: number,
};

/** RowFacts plus what the window adds: the copy note and the links. */
export type RowModel = RowFacts & {
    note: string | null,
    metaLink: string | null,
    streamsLink: string | null,
};
