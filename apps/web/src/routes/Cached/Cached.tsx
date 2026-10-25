import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Play, Info, Trash2, X, Search, FolderOpen } from 'lucide-react';
import { useNavigate } from 'react-router';
import { toPath } from 'rillio-router';
import { useCore } from 'rillio/core';
import { useProfile } from 'rillio/common';
import useToast from 'rillio/common/Toast/useToast';
import { useTauriApi } from 'rillio/common/Platform/shell/isShell';
import { fetchStreamingModeEnabled, postStreamingModeEnabled } from 'rillio/common/streamingMode';
import { playerDeepLink, type CacheMeta } from 'rillio/common/cacheMetadata';
import { Button, IconButton, ModalRoute, Switch, cn } from 'rillio/components/ui';
import AnimatedPercentage from 'rillio/components/ui/animated-percentage';
import useCachedTorrents, { CacheEntry } from './useCachedTorrents';
import TorrentFiles from './TorrentFiles';
import CacheRow from './CacheRow';
import useMetadataMatcher from './useMetadataMatcher';
import { useFileLists, useLibraryMap, useTitleVideos } from './useCacheDetails';
import type { EpisodeRowModel, RowFacts, RowModel } from './cacheRowTypes';
import UpdateFailureNotice from 'rillio/routes/Settings/General/UpdateFailureNotice';

// Every line a row or the hero shows is decided in cacheRowModel (pure,
// jest-covered); this file wires data and actions to it.
const { rowFacts, copyNotes, etaText, formatSize, formatOf, relativeTime } = require('./cacheRowModel') as {
    rowFacts: (args: object) => RowFacts,
    copyNotes: (rows: RowFacts['copyKey'][]) => Record<string, string>,
    etaText: (seconds: number) => string | null,
    formatSize: (bytes: number) => string,
    formatOf: (done: number, total: number) => string,
    relativeTime: (now: number, at: number) => string,
};
const { videoKey, stillOpen } = require('./cacheDetailsState') as {
    videoKey: (meta: CacheEntry['meta']) => string | null,
    stillOpen: (openCard: string | null, entries: CacheEntry[] | null) => string | null,
};
const { parseReleaseInfo } = require('rillio/common/releaseInfo') as {
    parseReleaseInfo: (name: string) => { resolution: string | null, source: string | null, service: string | null, hdrLabel: string | null, group: string | null },
};

// Identified entries can deep link to their title directly, no continue-watching
// lookup needed.
const metaDetailsLink = (entry: CacheEntry): string | null =>
    entry.meta !== undefined ?
        `#/metadetails/${encodeURIComponent(entry.meta.type)}/${encodeURIComponent(entry.meta.metaId)}`
        :
        null;

// "Find another source": the title's own streams page (the episode's, for a
// series), where every addon's sources for it are listed. Only for an entry
// that has been identified; an unidentified one has no title to look up.
const streamsPageLink = (entry: CacheEntry): string | null => {
    const meta = entry.meta;
    if (meta === undefined) return null;
    const video = typeof meta.videoId === 'string' && meta.videoId.length > 0 ?
        `/${encodeURIComponent(meta.videoId)}`
        :
        '';
    return `#/metadetails/${encodeURIComponent(meta.type)}/${encodeURIComponent(meta.metaId)}${video}`;
};

// Same parser the stream cards use, for the toolbar's quality filters and sort.
const { parseStream } = require('rillio/routes/MetaDetails/StreamsList/streamQuality');

// The toolbar's decision logic (sort orders, filter facets, search), pure and
// jest-covered in cacheListOps.
const { SORTS, FILTERS, sortEntries, filterEntries } = require('./cacheListOps');

const SORT_LABELS: Record<string, string> = {
    date: 'Recent',
    size: 'Largest',
    quality: 'Quality',
};

// Short labels: the whole toolbar is ONE quiet row. Active = an unfinished
// transfer, Done = complete on disk.
const FILTER_LABELS: Record<string, string> = {
    '4k': '4K',
    '1080p': '1080p',
    hdr: 'HDR',
    downloading: 'Active',
    complete: 'Done',
    kept: 'Kept',
};

type ParsedQuality = {
    resolution: number,
    hdr: boolean,
    flags: string[],
};

// Best-effort infoHash -> library deep links, mined from continue watching
// once per mount: each item there carries the exact stream it was played with,
// encoded (base64 of zlib JSON) as the first segment of deepLinks.player. That
// encoded stream is the only client-side bridge from a torrent infoHash back
// to a meta id. We keep both the MetaDetails link (the "more info" button) and
// the FULL player link (stream + transport urls + type/id/videoId), so playing
// from here restores the title's loading screen and records library progress.
// Cached torrents never played from a library title simply get neither.
type LibraryLinks = {
    metaLink: string | null,
    playerLink: string,
    // Mined alongside the links for the download hero: the clean library title
    // (the entry name is a scene-tagged FILENAME) and the meta id, which for
    // IMDb ids resolves artwork (backdrop + title logo) via metahub - the same
    // CDN the rest of the catalog UI already loads art from.
    itemName: string | null,
    metaId: string | null,
};

