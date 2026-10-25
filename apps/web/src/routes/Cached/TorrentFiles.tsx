// Copyright (C) 2017-2026 Smart code 203358507

/**
 * The file browser (the Cache page's advanced view): everything inside a
 * cached torrent, not just the file being streamed. A torrent is a directory,
 * and until now the app pretended it was a single movie - so extras, bundled
 * subtitles, or the second half of a double feature were invisible and
 * unreachable even though the swarm was right there.
 *
 * Each file can be pulled into the download selection (or dropped out of it),
 * any video file can be played straight from here, and any file can be shown
 * in the file manager. FileRow and useFileToggle are shared with the Cache
 * window's expanded rows (CacheRow), which list a torrent's files the same way.
 */

import React, { useCallback, useEffect, useState } from 'react';
import { Play, Download, Loader2, Check, X, FolderOpen } from 'lucide-react';
import { useProfile } from 'rillio/common';
import { IconButton, cn } from 'rillio/components/ui';
import { serverFetch } from 'rillio/common/serverFetch';

export type TorrentFile = {
    index: number,
    path: string,
    length: number,
    downloaded: number,
    selected: boolean,
    video: boolean,
};

const { formatSize, formatOf } = require('./cacheRowModel') as {
    formatSize: (bytes: number) => string,
    formatOf: (done: number, total: number) => string,
};

/**
 * Selection changes for one torrent's files: the indices in flight (so a click
 * cannot be double-fired while the engine applies the last one) and the toggle
 * itself, which re-reads the list when it settles.
 */
export const useFileToggle = (
    infoHash: string,
    onSetFileSelected: (infoHash: string, fileIdx: number, selected: boolean) => Promise<void> | void,
    reload: () => void,
) => {
    const [pending, setPending] = useState<number[]>([]);
    const toggle = useCallback((file: TorrentFile) => {
        setPending((current) => [...current, file.index]);
        Promise.resolve(onSetFileSelected(infoHash, file.index, !file.selected))
            .finally(() => {
                setPending((current) => current.filter((i) => i !== file.index));
                reload();
            });
    }, [infoHash, onSetFileSelected, reload]);
    return { pending, toggle };
};

/** "Complete", "6 of 15 MB", "Not downloaded" for one file. */
export const fileState = (file: TorrentFile): string => {
    if (file.length > 0 && file.downloaded >= file.length) return 'Complete';
    if (!file.selected) return file.downloaded > 0 ? `Not downloading · ${formatSize(file.downloaded)} kept` : 'Not downloaded';
    return file.downloaded > 0 ? formatOf(file.downloaded, file.length) : 'Waiting';
};

/** A thin progress line for a file part-way on disk, or null. */
export const FileProgress = ({ file, className }: { file: TorrentFile, className?: string }) => {
    if (!(file.downloaded > 0) || file.length <= 0 || file.downloaded >= file.length) return null;
    return (
        <div className={cn('h-0.5 w-16 shrink-0 overflow-hidden rounded-full bg-surface-hover', className)}>
            <div className="h-full rounded-full bg-fg-subtle" style={{ width: `${Math.round((file.downloaded / file.length) * 100)}%` }} />
        </div>
    );
};

/** The per-file controls: play (a downloaded video), show in folder, download / stop. */
export const FileActions = ({ infoHash, file, busy, initializing, onPlayFile, onToggle, onReveal }: {
    infoHash: string,
    file: TorrentFile,
    busy: boolean,
    initializing: boolean,
    onPlayFile: (infoHash: string, fileIdx: number, name: string) => void,
    onToggle: (file: TorrentFile) => void,
    onReveal?: ((infoHash: string, fileIdx?: number) => void) | null,
}) => (
    <div className="flex shrink-0 items-center">
        {
            // Play only what is actually playable AND actually here:
            // offering Play on a file nobody selected would start a
            // download behind a spinner with no explanation.
            file.video && file.selected ?
                <IconButton
                    onClick={() => onPlayFile(infoHash, file.index, file.path)}
                    title="Play this file"
                    className="size-8 shrink-0 text-accent hover:brightness-110"
                >
                    <Play className="size-4" />
                </IconButton>
                :
                null
        }
        {
            onReveal ?
                <IconButton
                    onClick={() => onReveal(infoHash, file.index)}
                    title={file.downloaded > 0 ? 'Show this file in its folder' : 'Not on disk yet: shows the folder it goes in'}
                    className="size-8 shrink-0 text-fg-subtle hover:text-fg"
                >
                    <FolderOpen className="size-4" />
                </IconButton>
                :
                null
        }
        <IconButton
            onClick={() => onToggle(file)}
            disabled={busy || initializing}
            title={
                initializing ? 'Available once this torrent finishes preparing' :
                    file.selected ? 'Stop downloading this file' : 'Download this file too'
            }
            className={cn(
                'group/toggle size-8 shrink-0',
                file.selected ? 'text-fg-muted hover:text-danger' : 'text-fg-subtle hover:text-accent',
            )}
        >
            {
                busy ? <Loader2 className="size-4 animate-spin" /> :
                    file.selected ?
                        // A check states what IS; hovering must state what
                        // the click DOES, and this click removes the file
                        // from the download.
                        <>
                            <Check className="size-4 group-hover/toggle:hidden" />
                            <X className="hidden size-4 group-hover/toggle:block" />
                        </>
                        :
                        <Download className="size-4" />
            }
        </IconButton>
    </div>
);

