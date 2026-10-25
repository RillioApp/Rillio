// Copyright (C) 2017-2026 Smart code 203358507

/**
 * One download in the Cache window's list.
 *
 * At rest, three parts and nothing else: WHAT it is (title, "S02E03 · title"
 * or "Season 2 pack · 5 episodes", or a cleaned name and "Not matched to a
 * title"), WHICH copy (the release line), and on the right what is on disk
 * and what was watched when. A pack adds a strip of one bar per episode.
 *
 * On hover or keyboard focus (and on click, for touch) the row opens in place:
 * the full release name, where it came from and exact times, the actions
 * (the same ones and the same rules as before), and every file of the torrent:
 * for a pack, an episode table over the video files and the other files
 * (.nfo, subtitles, samples, extras) below it; otherwise the file list as the
 * file browser shows it. Each file keeps its download / stop, play and show-in-
 * folder controls. The open state reflows the list (no opacity tricks).
 *
 * Every word on it comes from cacheRowModel (jest-covered).
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Play, Info, Trash2, FolderOpen } from 'lucide-react';
import { Button, IconButton, cn } from 'rillio/components/ui';
import type { CacheEntry } from './useCachedTorrents';
import { FileRow, FileActions, FileProgress, fileState, useFileToggle, type TorrentFile } from './TorrentFiles';
import type { RowModel, ReleasePart, StripBar, EpisodeRowModel } from './cacheRowTypes';

const model = require('./cacheRowModel') as {
    formatSize: (bytes: number) => string,
    formatOf: (done: number, total: number) => string,
    minutesOf: (view: { offset: number, duration: number }) => string,
    basename: (path: string) => string,
    onDisk: (file: TorrentFile) => boolean,
};

// The tag a row with an issue carries, by the server's issue kind.
const ISSUE_LABELS: Record<string, string> = {
    'metadata-missing': 'Waiting to restore',
    'engine-error': 'Stopped on an error',
    'data-missing': 'Files missing',
};
const issueLabel = (kind: string): string => ISSUE_LABELS[kind] ?? 'Needs attention';
const ISSUE_TEXT: Record<string, string> = {
    'metadata-missing': 'Rillio lost the details of this download and is getting them again. What is on disk is kept.',
    'engine-error': 'This download stopped on an error.',
    'data-missing': 'Some of its files are no longer on disk.',
};
const issueText = (kind: string): string => ISSUE_TEXT[kind] ?? 'Something is wrong with this download.';
// The issues "Fetch again" can do something about (see Engine::refetch).
const FETCHABLE_ISSUES = ['metadata-missing', 'engine-error'];

// Clipboard API first; where it is refused, the legacy copy from an off-screen
// textarea. Resolves to whether the text was copied.
const copyText = (text: string): Promise<boolean> => {
    const legacy = (): boolean => {
        const area = document.createElement('textarea');
        area.value = text;
        area.setAttribute('readonly', '');
        area.style.position = 'fixed';
        area.style.left = '-9999px';
        document.body.appendChild(area);
        area.select();
        let ok = false;
        try { ok = document.execCommand('copy'); } catch { ok = false; }
        document.body.removeChild(area);
        return ok;
    };
    return navigator.clipboard?.writeText ?
        navigator.clipboard.writeText(text).then(() => true, legacy)
        :
        Promise.resolve(legacy());
};

/** "1080p · ATVP WEB-DL · H.264 · NTb", the resolution emphasised. */
export const ReleaseLine = ({ parts, className }: { parts: ReleasePart[], className?: string }) => (
    parts.length === 0 ?
        null
        :
        <div className={cn('flex min-w-0 flex-wrap items-center gap-x-1.5 text-xs text-fg-muted', className)}>
            {parts.map((part, index) => (
                <React.Fragment key={`${part.text}-${index}`}>
                    {index > 0 ? <span className="text-fg-subtle">·</span> : null}
                    <span className={cn('whitespace-nowrap', part.strong && 'font-bold text-fg')}>{part.text}</span>
                </React.Fragment>
            ))}
        </div>
);