// metahub artwork for an IMDb id; other id spaces (kitsu etc.) have no CDN
// here and fall back to the text title / plain gradient.
const metahubBackground = (metaId: string | null): string | null =>
    metaId !== null && /^tt\d+$/.test(metaId) ? `https://images.metahub.space/background/medium/${metaId}/img` : null;
const metahubLogo = (metaId: string | null): string | null =>
    metaId !== null && /^tt\d+$/.test(metaId) ? `https://images.metahub.space/logo/medium/${metaId}/img` : null;

const useLibraryLinksByInfoHash = (): Record<string, LibraryLinks> => {
    const core = useCore();
    const [links, setLinks] = useState<Record<string, LibraryLinks>>({});
    useEffect(() => {
        let cancelled = false;
        core.transport.getState('continue_watching_preview')
            .then(async (state) => {
                const { items } = state as {
                    items?: {
                        deepLinks?: {
                            player?: string | null,
                            metaDetailsStreams?: string | null,
                            metaDetailsVideos?: string | null,
                        },
                    }[],
                };
                const pairs = await Promise.all((items ?? []).map(async (item) => {
                    const playerLink = item.deepLinks?.player;
                    const metaLink = item.deepLinks?.metaDetailsStreams ?? item.deepLinks?.metaDetailsVideos;
                    if (typeof playerLink !== 'string') return null;
                    // '#/player/{encodedStream}/...' - segment 2 of the path is the stream.
                    const encoded = toPath(playerLink).split('/')[2];
                    if (typeof encoded !== 'string' || encoded.length === 0) return null;
                    const stream = await core.transport.decodeStream(decodeURIComponent(encoded)) as Stream | null;
                    // '/metadetails/{type}/{id}/...' - segment 3 is the meta id.
                    const rawId = typeof metaLink === 'string' ? toPath(metaLink).split('/')[3] : undefined;
                    const itemName = (item as { name?: unknown }).name;
                    return stream !== null && typeof stream.infoHash === 'string' ?
                        [stream.infoHash, {
                            metaLink: typeof metaLink === 'string' ? metaLink : null,
                            playerLink,
                            itemName: typeof itemName === 'string' ? itemName : null,
                            metaId: typeof rawId === 'string' && rawId.length > 0 ? decodeURIComponent(rawId) : null,
                        }] as [string, LibraryLinks]
                        :
                        null;
                }));
                if (!cancelled) {
                    setLinks(pairs.reduce<Record<string, LibraryLinks>>((result, pair) => {
                        if (pair !== null) result[pair[0]] = pair[1];
                        return result;
                    }, {}));
                }
            })
            .catch((error) => console.error('Cached: mapping torrents to library items failed', error));
        return () => { cancelled = true; };
    }, []);
    return links;
};

// The server-side streaming-mode toggle (auto-clean watched streams), read
// once per mount and posted optimistically on change. `null` while unknown
// (loading, or no reachable server) - the switch hides and the copy stays
// neutral rather than claiming a state we have not confirmed.
const useStreamingModeSetting = () => {
    const profile = useProfile();
    const serverUrl = profile.settings.streamingServerUrl;
    const [enabled, setEnabled] = useState<boolean | null>(null);
    useEffect(() => {
        if (typeof serverUrl !== 'string') return;
        let cancelled = false;
        fetchStreamingModeEnabled(serverUrl)
            .then((value) => { if (!cancelled) setEnabled(value); })
            .catch(() => { /* stays null: no switch, neutral copy */ });
        return () => { cancelled = true; };
    }, [serverUrl]);
    const toggle = useCallback((next: boolean) => {
        if (typeof serverUrl !== 'string') return;
        setEnabled(next);
        postStreamingModeEnabled(serverUrl, next)
            .catch((error) => {
                console.error('Cached: persisting streaming mode failed', error);
                setEnabled(!next);
            });
    }, [serverUrl]);
    return { enabled, toggle };
};