/** One file: its path, size and state, and its controls. */
export const FileRow = ({ infoHash, file, busy, initializing, onPlayFile, onToggle, onReveal, className }: {
    infoHash: string,
    file: TorrentFile,
    busy: boolean,
    initializing: boolean,
    onPlayFile: (infoHash: string, fileIdx: number, name: string) => void,
    onToggle: (file: TorrentFile) => void,
    onReveal?: ((infoHash: string, fileIdx?: number) => void) | null,
    className?: string,
}) => (
    <div className={cn('flex items-center gap-3 py-2.5', className)}>
        <div className="min-w-0 flex-1">
            <div className="truncate text-xs text-fg-muted" title={file.path}>{file.path}</div>
            <div className="mt-1 flex items-center gap-2 text-[0.6875rem] tabular-nums text-fg-subtle">
                <span className="text-[0.6875rem]">{formatSize(file.length)}</span>
                <span className="text-[0.6875rem]">·</span>
                <span className="text-[0.6875rem]">{fileState(file)}</span>
                <FileProgress file={file} />
            </div>
        </div>
        <FileActions
            infoHash={infoHash}
            file={file}
            busy={busy}
            initializing={initializing}
            onPlayFile={onPlayFile}
            onToggle={onToggle}
            onReveal={onReveal}
        />
    </div>
);

type Props = {
    infoHash: string,
    // librqbit hard-refuses a selection change while a torrent is hash-checking
    // ("can't update initializing torrent"), exactly as it refuses pause there.
    // Offering the control anyway would produce a toast-worthy 409 for a user
    // who did nothing wrong, so it is disabled with the reason instead.
    initializing: boolean,
    onPlayFile: (infoHash: string, fileIdx: number, name: string) => void,
    onSetFileSelected: (infoHash: string, fileIdx: number, selected: boolean) => Promise<void> | void,
    onReveal?: ((infoHash: string, fileIdx?: number) => void) | null,
};

const TorrentFiles = ({ infoHash, initializing, onPlayFile, onSetFileSelected, onReveal }: Props) => {
    const profile = useProfile();
    const serverUrl = profile.settings.streamingServerUrl;
    const [files, setFiles] = useState<TorrentFile[] | null>(null);
    const [failed, setFailed] = useState(false);

    const load = useCallback(() => {
        if (typeof serverUrl !== 'string') return;
        serverFetch(new URL(`cache/files/${infoHash}`, serverUrl))
            .then((resp) => {
                if (!resp.ok) throw new Error(`cache/files responded ${resp.status}`);
                return resp.json();
            })
            .then((list: TorrentFile[]) => { setFiles(list); setFailed(false); })
            .catch((error) => {
                console.error('TorrentFiles: listing the torrent failed', error);
                setFailed(true);
            });
    }, [serverUrl, infoHash]);

    // Poll while open: a file the user just added starts filling immediately,
    // and a frozen byte count reads as a stalled download.
    useEffect(() => {
        load();
        const interval = setInterval(load, 3000);
        return () => clearInterval(interval);
    }, [load]);

    const { pending, toggle } = useFileToggle(infoHash, onSetFileSelected, load);

    if (failed) {
        return <div className="px-6 pb-4 text-xs text-fg-muted">Could not read this torrent&apos;s contents.</div>;
    }
    if (files === null) {
        return <div className="px-6 pb-4 text-xs text-fg-muted">Loading files...</div>;
    }

    return (
        // Nearly opaque, not a light tint: under the hero this panel sits on top of
        // backdrop ARTWORK, and a 20% scrim left the filenames fighting Rick's face.
        <div className="divide-y divide-white/5 border-t border-white/10 bg-black/75 backdrop-blur-md">
            {files.map((file) => (
                <FileRow
                    key={file.index}
                    infoHash={infoHash}
                    file={file}
                    busy={pending.includes(file.index)}
                    initializing={initializing}
                    onPlayFile={onPlayFile}
                    onToggle={toggle}
                    onReveal={onReveal}
                    className="pl-10 pr-6"
                />
            ))}
        </div>
    );
};

export default TorrentFiles;