/** One bar per episode of a pack. */
const PackStrip = ({ bars, summary }: { bars: StripBar[], summary: string | null }) => (
    <div className="mt-1 flex min-w-0 flex-wrap items-center gap-1">
        {bars.map((bar) => (
            <span
                key={bar.label}
                title={`${bar.label}: ${BAR_WORDS[bar.state]}`}
                className={cn(
                    'h-1.5 w-[1.125rem] shrink-0 rounded-full',
                    bar.state === 'watched' && 'bg-accent',
                    bar.state === 'unwatched' && 'bg-surface-hover',
                    bar.state === 'disk' && 'bg-fg-subtle/60',
                    bar.state === 'missing' && 'border border-dashed border-fg-subtle/60',
                )}
                style={bar.state === 'partial' ?
                    { background: `linear-gradient(90deg, var(--color-accent) ${Math.round((bar.fraction ?? 0) * 100)}%, var(--color-surface-hover) ${Math.round((bar.fraction ?? 0) * 100)}%)` }
                    :
                    undefined}
            />
        ))}
        {summary !== null ? <span className="ml-1.5 truncate text-xs text-fg-subtle">{summary}</span> : null}
    </div>
);
const BAR_WORDS: Record<string, string> = {
    watched: 'watched',
    partial: 'in progress',
    unwatched: 'not started',
    disk: 'on disk',
    missing: 'not downloaded',
};

/** Poster art, or a quiet "?" for a torrent no title was matched to. */
export const Poster = ({ url, className }: { url: string | null, className?: string }) => {
    const [failed, setFailed] = useState(false);
    useEffect(() => setFailed(false), [url]);
    return url !== null && !failed ?
        <img src={url} alt="" onError={() => setFailed(true)} className={cn('h-16 w-[2.875rem] shrink-0 rounded-lg object-cover', className)} />
        :
        <div className={cn('flex h-16 w-[2.875rem] shrink-0 items-center justify-center rounded-lg bg-surface text-lg font-extrabold text-fg-subtle', className)}>
            {url === null ? '?' : ''}
        </div>;
};

const viewedText = (row: EpisodeRowModel): { text: string, tone: 'accent' | 'muted' | 'subtle' } => {
    const view = row.view;
    if (view !== null && view.state === 'watched') return { text: 'Watched', tone: 'accent' };
    if (view !== null && view.state === 'partial') return { text: model.minutesOf(view as { offset: number, duration: number }), tone: 'muted' };
    if (!model.onDisk(row.file)) return { text: '-', tone: 'subtle' };
    if (view !== null) return { text: 'Not started', tone: 'subtle' };
    return { text: '', tone: 'subtle' };
};