// Download speed for the hero: the byte delta between cache polls (~3s apart).
// A poll that brings no new bytes is an honest 0.
const useDownloadSpeed = (entry: CacheEntry | null): number => {
    const [speed, setSpeed] = useState(0);
    const lastRef = useRef<{ hash: string, t: number, bytes: number } | null>(null);
    useEffect(() => {
        if (entry === null) {
            lastRef.current = null;
            setSpeed(0);
            return;
        }
        const now = performance.now();
        const last = lastRef.current;
        if (last === null || last.hash !== entry.infoHash) {
            lastRef.current = { hash: entry.infoHash, t: now, bytes: entry.downloaded };
            setSpeed(0);
            return;
        }
        const dt = (now - last.t) / 1000;
        // The poll interval is 3s; anything much faster is a state echo (a
        // mutation re-poll), not a fresh sample.
        if (dt < 1) return;
        setSpeed(Math.max(0, (entry.downloaded - last.bytes) / dt));
        lastRef.current = { hash: entry.infoHash, t: now, bytes: entry.downloaded };
    }, [entry]);
    return speed;
};

// Buttons over the hero's artwork: the house surface tint plus a blur, so
// they read on any backdrop.
const HERO_BTN = 'h-10 shrink-0 rounded-full bg-surface-hover px-4 text-[0.8125rem] font-bold text-fg backdrop-blur-md hover:brightness-110';
const HERO_ICON = 'size-10 shrink-0 rounded-full bg-surface-hover text-fg backdrop-blur-md hover:brightness-110';
const HERO_PILL = 'inline-flex h-5 shrink-0 items-center rounded-full px-2 text-[0.6875rem] font-extrabold leading-none tracking-wide';

/**
 * The download hero: the active download presented like a title, not a row.
 * The title logo (or clean name) and, under it ALWAYS, what exactly this is
 * (the episode and its title, or the pack) and which copy (resolution, HDR
 * kind, source, group): two downloads of one show used to look identical here
 * apart from the percentage, because the logo replaced the whole text line.
 */
