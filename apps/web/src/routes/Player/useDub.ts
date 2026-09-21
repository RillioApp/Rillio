// Copyright (C) 2017-2026 Smart code 203358507

import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useToast } from 'rillio/common';
import { getTauri } from 'rillio/common/Platform/shell/isShell';
import { getItem, removeItem, setItem } from 'rillio/common/profileStorage';
import type { DubScript } from './useSubtitles';

// The viewer's choice per stream, so a title they dubbed comes back dubbed
// (the shell keeps the audio it made; the choice lives here, web-only).
const CHOICE_KEY = 'rillio.dub.on.';
const streamKey = (url: string): string => {
    let hash = 5381;
    for (let i = 0; i < url.length; i++) hash = ((hash * 33) ^ url.charCodeAt(i)) >>> 0;
    return CHOICE_KEY + hash.toString(16);
};
const readChoice = (url: string): boolean => {
    try {
        return getItem(streamKey(url)) === 'true';
    } catch (error) {
        console.error('useDub: failed to read the choice', error);
        return false;
    }
};
const writeChoice = (url: string, on: boolean): void => {
    try {
        if (on) setItem(streamKey(url), 'true'); else removeItem(streamKey(url));
    } catch (error) {
        console.error('useDub: failed to persist the choice', error);
    }
};

// The AI dub (docs/dubbing/stage1-pipeline): the shell dubs the stream ahead
// of playback into an English audio track it adds to the player itself
// (`dub_select`); the web only starts, stops and shows the phase. The one-time
// weights pack (`pack_*`) is the opt-in: its size is on the row before any
// download starts.

// What the shell sends on `dub` (src-tauri dub.rs).
type DubEvent = { kind: 'status', url: string, state: 'preparing' | 'running' | 'done' | 'failed', detail: string | null, produced: number, windows: number, aheadS: number, waiting: boolean };
// `pack-progress` (src-tauri packs.rs), snake_case as serialized.
type PackProgress = { file: string, done: number, total: number, bytes_done: number, bytes_total: number };
type PackStatus = { version: number, installed: boolean, bytes_total: number, bytes_present: number };

export type DubState = {
    supported: boolean,
    state: 'idle' | 'needs-pack' | 'downloading' | 'preparing' | 'running' | 'done' | 'error',
    // 0..1 while downloading the pack.
    progress: number | null,
    // The failure (error).
    detail: string | null,
    // Bytes the pack download would fetch (needs-pack).
    packBytes: number | null,
    // Seconds of dub ready ahead of the playhead (preparing / running).
    aheadS: number,
    // The player is holding on audio not made yet (buffering the dub).
    waiting: boolean,
};

// Where the dub's lines come from: the recognizer and the translator ("AI"),
// or the external subtitle track the viewer has loaded. The subtitles win on
// accuracy where they exist (measured 2026-09-21: the recognizers mishear
// names and homophones the subtitles simply have), so the viewer chooses.
export type DubSource = 'ai' | 'subtitles';
const SOURCE_KEY = 'rillio.dub.source';
// The dub speaks English today, so only an English track can be its lines.
const DUB_LANGUAGE_PREFIX = 'en';
const readSource = (): DubSource => {
    try {
        return getItem(SOURCE_KEY) === 'subtitles' ? 'subtitles' : 'ai';
    } catch (error) {
        console.error('useDub: failed to read the translation source', error);
        return 'ai';
    }
};

// The slice of the video controller the dub uses (`useVideo`).
type Args = {
    // The loaded external subtitle track with its lines, and the delay the
    // viewer set on it (`useSubtitles`).
    script: DubScript | null,
    scriptDelay: number,
    video: {
        state: {
            stream: { url?: unknown } | null,
            audioTracks: AudioTrack[],
            selectedAudioTrackId: string | null,
        },
        // Selects an audio track by id (mpv `aid`), without touching the
        // library's remembered choice: the dub track's id means nothing next time.
        setAudioTrack: (id: string) => void,
    },
    // The title's name and synopsis: the recognizer and the translator read
    // it, so ambiguous words resolve to the story's ("relic", not "alien").
    about: string | null,
};