/** The pack's episodes: EP, title, size, on disk, viewed, and each file's controls. */
const EpisodeTable = ({ infoHash, rows, pending, initializing, onPlayEpisode, onToggle, onReveal }: {
    infoHash: string,
    rows: EpisodeRowModel[],
    pending: number[],
    initializing: boolean,
    onPlayEpisode: (row: EpisodeRowModel) => void,
    onToggle: (file: TorrentFile) => void,
    onReveal: ((infoHash: string, fileIdx?: number) => void) | null,
}) => (
    <div className="flex flex-col">
        <div className="grid grid-cols-[4.5rem_minmax(0,1fr)_5rem_9rem_7.5rem_6.5rem] items-center gap-3 py-1.5 text-[0.6875rem] font-extrabold uppercase tracking-wider text-fg-subtle">
            <span>Ep</span><span>Title</span><span>Size</span><span>On disk</span><span>Viewed</span><span />
        </div>
        {rows.map((row) => {
            const here = model.onDisk(row.file);
            const viewed = viewedText(row);
            return (
                <div
                    key={row.file.index}
                    className={cn(
                        'grid grid-cols-[4.5rem_minmax(0,1fr)_5rem_9rem_7.5rem_6.5rem] items-center gap-3 border-t border-line py-1.5 text-[0.8125rem]',
                        !here && 'text-fg-subtle',
                    )}
                >
                    <span className="font-bold">{row.label}</span>
                    <span className="truncate" title={row.file.path}>{row.title ?? model.basename(row.file.path)}</span>
                    <span className="tabular-nums text-fg-muted">{model.formatSize(row.file.length)}</span>
                    <span className="flex min-w-0 items-center gap-2 tabular-nums text-fg-muted">
                        <FileProgress file={row.file} className="w-10" />
                        <span className="truncate">{fileState(row.file)}</span>
                    </span>
                    <span className="flex min-w-0 items-center gap-2">
                        {
                            row.view !== null && row.view.state === 'partial' ?
                                <span className="h-0.5 w-8 shrink-0 overflow-hidden rounded-full bg-surface-hover">
                                    <span className="block h-full rounded-full bg-accent" style={{ width: `${Math.round(((row.view.offset ?? 0) / (row.view.duration || 1)) * 100)}%` }} />
                                </span>
                                :
                                null
                        }
                        <span className={cn('truncate', viewed.tone === 'accent' ? 'text-accent' : viewed.tone === 'muted' ? 'text-fg-muted' : 'text-fg-subtle')}>{viewed.text}</span>
                    </span>
                    <FileActions
                        infoHash={infoHash}
                        file={row.file}
                        busy={pending.includes(row.file.index)}
                        initializing={initializing}
                        onPlayFile={() => onPlayEpisode(row)}
                        onToggle={onToggle}
                        onReveal={onReveal}
                    />
                </div>
            );
        })}
    </div>
);

type Handlers = {
    onPlay: (entry: CacheEntry) => void,
    onPlayEpisode: (entry: CacheEntry, row: EpisodeRowModel) => void,
    onPlayFile: (infoHash: string, fileIdx: number, name: string) => void,
    onMoreInfo: (link: string) => void,
    onSetPaused: (infoHash: string, paused: boolean) => void,
    onSetPinned: (infoHash: string, pinned: boolean) => void,
    onSetFileSelected: (infoHash: string, fileIdx: number, selected: boolean) => Promise<void> | void,
    onReloadFiles: (infoHash: string) => void,
    onDelete: (infoHash: string) => void,
    onRefetch: (infoHash: string) => void,
    // Absent where there is no file manager to show (Android, a plain browser).
    onReveal: ((infoHash: string, fileIdx?: number) => void) | null,
};

// How long the pointer must rest on a row before it opens: sweeping across
// the list to reach a row further down must not open every row on the way.
const HOVER_INTENT_MS = 160;