const HeroDownload = ({ row, links, streamingMode, onPlay, onPlayEpisode, onPlayFile, onMoreInfo, onSetPaused, onSetPinned, onSetFileSelected, onDelete, onReveal }: {
    row: RowModel,
    links: LibraryLinks | undefined,
    streamingMode: boolean | null,
    onPlay: (entry: CacheEntry) => void,
    onPlayEpisode: (entry: CacheEntry, episode: EpisodeRowModel) => void,
    onPlayFile: (infoHash: string, fileIdx: number, name: string) => void,
    onMoreInfo: (link: string) => void,
    onSetPaused: (infoHash: string, paused: boolean) => void,
    onSetPinned: (infoHash: string, pinned: boolean) => void,
    onSetFileSelected: (infoHash: string, fileIdx: number, selected: boolean) => Promise<void> | void,
    onDelete: (infoHash: string) => void,
    onReveal: ((infoHash: string, fileIdx?: number) => void) | null,
}) => {
    const { entry, described } = row;
    const speed = useDownloadSpeed(entry);
    const [filesOpen, setFilesOpen] = useState(false);
    const paused = entry.state === 'paused';
    const progress = entry.total > 0 ? Math.min(1, entry.downloaded / entry.total) : 0;
    const pct = Math.floor(progress * 100);
    const eta = !paused && speed > 0 ? etaText((entry.total - entry.downloaded) / speed) : null;
    const release = useMemo(() => parseReleaseInfo(entry.name), [entry.name]);

    // The matched metadata is the best source (it carries the addon's own
    // artwork urls and works for any identified torrent); the continue-watching
    // bridge + metahub stay as the fallback for anything not yet identified.
    const metaId = entry.meta?.metaId ?? links?.metaId ?? null;
    const title = entry.meta !== undefined ? described.title : (links?.itemName ?? described.title);
    const backgroundUrl = entry.meta?.background ?? metahubBackground(metaId);
    const logoUrl = entry.meta?.logo ?? metahubLogo(metaId);
    const [backgroundFailed, setBackgroundFailed] = useState(false);
    const [logoFailed, setLogoFailed] = useState(false);
    useEffect(() => { setBackgroundFailed(false); setLogoFailed(false); }, [metaId]);

    // "S02E03 · Who Is Alive?": the tag strong, the episode title plain.
    const [tag, ...rest] = (described.subtitle ?? '').split(' · ');
    const stats = [
        entry.total > 0 ? formatOf(entry.downloaded, entry.total) : null,
        paused ? 'Paused' : `${formatSize(speed)}/s`,
        eta,
        typeof entry.addedAt === 'number' && entry.addedAt > 0 ? `Added ${relativeTime(Date.now(), entry.addedAt)}` : null,
    ].filter(Boolean).join(' · ');
    const action = row.action;

    return (
        <div className="relative mx-6 mb-2 mt-1 overflow-hidden rounded-[1.125rem] bg-surface">
            {
                backgroundUrl !== null && !backgroundFailed ?
                    <img
                        src={backgroundUrl}
                        alt=""
                        onError={() => setBackgroundFailed(true)}
                        className="absolute inset-0 size-full object-cover object-[50%_20%]"
                    />
                    :
                    null
            }
            {/* Light at the top, where the title logo sits on the artwork; dark under the text. */}
            <div className="absolute inset-0 bg-gradient-to-b from-bg/10 via-bg/55 to-bg/90" />

            <div className="relative flex flex-col gap-3.5 px-6 py-5">
                <div className="flex flex-col gap-1.5">
                    {
                        logoUrl !== null && !logoFailed ?
                            <img
                                src={logoUrl}
                                alt={title}
                                onError={() => setLogoFailed(true)}
                                className="max-h-14 max-w-[55%] self-start object-contain object-left drop-shadow-[0_2px_12px_rgba(0,0,0,0.9)]"
                            />
                            :
                            <div className="truncate text-2xl font-extrabold text-fg" title={entry.name}>{title}</div>
                    }
                    {/* Always there, logo or not: what exactly this is, and which copy. */}
                    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-sm text-fg">
                        {tag ? <span className="text-sm font-bold">{tag}</span> : null}
                        {rest.length > 0 ? <><span className="text-sm text-fg-subtle">·</span><span className="text-sm">{rest.join(' · ')}</span></> : null}
                        {described.unmatched ? <span className="rounded-full bg-surface px-2 py-0.5 text-[0.6875rem] font-bold text-fg-subtle">Not matched to a title</span> : null}
                        {release.resolution ? <span className={cn(HERO_PILL, 'bg-accent text-bg')}>{release.resolution}</span> : null}
                        {release.hdrLabel ? <span className={cn(HERO_PILL, 'bg-surface-hover uppercase text-fg')}>{release.hdrLabel}</span> : null}
                        {release.source ? <span className={cn(HERO_PILL, 'bg-surface-hover uppercase text-fg')}>{[release.service, release.source].filter(Boolean).join(' ')}</span> : null}
                        {release.group ? <span className="text-sm font-semibold text-fg">{release.group}</span> : null}
                        {row.note !== null ? <span className="rounded-full bg-surface-hover px-2 py-0.5 text-[0.6875rem] font-bold text-fg-muted">{row.note}</span> : null}
                    </div>
                </div>

                <div className="flex flex-wrap items-end justify-between gap-4">
                    <div className="flex min-w-[18rem] flex-1 flex-col gap-1.5">
                        <div className="flex items-baseline gap-2.5">
                            <span className="text-[0.6875rem] font-extrabold uppercase tracking-widest text-fg-muted">{paused ? 'Paused' : 'Downloading'}</span>
                            <span className="text-[1.875rem] font-extrabold leading-none text-fg"><AnimatedPercentage text={`${pct}%`} /></span>
                        </div>
                        <div className="h-1 w-full overflow-hidden rounded-full bg-surface-hover">
                            <div className="h-full rounded-full bg-accent transition-[width] duration-700" style={{ width: `${Math.round(progress * 100)}%` }} />
                        </div>
                        <span className="text-[0.8125rem] tabular-nums text-fg-muted">{stats}</span>
                    </div>
                    <div className="flex flex-wrap items-center gap-2">
                        {
                            // Streaming DURING the download is the point of a streaming
                            // server, so Play is offered as soon as there is a file to
                            // play.
                            action !== null ?
                                <Button onClick={() => (action.row !== null ? onPlayEpisode(entry, action.row) : onPlay(entry))} className="h-10 px-[1.125rem] text-[0.8125rem] font-bold" title="Play (streams while it downloads)">
                                    <Play className="size-4" />{action.label}
                                </Button>
                                :
                                null
                        }
                        <Button variant="ghost" className={HERO_BTN} onClick={() => onSetPaused(entry.infoHash, !paused)} title={paused ? 'Resume this download' : 'Pause this download'}>
                            {/* "Resume download", not "Resume": Play beside it may read "Resume" (playback). */}
                            {paused ? 'Resume download' : 'Pause'}
                        </Button>
                        {
                            // Keep only matters while auto-clean can delete things.
                            streamingMode === true ?
                                <Button
                                    variant="ghost"
                                    className={cn(HERO_BTN, entry.pinned && 'bg-accent-soft text-accent')}
                                    onClick={() => onSetPinned(entry.infoHash, !entry.pinned)}
                                    title={entry.pinned ? 'Kept: never cleaned up automatically. Click to allow cleanup' : 'Keep this title in the cache (never cleaned up automatically)'}
                                >
                                    {entry.pinned ? 'Kept' : 'Keep'}
                                </Button>
                                :
                                null
                        }
                        <Button variant="ghost" className={cn(HERO_BTN, filesOpen && 'bg-accent-soft text-accent')} onClick={() => setFilesOpen((open) => !open)} title="Show the files in this torrent">
                            Files
                        </Button>
                        {
                            onReveal !== null ?
                                <IconButton onClick={() => onReveal(entry.infoHash)} title="View location: show this download in its folder" className={HERO_ICON}>
                                    <FolderOpen className="size-4" />
                                </IconButton>
                                :
                                null
                        }
                        {
                            row.metaLink !== null ?
                                <IconButton onClick={() => onMoreInfo(row.metaLink as string)} title="More info" className={HERO_ICON}>
                                    <Info className="size-4" />
                                </IconButton>
                                :
                                null
                        }
                        <IconButton onClick={() => onDelete(entry.infoHash)} title="Delete from cache (frees disk space; can be re-downloaded)" className={cn(HERO_ICON, 'hover:text-danger')}>
                            <Trash2 className="size-4" />
                        </IconButton>
                    </div>
                </div>
            </div>
            {
                // A pack's episodes each get their own Play here, which is how you
                // start watching one before the whole thing has finished.
                filesOpen ?
                    <div className="relative">
                        <TorrentFiles
                            infoHash={entry.infoHash}
                            initializing={entry.state === 'initializing'}
                            onPlayFile={onPlayFile}
                            onSetFileSelected={onSetFileSelected}
                            onReveal={onReveal}
                        />
                    </div>
                    :
                    null
            }
        </div>
    );
};

