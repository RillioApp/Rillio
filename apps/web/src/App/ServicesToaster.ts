// Copyright (C) 2017-2023 Smart code 203358507

import React from 'react';
import { useCore } from 'rillio/core';
import { useToast, useFileDrop } from 'rillio/common';
import { getTauri } from 'rillio/common/Platform/shell/isShell';
import { toUpdateFailure } from 'rillio/common/Platform/shell/updateFailure';

const ServicesToaster = () => {
    const core = useCore();
    const toast = useToast();
    const filedrop = useFileDrop();

    React.useEffect(() => {
        const onCoreEvent = (name: string, data: any) => {
            switch (name) {
                case 'TorrentParsed': {
                    toast.show({
                        type: 'success',
                        title: 'Torrent file parsed',
                        timeout: 4000
                    });
                    break;
                }
                case 'MagnetParsed': {
                    toast.show({
                        type: 'info',
                        title: 'Magnet link parsed',
                        timeout: 4000
                    });
                    break;
                }
                case 'PlayingOnDevice': {
                    toast.show({
                        type: 'success',
                        title: `Stream opened in ${data.device}`,
                        timeout: 4000
                    });
                    break;
                }
            }
        };
        const onCoreError = (source: any, error: any) => {
            if (source.event === 'UserPulledFromAPI' && source.args.uid === null) return;
            if (source.event === 'LibrarySyncWithAPIPlanned' && source.args.uid === null) return;
            if (error.type === 'Other' && error.code === 3 && source.event === 'AddonInstalled' && source.args.transport_url.startsWith('https://www.strem.io/trakt/addon')) return;

            toast.show({
                type: 'error',
                title: source.event,
                message: error.message,
                timeout: 4000,
                dataset: {
                    type: 'CoreEvent'
                }
            });
        };
        const onFileDrop = (file: File, _buffer: ArrayBuffer, supported: boolean) => {
            if (!supported) {
                toast.show({
                    type: 'error',
                    title: 'Unsupported file',
                    message: file.name,
                    timeout: 4000
                });
            }
        };
        core.on('event', onCoreEvent);
        core.on('error', onCoreError);
        filedrop.on('*', onFileDrop);
        return () => {
            core.off('event', onCoreEvent);
            core.off('error', onCoreError);
            filedrop.off('*', onFileDrop);
        };
    }, []);

    // Desktop self-updater: the native shell checks GitHub Releases on every
    // launch and emits `update-available` with the version. Surface it as a
    // clickable toast (click installs + restarts). Reappears each startup until
    // the update is taken. No-op outside the Tauri shell.
    React.useEffect(() => {
        const TAURI = getTauri();
        if (!TAURI?.event?.listen || !TAURI?.core?.invoke) return;
        let unlisten: (() => void) | undefined;
        let cancelled = false;
        TAURI.event.listen('update-available', (event: any) => {
            const version = typeof event?.payload === 'string' ? event.payload : null;
            toast.show({
                type: 'info',
                icon: 'download',
                title: version ? `Rillio ${version} is available` : 'An update is available',
                message: 'Click to install and restart',
                timeout: 60000,
                onSelect: () => {
                    // The shell owns the whole install UX: it hides this window and
                    // shows the detached update splash (update_window.rs), so the web
                    // side renders nothing. On failure the shell re-shows the window.
                    // The shell rejects with an UpdateFailure: the toast gets its
                    // plain sentence (the update window offers Try again and Copy
                    // error). An 'alert' toast, not 'error': a failed download is
                    // a warning to act on, and nothing about it is red.
                    TAURI.core.invoke('install_update').catch((e: unknown) => {
                        toast.show({ type: 'alert', title: 'Couldn\'t update Rillio', message: toUpdateFailure(e).summary, timeout: 5000 });
                    });
                },
            });
        }).then((un: () => void) => { if (cancelled) un(); else unlisten = un; }).catch(() => { /* not in shell */ });
        return () => { cancelled = true; if (typeof unlisten === 'function') unlisten(); };
    }, []);

    // The native shell emits `streaming-server-error` if the in-process streaming
    // server fails to start (e.g. port 11470 already held by another instance).
    // Surface it so a dead backend is not silent. No-op outside the Tauri shell.
    React.useEffect(() => {
        const TAURI = getTauri();
        if (!TAURI?.event?.listen) return;
        let unlisten: (() => void) | undefined;
        let cancelled = false;
        TAURI.event.listen('streaming-server-error', (event: any) => {
            const detail = typeof event?.payload === 'string' ? event.payload : null;
            toast.show({
                type: 'error',
                title: 'Streaming server failed to start',
                message: detail || 'Streams may not load. Another instance may already be using the port.',
                timeout: 10000,
            });
        }).then((un: () => void) => { if (cancelled) un(); else unlisten = un; }).catch(() => { /* not in shell */ });
        return () => { cancelled = true; if (typeof unlisten === 'function') unlisten(); };
    }, []);

    return null;
};

export default ServicesToaster;