const CacheRow = ({ row, streamingMode, refetching, ...handlers }: {
    row: RowModel,
    streamingMode: boolean | null,
    refetching: boolean,
} & Handlers) => {
    const { entry, described } = row;
    const [hovered, setHovered] = useState(false);
    const [focused, setFocused] = useState(false);
    const [pinnedOpen, setPinnedOpen] = useState(false);
    const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
    const open = hovered || focused || pinnedOpen;
    useEffect(() => () => { if (timer.current !== null) clearTimeout(timer.current); }, []);

    const onPointerEnter = useCallback((event: React.PointerEvent) => {
        if (event.pointerType === 'touch') return;
        timer.current = setTimeout(() => setHovered(true), HOVER_INTENT_MS);
    }, []);
    const onPointerLeave = useCallback(() => {
        if (timer.current !== null) clearTimeout(timer.current);
        timer.current = null;
        setHovered(false);
    }, []);
    const onSummaryClick = useCallback((event: React.MouseEvent) => {
        if ((event.target as Element).closest('[data-row-action]')) return;
        setPinnedOpen((current) => !current);
    }, []);

    const issue = entry.issue;
    const complete = entry.total > 0 && entry.downloaded >= entry.total;
    const paused = entry.state === 'paused';
    const preparing = entry.state === 'initializing';
    const pausable = issue === undefined && !complete && (entry.state === 'live' || paused);
    const reload = useCallback(() => handlers.onReloadFiles(entry.infoHash), [handlers.onReloadFiles, entry.infoHash]);
    const { pending, toggle } = useFileToggle(entry.infoHash, handlers.onSetFileSelected, reload);

    const [copied, setCopied] = useState<'idle' | 'copied' | 'failed'>('idle');
    const copyDetails = useCallback(() => {
        if (issue === undefined) return;
        const details = [
            `Rillio cache entry ${entry.infoHash}`,
            `name: ${entry.name}`,
            `issue: ${issue.kind}`,
            `reason: ${issue.reason}`,
            typeof entry.cannotRemove === 'string' ? `cannot be removed: ${entry.cannotRemove}` : null,
        ].filter((line) => line !== null).join('\n');
        copyText(details).then((ok) => {
            setCopied(ok ? 'copied' : 'failed');
            setTimeout(() => setCopied('idle'), 1500);
        });
    }, [entry.infoHash, entry.name, entry.cannotRemove, issue]);

    const action = row.action;
    const play = () => {
        if (action === null) return;
        if (action.row !== null) handlers.onPlayEpisode(entry, action.row);
        else handlers.onPlay(entry);
    };
    const files = row.files;
    const otherFiles = row.kind === 'pack' ? row.other : (files ?? []);
    const downloadBar = issue === undefined && !complete && !preparing && entry.total > 0 && row.kind !== 'pack';
    const viewBar = !downloadBar && row.view !== null && row.view.state === 'partial';

    return (
        <div
            role="group"
            aria-expanded={open}
            aria-label={`${described.title}${described.subtitle ? `, ${described.subtitle}` : ''}`}
            tabIndex={0}
            onPointerEnter={onPointerEnter}
            onPointerLeave={onPointerLeave}
            onFocus={() => setFocused(true)}
            onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setFocused(false); }}
            className={cn(
                'outline-none transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-highlight',
                open ? 'rounded-2xl bg-surface' : 'hover:bg-surface',
            )}
        >
            <div className="flex cursor-pointer items-center gap-4 px-4 py-3.5" onClick={onSummaryClick}>
                <Poster url={row.poster} />
                <div className="flex min-w-0 flex-1 flex-col gap-1">
                    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5">
                        <span className="truncate text-[0.9375rem] font-bold text-fg" title={entry.name}>{described.title}</span>
                        {described.subtitle !== null ? <span className="text-[0.8125rem] text-fg-muted">{described.subtitle}</span> : null}
                        {
                            described.unmatched ?
                                <span className="rounded-full bg-surface px-2 py-0.5 text-[0.6875rem] font-bold text-fg-subtle">Not matched to a title</span>
                                :
                                null
                        }
                        {
                            issue !== undefined ?
                                <span className="whitespace-nowrap text-[0.6875rem] font-medium text-warning">{issueLabel(issue.kind)}</span>
                                :
                                null
                        }
                        {
                            // Kept is standing information while auto-clean can delete things.
                            streamingMode === true && entry.pinned ?
                                <span className="rounded-full bg-accent-soft px-2 py-0.5 text-[0.6875rem] font-bold text-accent">Kept</span>
                                :
                                null
                        }
                    </div>
                    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
                        <ReleaseLine parts={described.release} />
                        {
                            row.note !== null ?
                                <span className="rounded-full bg-surface-hover px-2 py-0.5 text-[0.6875rem] font-bold text-fg-muted">{row.note}</span>
                                :
                                null
                        }
                    </div>
                    {
                        issue !== undefined ?
                            <div className="line-clamp-2 text-xs text-fg-muted">
                                {
                                    refetching ?
                                        'Fetching again, this can take a few minutes'
                                        :
                                        `${issueText(issue.kind)}${typeof entry.cannotRemove === 'string' ? ' It cannot be removed from here until it is restored.' : ''}`
                                }
                            </div>
                            :
                            null
                    }
                    {row.strip !== null ? <PackStrip bars={row.strip} summary={row.summary} /> : null}
                    {
                        downloadBar ?
                            <div className="mt-1 h-[3px] w-60 max-w-full overflow-hidden rounded-full bg-surface-hover">
                                <div
                                    className={cn('h-full rounded-full transition-[width] duration-700', paused ? 'bg-fg-subtle' : 'bg-accent')}
                                    style={{ width: `${Math.round(Math.min(1, entry.downloaded / entry.total) * 100)}%` }}
                                />
                            </div>
                            :
                            viewBar && row.view !== null ?
                                <div className="mt-1 h-[3px] w-60 max-w-full overflow-hidden rounded-full bg-surface-hover">
                                    <div className="h-full rounded-full bg-accent" style={{ width: `${Math.round(((row.view.offset ?? 0) / (row.view.duration || 1)) * 100)}%` }} />
                                </div>
                                :
                                null
                    }
                </div>
                {
                    open ?
                        <div className="flex shrink-0 flex-wrap items-center justify-end gap-2" data-row-action>
                            {
                                action !== null ?
                                    <Button size="sm" onClick={play} className="h-[2.125rem] px-3.5" title={action.row !== null ? `Play ${action.row.title ?? action.row.label}` : 'Play (streams while it downloads)'}>
                                        <Play className="size-3.5" />{action.label}
                                    </Button>
                                    :
                                    null
                            }
                            {
                                issue !== undefined && FETCHABLE_ISSUES.includes(issue.kind) ?
                                    <Button variant="ghost" size="sm" disabled={refetching} onClick={() => handlers.onRefetch(entry.infoHash)} className="h-[2.125rem] bg-surface-hover px-3.5 text-fg" title="Try now to get what this download is missing. What is already on disk is kept.">
                                        {refetching ? 'Fetching' : 'Fetch again'}
                                    </Button>
                                    :
                                    null
                            }
                            {
                                issue !== undefined ?
                                    <Button variant="ghost" size="sm" onClick={copyDetails} className="h-[2.125rem] bg-surface-hover px-3.5 text-fg" title="Copy the technical details of this problem, for a report">
                                        {copied === 'copied' ? 'Copied' : copied === 'failed' ? 'Couldn\'t copy' : 'Copy details'}
                                    </Button>
                                    :
                                    null
                            }
                            {
                                row.streamsLink !== null ?
                                    <Button variant="ghost" size="sm" onClick={() => handlers.onMoreInfo(row.streamsLink as string)} className="h-[2.125rem] bg-surface-hover px-3.5 text-fg" title="Open this title and pick another source for it">
                                        Find another source
                                    </Button>
                                    :
                                    null
                            }
                            {
                                pausable ?
                                    <Button variant="ghost" size="sm" onClick={() => handlers.onSetPaused(entry.infoHash, !paused)} className="h-[2.125rem] bg-surface-hover px-3.5 text-fg" title={paused ? 'Resume this download' : 'Pause this download'}>
                                        {paused ? 'Resume download' : 'Pause'}
                                    </Button>
                                    :
                                    null
                            }
                            {
                                // Keep only matters while auto-clean can delete things.
                                streamingMode === true ?
                                    <Button
                                        variant="ghost"
                                        size="sm"
                                        onClick={() => handlers.onSetPinned(entry.infoHash, !entry.pinned)}
                                        className={cn('h-[2.125rem] px-3.5', entry.pinned ? 'bg-accent-soft text-accent' : 'bg-surface-hover text-fg')}
                                        title={entry.pinned ? 'Kept: never cleaned up automatically. Click to allow cleanup' : 'Keep this title in the cache (never cleaned up automatically)'}
                                    >
                                        {entry.pinned ? 'Kept' : 'Keep'}
                                    </Button>
                                    :
                                    null
                            }
                            {
                                handlers.onReveal !== null && entry.state !== 'waiting' ?
                                    <IconButton onClick={() => (handlers.onReveal as (h: string) => void)(entry.infoHash)} title="View location: show this download in its folder" className="size-[2.125rem] bg-surface-hover text-fg-muted hover:text-fg">
                                        <FolderOpen className="size-4" />
                                    </IconButton>
                                    :
                                    null
                            }
                            {
                                row.metaLink !== null ?
                                    <IconButton onClick={() => handlers.onMoreInfo(row.metaLink as string)} title="More info" className="size-[2.125rem] bg-surface-hover text-fg-muted hover:text-fg">
                                        <Info className="size-4" />
                                    </IconButton>
                                    :
                                    null
                            }
                            {
                                // No remove action the server has said it cannot carry out.
                                typeof entry.cannotRemove !== 'string' ?
                                    <IconButton onClick={() => handlers.onDelete(entry.infoHash)} title="Delete from cache (frees disk space; can be re-downloaded)" className="size-[2.125rem] bg-surface-hover text-fg-muted hover:text-danger">
                                        <Trash2 className="size-4" />
                                    </IconButton>
                                    :
                                    null
                            }
                        </div>
                        :
                        <div className="flex shrink-0 flex-col items-end gap-1 text-right">
                            <span className={cn('text-[0.8125rem] tabular-nums', entry.state === 'error' ? 'text-danger' : 'text-fg')}>{row.disk}</span>
                            {row.viewLine !== null ? <span className={cn('text-xs', row.watchedTone ? 'text-accent' : 'text-fg-subtle')}>{row.viewLine}</span> : null}
                        </div>
                }
            </div>
            {
                open ?
                    <div className="flex flex-col gap-3.5 pb-4 pl-[4.875rem] pr-4">
                        <div className="flex flex-col gap-0.5 text-xs text-fg-subtle">
                            <span className="break-all text-fg-muted">{row.fullName}</span>
                            <span>{row.details}</span>
                        </div>
                        {
                            row.kind === 'pack' && row.rows !== null ?
                                <EpisodeTable
                                    infoHash={entry.infoHash}
                                    rows={row.rows}
                                    pending={pending}
                                    initializing={preparing}
                                    onPlayEpisode={(episode) => handlers.onPlayEpisode(entry, episode)}
                                    onToggle={toggle}
                                    onReveal={handlers.onReveal}
                                />
                                :
                                null
                        }
                        {
                            files === null ?
                                (entry.state === 'waiting' ? null : <div className="text-xs text-fg-subtle">Reading the files...</div>)
                                :
                                otherFiles.length > 0 ?
                                    <div className="flex flex-col">
                                        {
                                            row.kind === 'pack' ?
                                                <div className="py-1.5 text-[0.6875rem] font-extrabold uppercase tracking-wider text-fg-subtle">Other files</div>
                                                :
                                                null
                                        }
                                        <div className="divide-y divide-line border-t border-line">
                                            {otherFiles.map((file) => (
                                                <FileRow
                                                    key={file.index}
                                                    infoHash={entry.infoHash}
                                                    file={file}
                                                    busy={pending.includes(file.index)}
                                                    initializing={preparing}
                                                    onPlayFile={handlers.onPlayFile}
                                                    onToggle={toggle}
                                                    onReveal={handlers.onReveal}
                                                />
                                            ))}
                                        </div>
                                    </div>
                                    :
                                    null
                        }
                    </div>
                    :
                    null
            }
        </div>
    );
};

export default CacheRow;