// The Cached page: everything the streaming engine holds on disk, with live
// download progress, pin (eviction protection) and delete. The user's window
// into "what is eating my disk" - and the place the disk-full error can send
// them to free space.
type Props = {
    onClose: () => void,
};

const Cached = ({ onClose }: Props) => {
    const closeCached = onClose;
    const navigate = useNavigate();
    const core = useCore();
    const toast = useToast();
    const profile = useProfile();
    const { entries, failed, error, refresh, setPinned, setPaused, setFileSelected, remove, refetch, refetching } = useCachedTorrents();
    // The load failure in the shape the shared failure notice takes: plain
    // words on screen, the technical text only in what "Copy error" copies.
    // Keyed on the error text so the 3s re-check does not reset the notice.
    const loadFailure = useMemo(() => ({
        stage: 'cache-list',
        kind: 'unreachable',
        summary: 'Rillio could not reach its download service.',
        hint: 'It keeps trying on its own, and this list comes back as soon as the service answers.',
        message: error ?? '',
        report: `Rillio: the cache list could not be loaded\n${error ?? '(no detail)'}`,
    }), [error]);
    const libraryLinks = useLibraryLinksByInfoHash();
    const streamingMode = useStreamingModeSetting();
    // Identify anything the Player never got to write metadata for.
    useMetadataMatcher(entries, refresh);
    const library = useLibraryMap();
    const videosByKey = useTitleVideos(entries);
    const { lists: filesByHash, reload: reloadFiles } = useFileLists(entries, profile.settings.streamingServerUrl);
    // The addon each title was streamed from, by transport url ("From Torrentio").
    const addonNames = useMemo(() => {
        const addons = (profile as unknown as { addons?: { transportUrl?: string, manifest?: { name?: string } }[] }).addons ?? [];
        return addons.reduce<Record<string, string>>((names, addon) => {
            if (typeof addon.transportUrl === 'string' && typeof addon.manifest?.name === 'string') names[addon.transportUrl] = addon.manifest.name;
            return names;
        }, {});
    }, [profile]);

    // "View location": the shell resolves the path from the engine itself
    // (reveal_cache_item); only where there is a file manager to show.
    const tauri = useTauriApi();
    const canReveal = tauri !== undefined && !/android/i.test(navigator.userAgent);
    const reveal = useCallback((infoHash: string, fileIdx?: number) => {
        tauri.core.invoke('reveal_cache_item', { infoHash, fileIdx: typeof fileIdx === 'number' ? fileIdx : null })
            .catch((reason: unknown) => {
                console.error('Cached: reveal_cache_item failed', infoHash, fileIdx, reason);
                toast.show({ type: 'error', title: 'Could not show the folder', message: typeof reason === 'string' ? reason : 'Rillio could not open the folder.' });
            });
    }, [tauri, toast]);

    // Prefer the FULL player deep link mined from continue watching (stream +
    // transport urls + type/id/videoId): it restores the title logo on the
    // loading screen and records progress into the library. When the torrent
    // is unknown to the library, fall back to a stream-only link: the core
    // encodes the stream (base64 of zlib JSON) and the /player/:stream route
    // decodes it back, loading the torrent through the local streaming server.
    // Navigating to a real route (player / metadetails) from inside the modal
    // closes the modal via the bus first, then navigates.
    const play = useCallback((entry: CacheEntry) => {
        const playerLink = libraryLinks[entry.infoHash]?.playerLink;
        if (typeof playerLink === 'string') {
            closeCached();
            navigate(toPath(playerLink));
            return;
        }
        core.transport.encodeStream({
            name: entry.name,
            description: '',
            infoHash: entry.infoHash,
            fileIdx: entry.fileIdx,
        })
            .then((encoded) => {
                if (typeof encoded === 'string') {
                    closeCached();
                    // With saved metadata this is the SAME deep link a catalog
                    // play produces (meta + library progress + next episode);
                    // without it, the bare stream as before.
                    navigate(playerDeepLink(encoded, entry.meta));
                } else {
                    console.error('Cached: the core could not encode a stream for', entry.infoHash);
                }
            })
            .catch((error) => console.error('Cached: encoding the stream failed', error));
    }, [navigate, libraryLinks, closeCached]);

    // Play one specific file out of the torrent (the file lists). Always the
    // stream-only path: the library deep link points at the title's own file,
    // which is precisely not what was clicked here.
    const playFile = useCallback((infoHash: string, fileIdx: number, name: string) => {
        core.transport.encodeStream({ name, description: '', infoHash, fileIdx })
            .then((encoded) => {
                if (typeof encoded === 'string') {
                    closeCached();
                    navigate(`/player/${encodeURIComponent(encoded)}`);
                } else {
                    console.error('Cached: the core could not encode a stream for', infoHash, fileIdx);
                }
            })
            .catch((error) => console.error('Cached: encoding the file stream failed', error));
    }, [navigate, closeCached]);

    // Play one episode of a pack as THAT episode: the deep link carries its
    // video id, so the player shows the right title and records progress on
    // the right episode. An episode the title's list does not hold (or an
    // unidentified pack) plays as a plain file.
    const playEpisode = useCallback((entry: CacheEntry, episode: EpisodeRowModel) => {
        if (entry.meta === undefined || episode.videoId === null) {
            playFile(entry.infoHash, episode.file.index, episode.file.path);
            return;
        }
        const meta: CacheMeta = {
            ...entry.meta,
            videoId: episode.videoId,
            season: episode.season ?? 1,
            episode: episode.episode,
            fileIdx: episode.file.index,
        };
        core.transport.encodeStream({ name: episode.file.path, description: '', infoHash: entry.infoHash, fileIdx: episode.file.index })
            .then((encoded) => {
                if (typeof encoded === 'string') {
                    closeCached();
                    navigate(playerDeepLink(encoded, meta));
                } else {
                    console.error('Cached: the core could not encode a stream for', entry.infoHash, episode.file.index);
                }
            })
            .catch((error) => console.error('Cached: encoding the episode stream failed', error));
    }, [navigate, closeCached, playFile]);

    const openMetaDetails = useCallback((link: string) => {
        closeCached();
        navigate(toPath(link));
    }, [navigate, closeCached]);

    const totalBytes = useMemo(
        () => (entries ?? []).reduce((sum, entry) => sum + entry.downloaded, 0),
        [entries],
    );

    // Toolbar state. "Recent" is the default order; filters are toggle chips
    // that AND together; the search box matches titles and filenames.
    const [sort, setSort] = useState<string>('date');
    const [filters, setFilters] = useState<string[]>([]);
    const [query, setQuery] = useState('');
    const toggleFilter = useCallback((filter: string) => {
        setFilters((current) => current.includes(filter) ?
            current.filter((f) => f !== filter)
            :
            [...current, filter]);
    }, []);

    // parseStream tokenizes a release name on every call; entries re-arrive
    // every 3s poll with the same names, so cache by name for the session.
    const qualityCache = useRef(new Map<string, ParsedQuality>());
    const getQuality = useCallback((entry: CacheEntry): ParsedQuality => {
        const cached = qualityCache.current.get(entry.name);
        if (cached !== undefined) return cached;
        const parsed: ParsedQuality = parseStream({ name: entry.name, description: '' });
        qualityCache.current.set(entry.name, parsed);
        return parsed;
    }, []);

    // What every row says, decided once per change of anything it reads.
    const facts = useMemo(() => {
        const now = Date.now();
        const map = new Map<string, RowFacts>();
        (entries ?? []).forEach((entry) => {
            map.set(entry.infoHash, rowFacts({
                entry,
                files: filesByHash[entry.infoHash] ?? null,
                // By the addon the list came from, not the bare meta id.
                videos: (() => { const key = videoKey(entry.meta); return key !== null ? videosByKey[key] : undefined; })(),
                library: entry.meta !== undefined ? library[entry.meta.metaId] : undefined,
                addonNames,
                now,
            }));
        });
        return map;
    }, [entries, filesByHash, videosByKey, library, addonNames]);
    const searchName = useCallback((entry: CacheEntry): string => {
        const described = facts.get(entry.infoHash)?.described;
        return described !== undefined ? `${described.title} ${described.subtitle ?? ''}` : entry.name;
    }, [facts]);
    const getRecency = useCallback((entry: CacheEntry): number => facts.get(entry.infoHash)?.recencyAt ?? entry.addedAt ?? 0, [facts]);

    // What the list actually shows: filters + search, then the chosen order.
    const visible = useMemo(
        () => sortEntries(
            filterEntries(entries ?? [], filters, query, getQuality, searchName),
            sort,
            getQuality,
            getRecency,
        ) as CacheEntry[],
        [entries, filters, query, sort, getQuality, searchName, getRecency],
    );

    // The hero: the first ACTIVE download (live or paused mid-transfer - paused
    // stays a hero so pausing does not bounce the layout). Initializing entries
    // have no honest byte counts yet and errors are rows, not heroes. Picked
    // from the FILTERED list so the toolbar applies to it like any row.
    const heroEntry = useMemo(
        () => visible.find((entry) =>
            // A download with an issue is a row with a tag, not the hero.
            entry.issue === undefined &&
            entry.total > 0 && entry.downloaded < entry.total &&
            (entry.state === 'live' || entry.state === 'paused')) ?? null,
        [visible],
    );

    // The rows in display order (hero first), with the copy notes that need
    // that order ("Another copy of the download above").
    const models = useMemo((): RowModel[] => {
        const ordered = heroEntry !== null ? [heroEntry, ...visible.filter((entry) => entry !== heroEntry)] : visible;
        const rowFactsList = ordered.map((entry) => facts.get(entry.infoHash)).filter((row): row is RowFacts => row !== undefined);
        const notes = copyNotes(rowFactsList.map((row) => row.copyKey));
        return rowFactsList.map((row) => ({
            ...row,
            note: notes[row.entry.infoHash] ?? null,
            metaLink: metaDetailsLink(row.entry) ?? libraryLinks[row.entry.infoHash]?.metaLink ?? null,
            streamsLink: streamsPageLink(row.entry),
        }));
    }, [visible, heroEntry, facts, libraryLinks]);
    const heroModel = heroEntry !== null ? models[0] ?? null : null;
    // The one open card, for this visit only (the window unmounts on close).
    const [openCard, setOpenCard] = useState<string | null>(null);
    const toggleCard = useCallback((infoHash: string) => {
        setOpenCard((current) => (current === infoHash ? null : infoHash));
    }, []);
    // A deleted download closes its card, so a re-add of the same hash starts closed.
    useEffect(() => {
        setOpenCard((current) => stillOpen(current, entries));
    }, [entries]);
    const rowModels = heroModel !== null ? models.slice(1) : models;

    return (
        <ModalRoute
            open
            onClose={closeCached}
            title="Cache"
            hideHeader
            showClose={false}
            className="flex h-[min(46rem,calc(100vh-6rem))] w-[min(62rem,calc(100vw-4rem))] max-w-[min(62rem,calc(100vw-4rem))] flex-col gap-0 overflow-hidden border border-line p-0"
        >
            <div className="flex items-start gap-3 px-7 pb-3.5 pt-6">
                <div className="min-w-0">
                    <div className="flex items-baseline gap-2.5">
                        <h1 className="text-[1.375rem] font-extrabold text-fg">Cache</h1>
                        {
                            entries !== null && entries.length > 0 ?
                                <div className="text-sm tabular-nums text-fg-muted">{formatSize(totalBytes)} on disk</div>
                                :
                                null
                        }
                    </div>
                    <div className="mt-1 text-[0.8125rem] text-fg-subtle">
                        {
                            streamingMode.enabled === true ?
                                'Streams are cleaned up a while after you watch them. Kept titles stay until you delete them.'
                                :
                                'Everything the player keeps on disk. Nothing is deleted automatically.'
                        }
                    </div>
                </div>
                {
                    // Streaming mode lives right where its effect is visible. Hidden
                    // until the server confirms a state (no reachable server = no
                    // switch to flip into the void).
                    streamingMode.enabled !== null ?
                        <label className="ml-auto flex shrink-0 cursor-pointer items-center gap-2.5 pt-1">
                            <span className="text-xs font-medium text-fg-muted">Auto-clean watched</span>
                            <Switch
                                checked={streamingMode.enabled}
                                onCheckedChange={streamingMode.toggle}
                            />
                        </label>
                        :
                        null
                }
                <IconButton
                    onClick={closeCached}
                    title="Close"
                    className={cn('size-10 text-fg-muted', streamingMode.enabled === null && 'ml-auto')}
                >
                    <X className="size-5" />
                </IconButton>
            </div>
            {
                // The toolbar: ONE quiet row - a bare inline search, the filter
                // facets as text chips (accent when active), and the sort behind
                // a thin divider.
                entries !== null && entries.length > 0 ?
                    <div className="flex min-h-10 shrink-0 flex-wrap items-center gap-x-2 gap-y-2 px-7 pb-3.5">
                        <Search className="size-3.5 shrink-0 text-fg-subtle" />
                        <input
                            type="text"
                            value={query}
                            onChange={(event) => setQuery(event.target.value)}
                            placeholder="Search the cache"
                            className="min-w-32 flex-1 bg-transparent text-[0.8125rem] text-fg outline-none placeholder:text-fg-subtle"
                        />
                        {
                            query.length > 0 ?
                                <IconButton onClick={() => setQuery('')} title="Clear search" className="size-5 shrink-0 text-fg-subtle hover:text-fg">
                                    <X className="size-3.5" />
                                </IconButton>
                                :
                                null
                        }
                        <div className="flex shrink-0 items-center gap-1">
                            {
                                (FILTERS as string[]).map((filter) => {
                                    const active = filters.includes(filter);
                                    return (
                                        <button
                                            key={filter}
                                            type="button"
                                            onClick={() => toggleFilter(filter)}
                                            className={cn(
                                                'h-7 rounded-full px-3 text-xs font-bold transition',
                                                active ?
                                                    'bg-accent-soft text-accent'
                                                    :
                                                    'text-fg-muted hover:bg-surface hover:text-fg',
                                            )}
                                        >
                                            {FILTER_LABELS[filter]}
                                        </button>
                                    );
                                })
                            }
                        </div>
                        <div className="h-4 w-px shrink-0 bg-line" />
                        <div className="flex shrink-0 items-center gap-1">
                            {
                                (SORTS as string[]).map((value) => (
                                    <button
                                        key={value}
                                        type="button"
                                        onClick={() => setSort(value)}
                                        className={cn(
                                            'h-7 rounded-full px-3 text-xs font-bold transition',
                                            sort === value ?
                                                'bg-accent-soft text-accent'
                                                :
                                                'text-fg-muted hover:bg-surface hover:text-fg',
                                        )}
                                    >
                                        {SORT_LABELS[value]}
                                    </button>
                                ))
                            }
                        </div>
                    </div>
                    :
                    null
            }
            <div className="min-h-0 flex-1 overflow-y-auto pb-4">
                {
                    failed ?
                        // Same treatment as a failed update (UpdateFailureNotice):
                        // what happened in plain words, "Try again", and "Copy
                        // error" for the technical text.
                        <div className="px-6 py-10">
                            <UpdateFailureNotice title="The cache could not be loaded" failure={loadFailure} onRetry={refresh} />
                        </div>
                        :
                        entries === null ?
                            <div className="px-5 py-10 text-center text-sm text-fg-muted">Loading…</div>
                            :
                            entries.length === 0 ?
                                <div className="px-5 py-10 text-center text-sm text-fg-muted">
                                    Nothing cached yet. Streams are kept here while you watch, and the Download button on any source stores it for later.
                                </div>
                                :
                                visible.length === 0 ?
                                    <div className="px-5 py-10 text-center text-sm text-fg-muted">
                                        Nothing here matches. Loosen the filters or clear the search.
                                    </div>
                                    :
                                    <>
                                        {
                                            heroModel !== null ?
                                                <HeroDownload
                                                    row={heroModel}
                                                    links={libraryLinks[heroModel.entry.infoHash]}
                                                    streamingMode={streamingMode.enabled}
                                                    onPlay={play}
                                                    onPlayEpisode={playEpisode}
                                                    onPlayFile={playFile}
                                                    onMoreInfo={openMetaDetails}
                                                    onSetPaused={setPaused}
                                                    onSetPinned={setPinned}
                                                    onSetFileSelected={setFileSelected}
                                                    onDelete={remove}
                                                    onReveal={canReveal ? reveal : null}
                                                />
                                                :
                                                null
                                        }
                                        <div className="flex flex-col gap-2 px-4 pt-2">
                                            {rowModels.map((row) => (
                                                <CacheRow
                                                    key={row.entry.infoHash}
                                                    row={row}
                                                    open={openCard === row.entry.infoHash}
                                                    onToggleOpen={toggleCard}
                                                    streamingMode={streamingMode.enabled}
                                                    refetching={refetching.includes(row.entry.infoHash)}
                                                    onRefetch={refetch}
                                                    onPlay={play}
                                                    onPlayEpisode={playEpisode}
                                                    onPlayFile={playFile}
                                                    onMoreInfo={openMetaDetails}
                                                    onSetPaused={setPaused}
                                                    onSetPinned={setPinned}
                                                    onSetFileSelected={setFileSelected}
                                                    onReloadFiles={reloadFiles}
                                                    onDelete={remove}
                                                    onReveal={canReveal ? reveal : null}
                                                />
                                            ))}
                                        </div>
                                    </>
                }
            </div>
        </ModalRoute>
    );
};

export default Cached;