const useDub = ({ video, about, script, scriptDelay }: Args) => {
    const { t } = useTranslation();
    const toast = useToast();
    const videoRef = useRef(video);
    videoRef.current = video;
    const aboutRef = useRef(about);
    aboutRef.current = about;
    const [source, setSourceState] = useState<DubSource>(readSource);
    // The subtitles can be the source only when an English external track is loaded.
    const scriptUsable = script !== null && (script.lang ?? '').toLowerCase().startsWith(DUB_LANGUAGE_PREFIX);
    const lines = source === 'subtitles' && scriptUsable && script !== null ?
        script.lines.map((line) => ({ startMs: line.startMs + scriptDelay, endMs: line.endMs + scriptDelay, text: line.text })) :
        null;
    const linesRef = useRef(lines);
    linesRef.current = lines;
    // What the running dub was started with: a change of it is another dub.
    const sourceId = lines === null ? 'ai' : `subtitles:${script?.trackId}:${scriptDelay}`;
    const startedWith = useRef<string | null>(null);
    // The player's track of a dub that another source replaced.
    const staleTrackId = useRef<string | null>(null);
    const sourceIdRef = useRef(sourceId);
    sourceIdRef.current = sourceId;
    const [dub, setDub] = useState<DubState>(() => ({
        supported: Boolean(getTauri()?.core?.invoke),
        state: 'idle',
        progress: null,
        detail: null,
        packBytes: null,
        aheadS: 0,
        waiting: false,
    }));
    const dubRef = useRef(dub);
    dubRef.current = dub;
    // A dub THIS hook started (a status event alone never arms it: a worker
    // resumed by the shell is not stopped by a stale selection here).
    const armed = useRef(false);
    // The audio track selected when the row was pressed: the shell selects the
    // dub track a moment later, and only a selection that is NEITHER counts as
    // the viewer switching away (which stops the worker).
    const pendingFrom = useRef<string | null>(null);
    // The track switch is requested once per start.
    const switchRequested = useRef(false);

    const fail = useCallback((error: unknown) => {
        armed.current = false;
        switchRequested.current = false;
        setDub((current) => ({ ...current, state: 'error', progress: null, detail: String(error) }));
        toast.show({ type: 'error', title: t('AUDIO_DUB_FAILED'), message: String(error), timeout: 6000 });
    }, [t, toast]);

    // The pack decides the row's first state.
    useEffect(() => {
        const tauri = getTauri();
        if (!tauri?.core?.invoke) return;
        let cancelled = false;
        tauri.core.invoke('pack_status').then((status: PackStatus) => {
            if (cancelled || status.installed) return;
            setDub((current) => current.state === 'idle' ?
                { ...current, state: 'needs-pack', packBytes: status.bytes_total - status.bytes_present } :
                current);
        }).catch((error: unknown) => {
            console.error('pack_status failed', error);
        });
        return () => {
            cancelled = true;
        };
    }, []);

    const stop = useCallback(() => {
        const tauri = getTauri();
        const { state } = dubRef.current;
        if (tauri?.core?.invoke && (state === 'preparing' || state === 'running')) {
            tauri.core.invoke('dub_stop').catch((error: unknown) => {
                console.error('dub_stop failed', error);
            });
        }
        armed.current = false;
        pendingFrom.current = null;
        switchRequested.current = false;
        setDub((current) => (current.state === 'preparing' || current.state === 'running' || current.state === 'done') ?
            { ...current, state: 'idle', progress: null, detail: null, aheadS: 0, waiting: false } :
            current);
    }, []);

    // Switch the player to the dub track at the pick: from here the player
    // buffers on audio not made yet and plays as it arrives. The first time
    // the shell adds the track (`dub_select`); once it exists in the player,
    // it is selected like any other track.
    const switchToDub = useCallback(() => {
        const tauri = getTauri();
        if (!tauri?.core?.invoke || switchRequested.current) return;
        switchRequested.current = true;
        pendingFrom.current = videoRef.current.state.selectedAudioTrackId;
        // Never the track of a dub that was just replaced (another source): it
        // is being removed and may still be listed for a moment.
        const existing = videoRef.current.state.audioTracks.find((track: AudioTrack) => track.generated && track.id !== staleTrackId.current);
        if (existing) {
            videoRef.current.setAudioTrack(existing.id);
            return;
        }
        tauri.core.invoke('dub_select').catch(fail);
    }, [fail]);

    const start = useCallback(() => {
        const tauri = getTauri();
        const url = videoRef.current.state.stream?.url;
        if (!tauri?.core?.invoke || typeof url !== 'string') return;
        armed.current = true;
        switchRequested.current = false;
        pendingFrom.current = videoRef.current.state.selectedAudioTrackId;
        writeChoice(url, true);
        setDub((current) => ({ ...current, state: 'preparing', progress: null, detail: null, aheadS: 0, waiting: false }));
        startedWith.current = sourceIdRef.current;
        tauri.core.invoke('dub_start', { url, about: aboutRef.current, lines: linesRef.current }).then(switchToDub).catch(fail);
    }, [fail, switchToDub]);

    const install = useCallback(() => {
        const tauri = getTauri();
        if (!tauri?.core?.invoke) return;
        setDub((current) => ({ ...current, state: 'downloading', progress: 0, detail: null }));
        tauri.core.invoke('pack_install')
            .then(() => {
                setDub((current) => ({ ...current, state: 'idle', progress: null, packBytes: null }));
                start();
            })
            .catch(fail);
    }, [fail, start]);

    // The row behaves like a track: pick it to install, start, or come back
    // to the dub track; picking any other track stops the worker.
    const select = useCallback(() => {
        const { state } = dubRef.current;
        if (state === 'needs-pack') {
            install();
        } else if (state === 'idle' || state === 'error' || state === 'done') {
            start();
        } else if (state === 'running' || state === 'preparing') {
            // Picked while its track is not the one playing (a second pick,
            // or a run the shell started on its own): the pick is the choice.
            armed.current = true;
            switchRequested.current = false;
            switchToDub();
        }
    }, [install, start, switchToDub]);

    // The shell's status for the CURRENT stream, and the pack download.
    useEffect(() => {
        const tauri = getTauri();
        if (!tauri?.event?.listen) return;
        const unlisteners: (() => void)[] = [];
        let cancelled = false;
        const keep = (promise: Promise<() => void>) => promise.then((fn) => {
            if (cancelled) fn(); else unlisteners.push(fn);
        });
        keep(tauri.event.listen('dub', (event: { payload: DubEvent }) => {
            const payload = event.payload;
            const url = videoRef.current.state.stream?.url;
            if (typeof url !== 'string' || payload.url !== url) return;
            const state = payload.state;
            if (state === 'failed') {
                fail(payload.detail ?? '');
                return;
            }
            setDub((current) => ({ ...current, state, progress: null, detail: null, aheadS: payload.aheadS, waiting: payload.waiting }));
        }));
        keep(tauri.event.listen('pack-progress', (event: { payload: PackProgress }) => {
            const { bytes_done, bytes_total } = event.payload;
            setDub((current) => current.state === 'downloading' ?
                { ...current, progress: bytes_total > 0 ? bytes_done / bytes_total : 0 } :
                current);
        }));
        return () => {
            cancelled = true;
            unlisteners.forEach((fn) => fn());
        };
    }, [fail]);

    // A new stream, and leaving the player: the worker must not outlive its stream.
    useEffect(() => {
        return () => stop();
    }, [video.state.stream, stop]);

    // A title the viewer dubbed before comes back dubbed: the shell kept the
    // audio it made, so it plays at once where it exists and fills the rest.
    const streamUrl = video.state.stream?.url;
    useEffect(() => {
        if (typeof streamUrl !== 'string' || !dub.supported) return;
        if (dub.state !== 'idle' || !readChoice(streamUrl)) return;
        // Once the player has reported its tracks (the switch needs the stream loaded).
        if (video.state.audioTracks.length === 0) return;
        start();
    }, [streamUrl, dub.supported, dub.state, video.state.audioTracks.length, start]);

    // Picking another audio track while dubbing stops the worker (a track
    // nobody hears is not worth the GPU). The dub track is the one the shell
    // added (`generated`); until it is observed selected, the pre-press track
    // still reads as "waiting".
    const dubTrackId = video.state.audioTracks.find((track: AudioTrack) => track.generated && track.id !== staleTrackId.current)?.id ?? null;
    const selected = dubTrackId !== null && video.state.selectedAudioTrackId === dubTrackId;
    useEffect(() => {
        if (!armed.current || (dub.state !== 'preparing' && dub.state !== 'running')) return;
        if (selected) {
            pendingFrom.current = null;
            return;
        }
        if (pendingFrom.current !== null && video.state.selectedAudioTrackId === pendingFrom.current) return;
        // The viewer chose another track: the choice for this title is off.
        const url = videoRef.current.state.stream?.url;
        if (typeof url === 'string') writeChoice(url, false);
        stop();
    }, [dub.state, selected, stop, video.state.selectedAudioTrackId]);

    const setSource = useCallback((next: DubSource) => {
        try {
            setItem(SOURCE_KEY, next);
        } catch (error) {
            console.error('useDub: failed to persist the translation source', error);
        }
        setSourceState(next);
    }, []);

    // Another source (the choice, another loaded track, a new delay) is another
    // dub: the running one stops, its track leaves the player, and the choice
    // for this title starts the new one through the resume effect above.
    useEffect(() => {
        const active = dub.state === 'preparing' || dub.state === 'running' || dub.state === 'done';
        if (!active || startedWith.current === null || startedWith.current === sourceId) return;
        const tauri = getTauri();
        const stale = videoRef.current.state.audioTracks.find((track: AudioTrack) => track.generated);
        stop();
        startedWith.current = null;
        staleTrackId.current = stale?.id ?? null;
        const id = Number.parseInt(stale?.id ?? '', 10);
        if (tauri?.core?.invoke && Number.isInteger(id)) {
            tauri.core.invoke('dub_forget_track', { id }).catch((error: unknown) => {
                console.error('dub_forget_track failed', error);
            });
        }
    }, [dub.state, sourceId, stop]);

    // The row reads as the chosen track from the pick onwards; `dubPlaying`
    // says whether its audio is the one heard yet.
    const chosen = selected || (armed.current && (dub.state === 'preparing' || dub.state === 'running' || dub.state === 'done'));
    return {
        dub,
        dubChosen: chosen,
        dubPlaying: selected,
        onDubSelect: select,
        // The translation source: the choice, whether the subtitles can be it
        // right now, and which one the dub actually uses.
        dubSource: source,
        dubSourceInUse: (lines === null ? 'ai' : 'subtitles') as DubSource,
        dubSubtitlesUsable: scriptUsable,
        onDubSourceChange: setSource,
    };
};

export default useDub;
