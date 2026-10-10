// Copyright (C) 2017-2026 Smart code 203358507

/**
 * "On this device" - the cached copies of the title you are looking at, shown
 * above the addon streams.
 *
 * The point is that a file already on disk should be the obvious thing to
 * press, not something you have to remember and go find on the Cache page. Each
 * row states what the addon list cannot: how much of it is actually here, and
 * where you stopped watching (the library's own timeOffset, the same number
 * that drives Continue Watching).
 *
 * Every number on a row is the viewed episode's FILE (its length, its
 * downloaded bytes, "Downloaded" only when that file is complete), never the
 * season pack it sits in: see cachedEpisodeMatch.js.
 */

import React from 'react';
import { Play, HardDrive, FolderOpen } from 'lucide-react';
import { useNavigate } from 'react-router';
import { useCore } from 'rillio/core';
import { openModal } from 'rillio/common/modalEvents';
import { Button } from 'rillio/components/ui/button';
import { cn } from 'rillio/components/ui/cn';
import { playerDeepLink } from 'rillio/common/cacheMetadata';
import toPath from 'rillio/router/toPath';
import type { CachedStream } from './useCachedStreams';

const GB = 1024 ** 3;
const MB = 1024 ** 2;
const formatBytes = (bytes: number): string => {
    if (bytes >= GB) return `${(bytes / GB).toFixed(2)} GB`;
    if (bytes >= MB) return `${(bytes / MB).toFixed(0)} MB`;
    return `${Math.max(0, Math.round(bytes / 1024))} KB`;
};

// "1h 12m in" / "6m in" - a position, not a duration, so it reads as a place to
// resume rather than a runtime.
const formatOffset = (ms: number): string => {
    const total = Math.floor(ms / 1000);
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    if (h > 0) return `${h}h ${m}m in`;
    return `${Math.max(1, m)}m in`;
};

type Props = {
    streams: CachedStream[],
    // The library item for this title, when it has one: carries the resume
    // position and which video it belongs to.
    libraryItem?: {
        state?: { timeOffset?: number, duration?: number, video_id?: string },
    } | null,
    videoId?: string | null,
};

const CachedStreams = ({ streams, libraryItem, videoId }: Props) => {
    const core = useCore();
    const navigate = useNavigate();

    // The resume position only belongs to a row when the library's progress is
    // for the SAME video (a series' library item tracks one episode at a time).
    const resumeMs = React.useMemo(() => {
        const state = libraryItem?.state;
        if (!state || typeof state.timeOffset !== 'number' || state.timeOffset <= 0) return null;
        if (typeof videoId === 'string' && videoId.length > 0 && typeof state.video_id === 'string' && state.video_id !== videoId) {
            return null;
        }
        return state.timeOffset;
    }, [libraryItem, videoId]);
    const duration = libraryItem?.state?.duration ?? 0;

    // Each entry is already ONE file: the viewed episode's file inside its
    // torrent, or a movie's playable file (see cachedEpisodeMatch.js).
    const play = React.useCallback((entry: CachedStream) => {
        // An addon listed exactly this file for this episode: its own deep
        // link is the one a click on that addon stream would follow, transport
        // urls and all, and the engine already holds the bytes.
        if (typeof entry.playHref === 'string') {
            navigate(toPath(entry.playHref));
            return;
        }
        core.transport.encodeStream({
            name: entry.name,
            description: '',
            infoHash: entry.infoHash,
            fileIdx: entry.fileIdx,
        })
            .then((encoded: unknown) => {
                if (typeof encoded !== 'string') {
                    console.error('CachedStreams: the core could not encode a stream for', entry.infoHash);
                    return;
                }
                navigate(playerDeepLink(encoded, entry.meta));
            })
            .catch((error: unknown) => console.error('CachedStreams: encoding the stream failed', error));
    }, [core, navigate]);

    if (streams.length === 0) {
        return null;
    }

    // Bare tiles, no wrapper: these are rendered INSIDE the curated carousel
    // (CuratedStreams' `leading`) so an on-device copy sits with the other
    // sources rather than in a band of its own. Set apart by the accent ring
    // and the wider right margin on the last one.
    return (
        <>
            {streams.map((entry, index) => {
                const complete = entry.total > 0 && entry.downloaded >= entry.total;
                const pct = entry.total > 0 ? Math.min(100, Math.round((entry.downloaded / entry.total) * 100)) : 0;
                const playable = entry.fileIdx !== undefined;
                return (
                    <Button
                        key={entry.infoHash}
                        variant="ghost"
                        onClick={playable ? () => play(entry) : undefined}
                        title={
                            playable ?
                                (complete ? 'Play from this device' : 'Play (streams while it downloads)')
                                :
                                'This torrent holds several videos; open it in Cache to pick one'
                        }
                        className={cn(
                            // Sized to its content (at least a carousel tile's
                            // width): a fixed w-44 squeezed the header row, and
                            // the global `word-break: break-word` then split
                            // "Downloaded" mid-word onto two lines.
                            'group flex h-auto w-max min-w-44 max-w-64 shrink-0 flex-col items-stretch justify-start gap-1 whitespace-normal rounded-xl px-3.5 py-3 text-left font-normal',
                            'bg-accent/10 ring-1 ring-inset ring-accent/30 hover:bg-accent/15',
                            // Breathing room before the addon tiles start.
                            index === streams.length - 1 && 'mr-4',
                        )}
                    >
                        <div className="flex items-center gap-1.5">
                            <HardDrive className="size-3.5 shrink-0 text-accent" />
                            <span className="shrink-0 whitespace-nowrap text-sm font-semibold text-accent">
                                {complete ? 'Downloaded' : `${pct}%`}
                            </span>
                            {
                                playable ?
                                    <Play className="size-3.5 shrink-0 text-accent" />
                                    :
                                    null
                            }
                            {/* Nested control, same idiom as the carousel's
                                download-to-cache button: it must not trigger the
                                card's own play. */}
                            <Button
                                variant="ghost"
                                tabIndex={-1}
                                title="Open in Cache"
                                onClick={(event: React.MouseEvent) => {
                                    event.preventDefault();
                                    event.stopPropagation();
                                    openModal('cached');
                                }}
                                className="ml-auto size-6 shrink-0 p-0 text-fg-subtle opacity-0 transition hover:bg-white/10 hover:text-fg group-hover:opacity-100"
                            >
                                <FolderOpen className="size-3.5" />
                            </Button>
                        </div>
                        <div className="truncate text-xs text-fg-muted">
                            {
                                resumeMs !== null ?
                                    `Stopped ${formatOffset(resumeMs)}`
                                    :
                                    complete ? 'On this device' : 'Downloading now'
                            }
                        </div>
                        <div className="flex flex-wrap items-center gap-x-2 whitespace-nowrap text-[11px] tabular-nums text-fg-subtle">
                            <span>{complete ? formatBytes(entry.downloaded) : `${formatBytes(entry.downloaded)} of ${formatBytes(entry.total)}`}</span>
                            {
                                resumeMs !== null && duration > 0 ?
                                    <span>{formatOffset(duration).replace(' in', '')}</span>
                                    :
                                    null
                            }
                        </div>
                        {
                            !complete && entry.total > 0 ?
                                <div className="mt-1 h-0.5 w-full overflow-hidden rounded-full bg-black/30">
                                    <div className="h-full rounded-full bg-accent transition-[width] duration-700" style={{ width: `${pct}%` }} />
                                </div>
                                :
                                null
                        }
                    </Button>
                );
            })}
        </>
    );
};

export default CachedStreams;
