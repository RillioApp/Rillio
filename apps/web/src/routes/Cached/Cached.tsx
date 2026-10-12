import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Play, Info, Trash2, X, ChevronRight, Search } from 'lucide-react';
import { useNavigate } from 'react-router';
import { toPath } from 'rillio-router';
import { useCore } from 'rillio/core';
import { useProfile } from 'rillio/common';
import { fetchStreamingModeEnabled, postStreamingModeEnabled } from 'rillio/common/streamingMode';
import { playerDeepLink } from 'rillio/common/cacheMetadata';
import { Button, IconButton, ModalRoute, Switch, cn } from 'rillio/components/ui';
import AnimatedPercentage from 'rillio/components/ui/animated-percentage';
import SpeedChart from 'rillio/components/ui/speed-chart';
import useCachedTorrents, { CacheEntry } from './useCachedTorrents';
import TorrentFiles from './TorrentFiles';
import useMetadataMatcher from './useMetadataMatcher';

// What to call an entry: the matched title (with the episode tag for a series)
// when the torrent has been identified, otherwise the scene filename it came
// with. See common/cacheMetadata.
const displayName = (entry: CacheEntry): string => {
    const meta = entry.meta;
    if (meta === undefined) return entry.name || entry.infoHash;
    const episode = typeof meta.season === 'number' && typeof meta.episode === 'number' ?
        ` S${String(meta.season).padStart(2, '0')}E${String(meta.episode).padStart(2, '0')}`
        :
        '';
    return `${meta.name}${episode}`;
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

// The tag a row with an issue carries, by the server's issue kind. A kind this
// build does not know still gets a tag: the reason beside it says the rest.
const ISSUE_LABELS: Record<string, string> = {
    'metadata-missing': 'Waiting to restore',
    'engine-error': 'Stopped on an error',
    'data-missing': 'Files missing',
};
const issueLabel = (kind: string): string => ISSUE_LABELS[kind] ?? 'Needs attention';

// Same parser the stream cards use: quality/HDR/languages are encoded in the
// entry name (the selected file's name for single-file selections, e.g. an
// episode filename, otherwise the torrent name - both carry the same tokens).
const { parseStream } = require('rillio/routes/MetaDetails/StreamsList/streamQuality');

// The toolbar's decision logic (sort orders, filter facets, search), pure and
// jest-covered in cacheListOps; quality parsing and naming are injected from
// here so there is exactly one tokenizer and one naming rule.
const { SORTS, FILTERS, sortEntries, filterEntries } = require('./cacheListOps');

const SORT_LABELS: Record<string, string> = {
    date: 'Newest',
    size: 'Largest',
    quality: 'Quality',
};

// Short labels: the whole toolbar is ONE quiet row, and "Downloading" alone
// would eat a chip row's worth of it. Active = an unfinished transfer, Done =
// complete on disk.
const FILTER_LABELS: Record<string, string> = {
    '4k': '4K',
    '1080p': '1080p',
    hdr: 'HDR',
    downloading: 'Active',
    complete: 'Done',
    kept: 'Kept',
};

// "3w ago" for the row subline: at a glance is all the date sort needs to be
// verifiable; exact timestamps would just be noise at this size.
const formatAge = (addedAt: number | undefined): string | null => {
    if (typeof addedAt !== 'number' || addedAt <= 0) return null;
    const seconds = Math.max(0, (Date.now() - addedAt) / 1000);
    if (seconds < 3600) return 'just now';
    if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
    if (seconds < 86400 * 7) return `${Math.round(seconds / 86400)}d ago`;
    if (seconds < 86400 * 30) return `${Math.round(seconds / (86400 * 7))}w ago`;
    return `${Math.round(seconds / (86400 * 30))}mo ago`;
};

type ParsedQuality = {
    resolution: number,
    hdr: boolean,
    flags: string[],
};

const GB = 1024 ** 3;
const MB = 1024 ** 2;

const formatBytes = (bytes: number): string => {
    if (bytes >= GB) return `${(bytes / GB).toFixed(2)} GB`;
    if (bytes >= MB) return `${(bytes / MB).toFixed(1)} MB`;
    return `${Math.max(0, Math.round(bytes / 1024))} KB`;
};

const stateLabel = (entry: CacheEntry): string => {
    if (entry.state === 'error') return entry.error ? `Error: ${entry.error}` : 'Error';
    if (entry.state === 'paused') return 'Paused';
    if (entry.total > 0 && entry.downloaded >= entry.total) return 'Complete';
    if (entry.state === 'live') return 'Downloading';
    return 'Preparing';
};

// One tag treatment: a fixed-height pill that can never wrap or be squeezed, with
// the small-caps typography a two-letter label needs to read as deliberate.
const BADGE = 'inline-flex h-5 shrink-0 items-center whitespace-nowrap rounded-md bg-surface px-2 text-[0.625rem] font-semibold uppercase leading-none tracking-wider text-fg-muted';

// Quality badges parsed from the torrent name, mirroring the stream cards.
const QualityBadges = ({ name }: { name: string }) => {
    const quality: ParsedQuality = useMemo(() => parseStream({ name, description: '' }), [name]);
    const resolution = quality.resolution >= 2160 ? '4K' :
        quality.resolution > 0 ? `${quality.resolution}p` : null;
    if (resolution === null && !quality.hdr && quality.flags.length === 0) {
        return null;
    }
    return (
        // shrink-0 all the way down: this sits next to a truncating title in a flex
        // row, and without it the row squeezes the badges until their labels wrap
        // mid-word ("HD R"). Fixed height + leading-none centers the glyphs; uppercase
        // + wide tracking is what makes a two-letter tag read as a tag.
        <span className="inline-flex shrink-0 items-center gap-1.5">
            {
                resolution !== null ?
                    <span className={BADGE}>{resolution}</span>
                    :
                    null
            }
            {
                quality.hdr ?
                    <span className={BADGE}>HDR</span>
                    :
                    null
            }
            {
                quality.flags.length > 0 ?
                    <span className="shrink-0 whitespace-nowrap text-[0.6875rem] font-medium tracking-wide text-fg-subtle">{quality.flags.slice(0, 4).join(' ')}</span>
                    :
                    null
            }
        </span>
    );
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

// Speed telemetry for the hero: samples the byte delta between cache polls
// (~3s apart). A poll that brings no new bytes records an honest 0 so the
// chart shows stalls instead of freezing on the last good bar.
const HISTORY_LENGTH = 48;
const useSpeedHistory = (entry: CacheEntry | null) => {
    const [speeds, setSpeeds] = useState<number[]>([]);
    const peakRef = useRef(0);
    const lastRef = useRef<{ hash: string, t: number, bytes: number } | null>(null);
    useEffect(() => {
        if (entry === null) {
            lastRef.current = null;
            peakRef.current = 0;
            setSpeeds([]);
            return;
        }
        const now = performance.now();
        const last = lastRef.current;
        if (last === null || last.hash !== entry.infoHash) {
            lastRef.current = { hash: entry.infoHash, t: now, bytes: entry.downloaded };
            peakRef.current = 0;
            setSpeeds([]);
            return;
        }
        const dt = (now - last.t) / 1000;
        // The poll interval is 3s; anything much faster is a state echo (a
        // mutation re-poll), not a fresh sample worth charting.
        if (dt < 1) return;
        const speed = Math.max(0, (entry.downloaded - last.bytes) / dt);
        lastRef.current = { hash: entry.infoHash, t: now, bytes: entry.downloaded };
        peakRef.current = Math.max(peakRef.current, speed);
        setSpeeds((h) => [...h, speed].slice(-HISTORY_LENGTH));
    }, [entry]);
    return { speeds, peak: peakRef.current, speed: speeds.length > 0 ? speeds[speeds.length - 1] : 0 };
};

const formatSpeed = (bytesPerSec: number): string => `${formatBytes(bytesPerSec)}/s`;

const formatEta = (seconds: number): string => {
    if (!Number.isFinite(seconds) || seconds <= 0) return '-';
    if (seconds >= 3600) return `${Math.floor(seconds / 3600)}h ${Math.round((seconds % 3600) / 60)}m`;
    const m = Math.floor(seconds / 60);
    const s = Math.round(seconds % 60);
    return m > 0 ? `${m}m ${s}s` : `${s}s`;
};

// Over-artwork glass (white-alpha family: these sit on the backdrop image, not
// on a panel, so the house surface tokens would read muddy here).
const GLASS_BTN = 'h-9 shrink-0 rounded-full border border-white/15 bg-white/10 px-4 text-[0.8rem] font-medium text-white backdrop-blur-md transition hover:bg-white/20';

/**
 * The download hero: the active download presented like a title, not a row -
 * backdrop artwork under a gradient, the title logo (or clean name), the
 * slot-machine percentage, a live speed chart and glass stats. The layout is
 * hydralauncher/hydra's downloads hero (MIT) rebuilt on the house tokens, with
 * metahub artwork standing in for their game art.
 */
const HeroDownload = ({ entry, links, metaLink, streamingMode, onPlay, onPlayFile, onMoreInfo, onSetPaused, onSetPinned, onSetFileSelected, onDelete }: {
    entry: CacheEntry,
    links: LibraryLinks | undefined,
    metaLink: string | null,
    streamingMode: boolean | null,
    onPlay: (entry: CacheEntry) => void,
    onPlayFile: (infoHash: string, fileIdx: number, name: string) => void,
    onMoreInfo: (link: string) => void,
    onSetPaused: (infoHash: string, paused: boolean) => void,
    onSetPinned: (infoHash: string, pinned: boolean) => void,
    onSetFileSelected: (infoHash: string, fileIdx: number, selected: boolean) => Promise<void> | void,
    onDelete: (infoHash: string) => void,
}) => {
    const { speeds, peak, speed } = useSpeedHistory(entry);
    const [filesOpen, setFilesOpen] = useState(false);
    const paused = entry.state === 'paused';
    const progress = entry.total > 0 ? Math.min(1, entry.downloaded / entry.total) : 0;
    const pct = Math.floor(progress * 100);
    const etaSeconds = speed > 0 ? (entry.total - entry.downloaded) / speed : NaN;

    // The matched metadata is the best source (it carries the addon's own
    // artwork urls and works for any identified torrent); the continue-watching
    // bridge + metahub stay as the fallback for anything not yet identified.
    const metaId = entry.meta?.metaId ?? links?.metaId ?? null;
    const title = entry.meta !== undefined ? displayName(entry) : (links?.itemName ?? entry.name);
    const backgroundUrl = entry.meta?.background ?? metahubBackground(metaId);
    const logoUrl = entry.meta?.logo ?? metahubLogo(metaId);
    const [backgroundFailed, setBackgroundFailed] = useState(false);
    const [logoFailed, setLogoFailed] = useState(false);
    useEffect(() => { setBackgroundFailed(false); setLogoFailed(false); }, [metaId]);

    return (
        <div className="relative overflow-hidden">
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
            {/* Melts the artwork into the list below; also carries the whole hero
                when there is no artwork (non-IMDb ids). */}
            <div className="absolute inset-0 bg-gradient-to-b from-black/35 via-bg/80 to-bg" />

            <div className="relative flex flex-col gap-4 px-6 pb-5 pt-6">
                <div className="flex min-h-14 items-center">
                    {
                        logoUrl !== null && !logoFailed ?
                            <img
                                src={logoUrl}
                                alt={title}
                                onError={() => setLogoFailed(true)}
                                className="max-h-16 max-w-[55%] object-contain object-left drop-shadow-[0_2px_12px_rgba(0,0,0,0.9)]"
                            />
                            :
                            <div className="flex min-w-0 items-center gap-2.5">
                                <div className="truncate text-2xl font-bold text-white [text-shadow:0_2px_12px_rgba(0,0,0,0.9)]" title={title}>{title}</div>
                                <QualityBadges name={entry.name} />
                            </div>
                    }
                </div>

                <div className="flex items-end justify-between gap-4">
                    <div className="flex flex-col gap-1">
                        <div className="text-[0.68rem] font-semibold uppercase tracking-wider text-white/70">
                            {paused ? 'Paused' : 'Downloading'}
                        </div>
                        <div className="text-[1.9rem] font-bold leading-none tracking-[-0.02em] text-white">
                            <AnimatedPercentage text={`${pct}%`} />
                        </div>
                    </div>
                    <div className="flex items-center gap-2">
                        {
                            // Keep only matters while auto-clean can delete things;
                            // with streaming mode off nothing is ever cleaned up, so
                            // the control would be a no-op and reads as noise.
                            streamingMode === true ?
                                <Button
                                    variant="ghost"
                                    className={cn(GLASS_BTN, entry.pinned && 'border-accent/40 bg-accent/20 text-accent')}
                                    onClick={() => onSetPinned(entry.infoHash, !entry.pinned)}
                                    title={entry.pinned ? 'Kept: never cleaned up automatically. Click to allow cleanup' : 'Keep this title in the cache (never cleaned up automatically)'}
                                >
                                    {entry.pinned ? 'Kept' : 'Keep'}
                                </Button>
                                :
                                null
                        }
                        <Button variant="ghost" className={GLASS_BTN} onClick={() => onSetPaused(entry.infoHash, !paused)} title={paused ? 'Resume this download' : 'Pause this download'}>
                            {paused ? 'Resume' : 'Pause'}
                        </Button>
                        {
                            // Streaming DURING the download is the point of a streaming
                            // server, so Play is offered as soon as there is a file to
                            // play. When the torrent holds several videos there is no
                            // single answer, so the Files button below is the way in.
                            typeof entry.fileIdx === 'number' ?
                                <IconButton onClick={() => onPlay(entry)} title="Play (streams while it downloads)" className="size-9 text-accent hover:brightness-110">
                                    <Play className="size-5" />
                                </IconButton>
                                :
                                <Button
                                    variant="ghost"
                                    className={cn(GLASS_BTN, filesOpen && 'bg-white/20')}
                                    onClick={() => setFilesOpen((open) => !open)}
                                    title="Show the files in this torrent"
                                >
                                    Files
                                </Button>
                        }
                        {
                            typeof metaLink === 'string' ?
                                <IconButton onClick={() => onMoreInfo(metaLink)} title="More info" className="size-9 text-white/70 hover:text-white">
                                    <Info className="size-5" />
                                </IconButton>
                                :
                                null
                        }
                        <IconButton onClick={() => onDelete(entry.infoHash)} title="Delete from cache (frees disk space; can be re-downloaded)" className="size-9 text-white/70 hover:text-danger">
                            <Trash2 className="size-5" />
                        </IconButton>
                    </div>
                </div>

                <div className="h-1 w-full overflow-hidden rounded-full bg-white/10">
                    <div className="h-full rounded-full bg-accent transition-[width] duration-700" style={{ width: `${Math.round(progress * 100)}%` }} />
                </div>

                {/* Same glass-and-bars treatment, a third of the height: the chart
                    keeps its own row (the accent bars ARE the look) but a short one,
                    and the three stats sit on ONE line instead of stacking. */}
                <div className="flex flex-col gap-1.5 rounded-xl border border-white/10 bg-white/5 px-4 py-2.5 backdrop-blur-md">
                    <SpeedChart speeds={speeds} peakSpeed={peak} height={26} />
                    <div className="flex items-baseline justify-between gap-4 text-[0.8rem] tabular-nums">
                        <span className="text-white/90">
                            <span className="mr-1.5 text-[0.6rem] font-semibold uppercase tracking-wider text-white/50">Speed</span>
                            <span className="font-semibold">{formatSpeed(speed)}</span>
                        </span>
                        <span className="text-white/90">
                            <span className="mr-1.5 text-[0.6rem] font-semibold uppercase tracking-wider text-white/50">Done</span>
                            <span className="font-semibold">{formatBytes(entry.downloaded)}</span>
                            <span className="text-white/45">{` / ${formatBytes(entry.total)}`}</span>
                        </span>
                        <span className="text-white/90">
                            <span className="mr-1.5 text-[0.6rem] font-semibold uppercase tracking-wider text-white/50">Left</span>
                            <span className="font-semibold">{paused ? '-' : formatEta(etaSeconds)}</span>
                        </span>
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
                        />
                    </div>
                    :
                    null
            }
        </div>
    );
};

const Row = ({ entry, metaLink, streamingMode, refetching, onPlay, onPlayFile, onMoreInfo, onSetPaused, onSetPinned, onSetFileSelected, onDelete, onRefetch }: {
    entry: CacheEntry,
    metaLink: string | null,
    streamingMode: boolean | null,
    // A "Fetch again" for this row is under way.
    refetching: boolean,
    onRefetch: (infoHash: string) => void,
    onPlay: (entry: CacheEntry) => void,
    onPlayFile: (infoHash: string, fileIdx: number, name: string) => void,
    onMoreInfo: (link: string) => void,
    onSetPaused: (infoHash: string, paused: boolean) => void,
    onSetPinned: (infoHash: string, pinned: boolean) => void,
    onSetFileSelected: (infoHash: string, fileIdx: number, selected: boolean) => Promise<void> | void,
    onDelete: (infoHash: string) => void,
}) => {
    const [filesOpen, setFilesOpen] = useState(false);
    const progress = entry.total > 0 ? Math.min(1, entry.downloaded / entry.total) : 0;
    const complete = entry.total > 0 && entry.downloaded >= entry.total;
    const paused = entry.state === 'paused';
    // While initializing there is no have-byte count to show: librqbit has not
    // built the chunk tracker yet, and its torrent-level counter means the HASH
    // CHECK's scan there, not data we hold. Showing "0 KB / 9.29 GB" next to a
    // half-downloaded file reads as lost progress, and a progress bar at 0% says
    // the same thing louder. Show the size alone until the real count exists.
    const preparing = entry.state === 'initializing';
    // Pause/resume only makes sense mid-download: live-and-incomplete or paused.
    // NOT while initializing - librqbit hard-refuses that ("torrent is
    // initializing, can't pause"), so offering the button only produced a pause
    // that reported success and did nothing. Delete still works there.
    // The server's verdict (see CacheEntry.issue). A row with an issue is still
    // a normal row: it carries a tag and its reason, offers "Fetch again" and
    // "Find another source", and is never offered as playable.
    const issue = entry.issue;
    // In the cache but not in the engine: there is no file list to open yet.
    const waiting = entry.state === 'waiting';
    const similarLink = streamsPageLink(entry);
    const pausable = issue === undefined && !complete && (entry.state === 'live' || paused);
    return (
        <div>
            <div className="group flex items-center gap-4 px-6 py-4 transition hover:bg-white/5">
                {/* The advanced affordance: a torrent is a directory, and this opens it.
                Quiet by default (a chevron, not a button) so the row still reads as
                one title, which is what it is for almost everyone. */}
                <IconButton
                    onClick={() => setFilesOpen((open) => !open)}
                    title={filesOpen ? 'Hide the files in this torrent' : 'Show all files in this torrent'}
                    className={cn('-ml-2 size-6 shrink-0 text-fg-subtle hover:text-fg', waiting && 'invisible')}
                >
                    <ChevronRight className={cn('size-4 transition-transform', filesOpen && 'rotate-90')} />
                </IconButton>
                <div className="min-w-0 flex-1">
                    <div className="flex min-w-0 items-center gap-2.5">
                        <div className="truncate text-sm font-medium text-fg" title={entry.name}>
                            {displayName(entry)}
                        </div>
                        <QualityBadges name={entry.name} />
                        {
                        // The issue tag: a quiet status-coloured word, not a badge.
                            issue !== undefined ?
                                <span className="shrink-0 whitespace-nowrap text-[0.6875rem] font-medium text-warning">
                                    {issueLabel(issue.kind)}
                                </span>
                                :
                                null
                        }
                    </div>
                    {/* danger, not warning: this row FAILED, it is not a caution. The rest
                    of the app already draws that line (warning = the slow-source and
                    oversized-library notices; danger = every error and every
                    destructive control), and this file was the last holdout. */}
                    {
                    // A row with an issue says what is wrong (or that a fetch is
                    // under way) where the bytes and state would be: byte counts
                    // for a torrent the engine is not running would be invented.
                        issue !== undefined ?
                            <div className="mt-1 truncate text-xs text-fg-muted" title={issue.reason}>
                                {refetching ? 'Fetching again, this can take a few minutes' : issue.reason}
                            </div>
                            :
                            null
                    }
                    <div className={cn('mt-1 text-xs tabular-nums', entry.state === 'error' ? 'text-danger' : 'text-fg-muted', issue !== undefined && 'hidden')}>
                        {
                            complete ?
                                formatBytes(entry.downloaded)
                                :
                                preparing ?
                                    (entry.total > 0 ? formatBytes(entry.total) : null)
                                    :
                                    <>
                                        {formatBytes(entry.downloaded)}
                                        {entry.total > 0 ? <span className="text-fg-subtle">{` / ${formatBytes(entry.total)}`}</span> : null}
                                    </>
                        }
                        <span className="text-fg-subtle">{' · '}</span>
                        {stateLabel(entry)}
                        {
                        // Only for a genuine pack. A movie shipping a .nfo beside it
                        // resolves to one playable video, and calling that "2 files"
                        // read as "this is a season pack" when it is not.
                            entry.fileCount > 1 && typeof entry.fileIdx !== 'number' ?
                                <><span className="text-fg-subtle">{' · '}</span>{`${entry.fileCount} files`}</>
                                :
                                null
                        }
                        {
                        // Honest about the pending cleanup: with streaming mode on, a
                        // watched un-kept stream is on the sweeper's list.
                            streamingMode === true && entry.watched && !entry.pinned ?
                                <><span className="text-fg-subtle">{' · '}</span>Watched, cleans up soon</>
                                :
                                null
                        }
                        {
                        // The age makes the (default) newest-first order legible.
                        // Finished rows only: an active row's subline already
                        // carries progress and state.
                            complete && formatAge(entry.addedAt) !== null ?
                                <span className="text-fg-subtle">{` · ${formatAge(entry.addedAt)}`}</span>
                                :
                                null
                        }
                    </div>
                    {
                        issue === undefined && !complete && !preparing && entry.total > 0 ?
                            <div className="mt-2.5 h-0.5 w-full overflow-hidden rounded-full bg-surface">
                                <div className="h-full rounded-full bg-accent transition-[width] duration-700" style={{ width: `${Math.round(progress * 100)}%` }} />
                            </div>
                            :
                            null
                    }
                </div>
                {
                // The two ways out of an issue, revealed on hover like the other
                // row actions: try again to get what is missing, or go and pick
                // another source for the same title.
                    issue !== undefined ?
                        <>
                            <Button
                                variant="outline"
                                size="sm"
                                disabled={refetching}
                                onClick={() => onRefetch(entry.infoHash)}
                                title="Try now to get what this download is missing. What is already on disk is kept."
                                className={cn(
                                    'shrink-0 bg-surface px-3.5 text-fg-muted hover:text-fg',
                                    refetching ? 'opacity-100' : 'opacity-0 group-hover:opacity-100',
                                )}
                            >
                                {refetching ? 'Fetching' : 'Fetch again'}
                            </Button>
                            {
                                similarLink !== null ?
                                    <Button
                                        variant="outline"
                                        size="sm"
                                        onClick={() => onMoreInfo(similarLink)}
                                        title="Open this title and pick another source for it"
                                        className="shrink-0 bg-surface px-3.5 text-fg-muted opacity-0 hover:text-fg group-hover:opacity-100"
                                    >
                                        Find another source
                                    </Button>
                                    :
                                    null
                            }
                        </>
                        :
                        null
                }
                {/* Same labeled-pill treatment as Pause. Kept state stays visible
                (it is standing information, not just an action); the un-kept
                affordance appears on hover like Info/Delete. Hidden entirely
                while auto-clean is off: nothing deletes, so Keep is a no-op. */}
                {
                    streamingMode === true ?
                        <Button
                            variant="outline"
                            size="sm"
                            onClick={() => onSetPinned(entry.infoHash, !entry.pinned)}
                            title={entry.pinned ? 'Kept: never cleaned up automatically. Click to allow cleanup' : 'Keep this title in the cache (never cleaned up automatically)'}
                            className={cn(
                                'shrink-0 bg-surface px-3.5',
                                entry.pinned ?
                                    'border-accent/40 text-accent opacity-100 hover:brightness-110'
                                    :
                                    'text-fg-muted opacity-0 hover:text-fg group-hover:opacity-100',
                            )}
                        >
                            {entry.pinned ? 'Kept' : 'Keep'}
                        </Button>
                        :
                        null
                }
                {
                // A labeled pill (not an icon): a pause/resume TRIANGLE would be
                // confusable with the media Play button, and inline status text
                // did not read as clickable. Always visible while transferring.
                    pausable ?
                        <Button
                            variant="outline"
                            size="sm"
                            onClick={() => onSetPaused(entry.infoHash, !paused)}
                            title={paused ? 'Resume this download' : 'Pause this download'}
                            className="shrink-0 bg-surface px-3.5 text-fg-muted hover:text-fg"
                        >
                            {paused ? 'Resume' : 'Pause'}
                        </Button>
                        :
                        null
                }
                {
                // Never for a row with an issue: what it would play is in doubt.
                    issue === undefined && typeof entry.fileIdx === 'number' ?
                        <IconButton
                            onClick={() => onPlay(entry)}
                            title="Play"
                            className="size-9 text-accent opacity-100 hover:brightness-110"
                        >
                            <Play className="size-5" />
                        </IconButton>
                        :
                        null
                }
                {
                    typeof metaLink === 'string' ?
                        <IconButton
                            onClick={() => onMoreInfo(metaLink)}
                            title="More info"
                            className="size-9 text-fg-muted opacity-0 hover:text-fg group-hover:opacity-100"
                        >
                            <Info className="size-5" />
                        </IconButton>
                        :
                        null
                }
                <IconButton
                    onClick={() => onDelete(entry.infoHash)}
                    title="Delete from cache (frees disk space; can be re-downloaded)"
                    className="size-9 text-fg-muted opacity-0 hover:text-danger group-hover:opacity-100"
                >
                    <Trash2 className="size-5" />
                </IconButton>
            </div>
            {
                filesOpen ?
                    <TorrentFiles
                        infoHash={entry.infoHash}
                        initializing={preparing}
                        onPlayFile={onPlayFile}
                        onSetFileSelected={onSetFileSelected}
                    />
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
    const { entries, failed, error, refresh, setPinned, setPaused, setFileSelected, remove, refetch, refetching } = useCachedTorrents();
    const libraryLinks = useLibraryLinksByInfoHash();
    const streamingMode = useStreamingModeSetting();
    // Identify anything the Player never got to write metadata for.
    useMetadataMatcher(entries, refresh);

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

    // Play one specific file out of the torrent (the file browser). Always the
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

    const openMetaDetails = useCallback((link: string) => {
        closeCached();
        navigate(toPath(link));
    }, [navigate, closeCached]);

    const totalBytes = useMemo(
        () => (entries ?? []).reduce((sum, entry) => sum + entry.downloaded, 0),
        [entries],
    );

    // Toolbar state. Newest-first is the default order; filters are toggle
    // chips that AND together; the search box matches titles and filenames.
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

    // What the list actually shows: filters + search, then the chosen order.
    const visible = useMemo(
        () => sortEntries(
            filterEntries(entries ?? [], filters, query, getQuality, displayName),
            sort,
            getQuality,
        ) as CacheEntry[],
        [entries, filters, query, sort, getQuality],
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

    return (
        <ModalRoute
            open
            onClose={closeCached}
            title="Cache"
            hideHeader
            showClose={false}
            className="flex h-[min(42rem,calc(100vh-6rem))] w-[min(52rem,calc(100vw-4rem))] max-w-[min(52rem,calc(100vw-4rem))] flex-col gap-0 overflow-hidden border border-line p-0"
        >
            <div className="flex items-start gap-3 px-6 pb-3 pt-5">
                <div className="min-w-0">
                    <div className="flex items-baseline gap-3">
                        <h1 className="text-xl font-semibold text-fg">Cache</h1>
                        {
                            entries !== null && entries.length > 0 ?
                                <div className="text-sm tabular-nums text-fg-muted">{formatBytes(totalBytes)} on disk</div>
                                :
                                null
                        }
                    </div>
                    <div className="mt-1 text-xs text-fg-subtle">
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
                    className={cn('size-9 text-fg-muted', streamingMode.enabled === null && 'ml-auto')}
                >
                    <X className="size-5" />
                </IconButton>
            </div>
            {
                // The toolbar: ONE quiet hairline row - a bare inline search, the
                // filter facets as text chips (accent when active), and the sort
                // behind a thin divider. No pills, no boxes: the flat list below
                // is the surface, this is just its caption.
                entries !== null && entries.length > 0 ?
                    <div className="flex h-10 shrink-0 items-center gap-3 border-b border-line px-6">
                        <Search className="size-3.5 shrink-0 text-fg-subtle" />
                        <input
                            type="text"
                            value={query}
                            onChange={(event) => setQuery(event.target.value)}
                            placeholder="Search the cache"
                            className="min-w-0 flex-1 bg-transparent text-xs text-fg outline-none placeholder:text-fg-subtle"
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
                                                'h-6 rounded-full px-2.5 text-[0.6875rem] font-medium transition',
                                                active ?
                                                    'bg-accent/15 text-accent'
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
                        <div className="h-3.5 w-px shrink-0 bg-line" />
                        <div className="flex shrink-0 items-center gap-1">
                            {
                                (SORTS as string[]).map((value) => (
                                    <button
                                        key={value}
                                        type="button"
                                        onClick={() => setSort(value)}
                                        className={cn(
                                            'h-6 rounded-full px-2.5 text-[0.6875rem] font-medium transition',
                                            sort === value ?
                                                'bg-accent/15 text-accent'
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
            <div className="min-h-0 flex-1 overflow-y-auto">
                {
                    failed ?
                        // The reason is shown as it arrived (the shell's own words
                        // for a server that did not start, or the failed request):
                        // it is what tells a slow start from a dead one, and what
                        // to send when reporting it. The list keeps re-checking on
                        // its own every few seconds; the button is for right now.
                        <div className="flex flex-col items-center gap-3 px-5 py-10 text-center">
                            <div className="text-sm text-fg-muted">The cache could not be loaded.</div>
                            {
                                error !== null ?
                                    <div className="max-w-md select-text break-words text-xs text-danger">{error}</div>
                                    :
                                    null
                            }
                            <div className="text-xs text-fg-subtle">Checking again automatically.</div>
                            <Button variant="outline" size="sm" onClick={refresh} className="bg-surface px-3.5 text-fg-muted hover:text-fg">
                                Try again
                            </Button>
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
                                            heroEntry !== null ?
                                                <HeroDownload
                                                    entry={heroEntry}
                                                    links={libraryLinks[heroEntry.infoHash]}
                                                    metaLink={metaDetailsLink(heroEntry) ?? libraryLinks[heroEntry.infoHash]?.metaLink ?? null}
                                                    streamingMode={streamingMode.enabled}
                                                    onPlay={play}
                                                    onPlayFile={playFile}
                                                    onMoreInfo={openMetaDetails}
                                                    onSetPaused={setPaused}
                                                    onSetPinned={setPinned}
                                                    onSetFileSelected={setFileSelected}
                                                    onDelete={remove}
                                                />
                                                :
                                                null
                                        }
                                        <div className="divide-y divide-surface">
                                            {visible.filter((entry) => entry.infoHash !== heroEntry?.infoHash).map((entry) => (
                                                <Row
                                                    key={entry.infoHash}
                                                    entry={entry}
                                                    metaLink={metaDetailsLink(entry) ?? libraryLinks[entry.infoHash]?.metaLink ?? null}
                                                    streamingMode={streamingMode.enabled}
                                                    refetching={refetching.includes(entry.infoHash)}
                                                    onRefetch={refetch}
                                                    onPlay={play}
                                                    onPlayFile={playFile}
                                                    onMoreInfo={openMetaDetails}
                                                    onSetPaused={setPaused}
                                                    onSetPinned={setPinned}
                                                    onSetFileSelected={setFileSelected}
                                                    onDelete={remove}
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
