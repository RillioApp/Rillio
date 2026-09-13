// Copyright (C) 2017-2026 Smart code 203358507

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { animate } from 'motion';
import { useTranslation } from 'react-i18next';
import { CONSTANTS, onFileDrop, onShortcut, useToast } from 'rillio/common';
import { getTauri } from 'rillio/common/Platform/shell/isShell';
import { pickSubtitlesTrack } from './smartTracks';

// What the shell's `subtitles_autosync` command answers (src-tauri autosync.rs).
type AutoSyncOutcome =
    | { kind: 'synced', delayMs: number, confidence: number, margin: number, speechFraction: number }
    | { kind: 'no-dialogue', speechFraction: number }
    | { kind: 'no-cues' }
    | { kind: 'ambiguous', delayMs: number, confidence: number, margin: number };

const formatDelaySeconds = (ms: number) => `${ms >= 0 ? '+' : ''}${(ms / 1000).toFixed(2)}`;

// What the shell's whisper worker sends on `subtitles-generate` (src-tauri transcribe.rs).
type GeneratedSegment = { startMs: number, endMs: number, text: string };
type GenerateEvent =
    | { kind: 'status', url: string, state: 'downloading' | 'loading' | 'running' | 'done' | 'idle' | 'error', detail: string | null, progress: number | null }
    | { kind: 'segments', url: string, language: string | null, segments: GeneratedSegment[] };

const GENERATED_TRACK_ID = 'GENERATED';

const vttTime = (ms: number) => {
    const total = Math.max(0, Math.round(ms));
    const h = Math.floor(total / 3600000);
    const m = Math.floor((total % 3600000) / 60000);
    const s = Math.floor((total % 60000) / 1000);
    const f = total % 1000;
    return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}.${String(f).padStart(3, '0')}`;
};

// The generated track as one VTT (the renderer re-parses the whole thing per
// batch; a film is a few thousand lines, well inside what that costs).
const toVtt = (segments: Iterable<GeneratedSegment>): string => {
    const lines = ['WEBVTT', ''];
    const sorted = [...segments].sort((a, b) => a.startMs - b.startMs);
    sorted.forEach((segment, index) => {
        // The shell times each chunk readably; across chunk edges a held line
        // may still reach into the next chunk's first line, so clamp here.
        const next = sorted[index + 1];
        let end = Math.max(segment.endMs, segment.startMs + 300);
        if (next && end > next.startMs - 80) end = Math.max(segment.startMs + 300, next.startMs - 80);
        lines.push(`${vttTime(segment.startMs)} --> ${vttTime(end)}`, segment.text, '');
    });
    return lines.join('\n');
};

const withFallbackLabels = (tracks?: SubtitleTrack[] | null): SubtitleTrack[] => {
    if (!Array.isArray(tracks)) {
        return [];
    }

    return tracks.map((track) => ({
        ...track,
        label: track.label || track.url || '',
    }));
};

const findTrackById = (tracks: SubtitleTrack[], id?: string | null) => {
    if (!id) {
        return undefined;
    }

    return tracks.find((track) => track.id === id);
};

// Language defaults go through smartTracks scoring, not find-first: find-first
// took whatever the muxer listed first in the wanted language, which for anime
// is routinely the "Signs & Songs" typesetting track, and a forced (partial)
// track anywhere beats the full-dialogue one it precedes.
const findTrackByLanguage = (tracks: SubtitleTrack[], language?: string | null) => {
    if (!language) {
        return undefined;
    }

    return pickSubtitlesTrack(tracks, language) ?? undefined;
};

// The offset (% from the bottom) the subtitles lift to while the chrome is up,
// derived from the control bar's REAL height (--player-chrome-clearance, a
// fixed rem value) against the CURRENT window height. A hardcoded percentage
// was wrong twice over: it landed at a different pixel height on every window
// size, and a value below the user's own offset made the fade change nothing.
const chromeLiftPercent = (): number => {
    const root = document.documentElement;
    const raw = getComputedStyle(root).getPropertyValue('--player-chrome-clearance').trim();
    const rem = parseFloat(getComputedStyle(root).fontSize) || 16;
    const px = raw.endsWith('rem') ? parseFloat(raw) * rem : parseFloat(raw) || 7.5 * rem;
    return Math.min(30, (px / Math.max(1, window.innerHeight)) * 100);
};

const useSubtitles = ({
    player,
    video,
    settings,
    streamStateChanged,
    menusOpen,
    closeMenus,
    closeSubtitlesMenu,
    toggleSubtitlesMenu,
    liftOffset = false,
}: UseSubtitlesArgs): UseSubtitlesResult => {
    const { t } = useTranslation();
    const toast = useToast();
    const videoRef = useRef(video);
    const settingsRef = useRef(settings);
    const defaultTrackSelected = useRef(false);
    const lastSelectedTrack = useRef<SelectedSubtitleTrack | null>(null);
    // The offset the USER means (settings default / their menu tweak / the
    // per-stream restore) - as opposed to what is currently applied, which may
    // be lifted above the control bar while the chrome is visible. Every write
    // path records intent here so the chrome fading can restore it.
    const intendedOffset = useRef<number | null>(null);
    const liftOffsetRef = useRef(liftOffset);
    liftOffsetRef.current = liftOffset;
    // The selected external track's cue intervals, as the renderer parsed
    // them (handed over with extraSubtitlesTrackLoaded): the subtitle half of
    // auto-sync. Keyed by track id so a stale list never syncs a new track;
    // the id is state as well so the menu button follows availability.
    const loadedCues = useRef<{ trackId: string, cues: [number, number][] } | null>(null);
    const [cuesTrackId, setCuesTrackId] = useState<string | null>(null);
    const [autoSyncRunning, setAutoSyncRunning] = useState(false);
    // Generated (whisper) subtitles: the shell transcribes ahead of the
    // playhead and streams lines; they accumulate here (keyed by start time,
    // so a re-sent batch never duplicates) and go to the renderer as one VTT.
    const [generate, setGenerate] = useState<SubtitlesGenerateState>(() => ({
        supported: Boolean(getTauri()?.core?.invoke),
        state: 'idle',
        progress: null,
        detail: null,
    }));
    const generateRef = useRef(generate);
    generateRef.current = generate;
    const generatedSegments = useRef<Map<number, GeneratedSegment>>(new Map());
    const generatedLang = useRef<string | null>(null);
    // Set by a fresh Generate press: select the track as soon as it has lines
    // (it does not exist before the first batch). `pendingFrom` remembers what
    // was selected at the press, so a viewer picking a different track while
    // the first lines are still coming reads as "stop", not as "waiting".
    const selectGeneratedWhenReady = useRef(false);
    const pendingFrom = useRef<{ embedded: string | null, extra: string | null }>({ embedded: null, extra: null });

    videoRef.current = video;
    settingsRef.current = settings;

    // The offset actually applied to the video right now (may be mid-tween),
    // and the running tween. Fractional values go through as-is: mpv >= 0.36
    // made sub-pos a float, and integer rounding turned the glide into ~7
    // visible one-percent jumps.
    const appliedOffset = useRef<number | null>(null);
    const offsetTween = useRef<{ stop: () => void } | null>(null);
    const writeOffset = useCallback((value: number, animated: boolean) => {
        offsetTween.current?.stop();
        offsetTween.current = null;
        const from = appliedOffset.current;
        if (!animated || from === null || from === value) {
            appliedOffset.current = value;
            videoRef.current.setSubtitlesOffset(value);
            return;
        }
        offsetTween.current = animate(from, value, {
            duration: 0.3,
            ease: 'easeOut',
            onUpdate: (v: number) => {
                appliedOffset.current = v;
                videoRef.current.setSubtitlesOffset(v);
            },
        });
    }, []);

    const applyOffset = useCallback((base: number) => {
        intendedOffset.current = base;
        writeOffset(liftOffsetRef.current ? Math.max(base, chromeLiftPercent()) : base, false);
    }, [writeOffset]);

    const streamSubtitles = useMemo(() => {
        return withFallbackLabels(player.selected?.stream.subtitles);
    }, [player.selected]);

    const externalSubtitles = useMemo(() => {
        return withFallbackLabels(player.subtitles);
    }, [player.subtitles]);

    const allTracks = useMemo(() => {
        return video.state.subtitlesTracks.concat(video.state.extraSubtitlesTracks);
    }, [video.state.subtitlesTracks, video.state.extraSubtitlesTracks]);

    const hasTracks = allTracks.length > 0;

    const applySubtitleStyle = useCallback(() => {
        const currentSettings = settingsRef.current;
        const currentVideo = videoRef.current;

        currentVideo.setSubtitlesSize(currentSettings.subtitlesSize);
        applyOffset(currentSettings.subtitlesOffset);
        currentVideo.setSubtitlesTextColor(currentSettings.subtitlesTextColor);
        currentVideo.setSubtitlesBackgroundColor(currentSettings.subtitlesBackgroundColor);
        currentVideo.setSubtitlesOutlineColor(currentSettings.subtitlesOutlineColor);
    }, [applyOffset]);

    const rememberTrack = useCallback((track: SubtitleTrack, embedded: boolean) => {
        lastSelectedTrack.current = { id: track.id, embedded };
        streamStateChanged({
            subtitleTrack: {
                id: track.id,
                embedded,
                lang: track.lang,
            },
        });
    }, [streamStateChanged]);

    const disableSubtitles = useCallback(() => {
        defaultTrackSelected.current = true;
        video.setSubtitlesTrack(null);
        video.setExtraSubtitlesTrack(null);
        streamStateChanged({ subtitleTrack: null });
    }, [streamStateChanged, video]);

    const selectEmbeddedTrack = useCallback((track: SubtitleTrack | null) => {
        if (!track) {
            disableSubtitles();
            return;
        }

        defaultTrackSelected.current = true;
        video.setSubtitlesTrack(track.id);
        rememberTrack(track, true);
    }, [disableSubtitles, rememberTrack, video]);

    const selectExtraTrack = useCallback((track: SubtitleTrack | null) => {
        if (!track) {
            disableSubtitles();
            return;
        }

        defaultTrackSelected.current = true;
        video.setExtraSubtitlesTrack(track.id);
        rememberTrack(track, false);
    }, [disableSubtitles, rememberTrack, video]);

    const changeDelay = useCallback((delay: number) => {
        video.setSubtitlesDelay(delay);
        streamStateChanged({ subtitleDelay: delay });
    }, [streamStateChanged, video]);

    const increaseDelay = useCallback(() => {
        changeDelay((video.state.extraSubtitlesDelay ?? 0) + 250);
    }, [changeDelay, video.state.extraSubtitlesDelay]);

    const decreaseDelay = useCallback(() => {
        changeDelay((video.state.extraSubtitlesDelay ?? 0) - 250);
    }, [changeDelay, video.state.extraSubtitlesDelay]);

    const changeSize = useCallback((size: number) => {
        video.setSubtitlesSize(size);
        streamStateChanged({ subtitleSize: size });
    }, [streamStateChanged, video]);

    const updateSize = useCallback((delta: number) => {
        const sizes = CONSTANTS.SUBTITLES_SIZES as number[];
        const sizeIndex = sizes.indexOf(video.state.subtitlesSize ?? -1);
        const nextIndex = Math.max(0, Math.min(sizes.length - 1, sizeIndex + delta));

        changeSize(sizes[nextIndex]);
    }, [changeSize, video.state.subtitlesSize]);

    const changeOffset = useCallback((offset: number) => {
        applyOffset(offset);
        streamStateChanged({ subtitleOffset: offset });
    }, [applyOffset, streamStateChanged]);

    // Auto-sync: the shell listens to the audio around the CURRENT position
    // and correlates it with this track's cues; a confident answer lands
    // through changeDelay, so it persists per stream exactly like a manual
    // nudge. Anchored at the position on purpose: drift is not always
    // uniform, so Sync is pressed where it is wrong and answers for here.
    const autoSync = useCallback(() => {
        const tauri = getTauri();
        const current = videoRef.current;
        const trackId = current.state.selectedExtraSubtitlesTrackId;
        const streamUrl = current.state.stream?.url;
        const cues = loadedCues.current;
        if (!tauri?.core?.invoke || typeof trackId !== 'string' || typeof streamUrl !== 'string' ||
            cues === null || cues.trackId !== trackId || typeof current.state.time !== 'number') {
            return;
        }
        setAutoSyncRunning(true);
        (tauri.core.invoke('subtitles_autosync', {
            url: streamUrl,
            positionMs: current.state.time,
            cues: cues.cues,
        }) as Promise<AutoSyncOutcome>)
            .then((outcome) => {
                switch (outcome.kind) {
                    case 'synced':
                        changeDelay(outcome.delayMs);
                        toast.show({
                            type: 'success',
                            title: t('SUBTITLES_AUTO_SYNC_DONE'),
                            message: t('SUBTITLES_AUTO_SYNC_DONE_DETAIL', { delay: formatDelaySeconds(outcome.delayMs) }),
                            timeout: 4000,
                        });
                        break;
                    case 'no-dialogue':
                        toast.show({
                            type: 'info',
                            title: t('SUBTITLES_AUTO_SYNC_NO_DIALOGUE'),
                            message: t('SUBTITLES_AUTO_SYNC_NO_DIALOGUE_DETAIL'),
                            timeout: 4000,
                        });
                        break;
                    case 'no-cues':
                        toast.show({
                            type: 'info',
                            title: t('SUBTITLES_AUTO_SYNC_NO_CUES'),
                            message: t('SUBTITLES_AUTO_SYNC_NO_CUES_DETAIL'),
                            timeout: 4000,
                        });
                        break;
                    case 'ambiguous':
                        toast.show({
                            type: 'alert',
                            title: t('SUBTITLES_AUTO_SYNC_AMBIGUOUS'),
                            message: t('SUBTITLES_AUTO_SYNC_AMBIGUOUS_DETAIL'),
                            timeout: 4000,
                        });
                        break;
                }
            })
            .catch((error: unknown) => {
                console.error('subtitles_autosync failed', error);
                toast.show({
                    type: 'error',
                    title: t('SUBTITLES_AUTO_SYNC_FAILED'),
                    message: String(error),
                    timeout: 5000,
                });
            })
            .finally(() => setAutoSyncRunning(false));
    }, [changeDelay, t, toast]);

    const pushGenerated = useCallback(() => {
        videoRef.current.setGeneratedSubtitles(
            toVtt(generatedSegments.current.values()),
            generatedLang.current,
            t('SUBTITLES_GENERATED_LABEL'),
        );
    }, [t]);

    const stopGenerate = useCallback(() => {
        const tauri = getTauri();
        if (tauri?.core?.invoke && generateRef.current.state !== 'idle') {
            tauri.core.invoke('subtitles_generate_stop').catch((error: unknown) => {
                console.error('subtitles_generate_stop failed', error);
            });
        }
        setGenerate((current) => ({ ...current, state: 'idle', progress: null, detail: null }));
    }, []);

    const startGenerate = useCallback(() => {
        const tauri = getTauri();
        const url = videoRef.current.state.stream?.url;
        if (!tauri?.core?.invoke || typeof url !== 'string') return;
        // Selection is pending until the player REPORTS the generated track
        // selected (see the stop-on-other-track effect); when the track
        // already exists (resuming) it is selected right away.
        selectGeneratedWhenReady.current = true;
        pendingFrom.current = {
            embedded: videoRef.current.state.selectedSubtitlesTrackId,
            extra: videoRef.current.state.selectedExtraSubtitlesTrackId,
        };
        if (generatedSegments.current.size > 0) {
            defaultTrackSelected.current = true;
            videoRef.current.setExtraSubtitlesTrack(GENERATED_TRACK_ID);
        }
        setGenerate((current) => ({ ...current, state: 'loading', progress: null, detail: null }));
        tauri.core.invoke('subtitles_generate_start', { url }).catch((error: unknown) => {
            console.error('subtitles_generate_start failed', error);
            setGenerate((current) => ({ ...current, state: 'error', progress: null, detail: String(error) }));
            toast.show({ type: 'error', title: t('SUBTITLES_GENERATE_FAILED'), message: String(error), timeout: 5000 });
        });
    }, [t, toast]);

    // AI subtitles are never the default while any other track exists. When
    // NOTHING is offered (no embedded track, no addon track, a few seconds into
    // playback so both have had time to show up) and the viewer wants
    // subtitles at all (a language set), generation starts on its own, once
    // per stream.
    const autoStarted = useRef(false);
    // The position playback was at when this stream loaded: the "few seconds"
    // are counted from there, not from zero, because a resumed stream starts
    // mid-film and its tracks are still being reported at that moment.
    const loadTime = useRef<number | null>(null);
    useEffect(() => {
        autoStarted.current = false;
        loadTime.current = null;
    }, [video.state.stream]);
    useEffect(() => {
        if (video.state.stream === null || typeof video.state.time !== 'number') return;
        if (loadTime.current === null) {
            loadTime.current = video.state.time;
            return;
        }
        if (autoStarted.current || !generate.supported || generate.state !== 'idle') return;
        if (hasTracks || settings.subtitlesLanguage === null) return;
        if (video.state.time - loadTime.current < 3000) return;
        autoStarted.current = true;
        startGenerate();
    }, [generate.supported, generate.state, hasTracks, settings.subtitlesLanguage, startGenerate, video.state.stream, video.state.time]);

    // The "Generate with AI" row behaves like a language: pick it to start
    // (or, while a run is in flight, to come back to its track). Stopping is
    // picking any other language, handled below.
    const selectGenerate = useCallback(() => {
        const { state } = generateRef.current;
        if (state === 'idle' || state === 'error' || state === 'done') {
            startGenerate();
        } else if (generatedSegments.current.size > 0 &&
            videoRef.current.state.selectedExtraSubtitlesTrackId !== GENERATED_TRACK_ID) {
            selectGeneratedWhenReady.current = true;
            pendingFrom.current = {
                embedded: videoRef.current.state.selectedSubtitlesTrackId,
                extra: videoRef.current.state.selectedExtraSubtitlesTrackId,
            };
            defaultTrackSelected.current = true;
            videoRef.current.setExtraSubtitlesTrack(GENERATED_TRACK_ID);
        }
    }, [startGenerate]);

    // The shell's status / line batches for the CURRENT stream.
    useEffect(() => {
        const tauri = getTauri();
        if (!tauri?.event?.listen) return;
        let unlisten: (() => void) | null = null;
        let cancelled = false;
        tauri.event.listen('subtitles-generate', (event: { payload: GenerateEvent }) => {
            const payload = event.payload;
            const url = videoRef.current.state.stream?.url;
            if (typeof url !== 'string' || payload.url !== url) return;
            if (payload.kind === 'status') {
                setGenerate((current) => ({ ...current, state: payload.state, progress: payload.progress, detail: payload.detail }));
                if (payload.state === 'error') {
                    toast.show({ type: 'error', title: t('SUBTITLES_GENERATE_FAILED'), message: payload.detail ?? '', timeout: 6000 });
                }
                return;
            }
            for (const segment of payload.segments) {
                generatedSegments.current.set(segment.startMs, segment);
            }
            if (typeof payload.language === 'string') {
                generatedLang.current = payload.language;
            }
            pushGenerated();
            if (selectGeneratedWhenReady.current &&
                videoRef.current.state.selectedExtraSubtitlesTrackId !== GENERATED_TRACK_ID) {
                // The track exists as of pushGenerated; the flag stays up until
                // the selection is observed in state (a render later).
                defaultTrackSelected.current = true;
                videoRef.current.setExtraSubtitlesTrack(GENERATED_TRACK_ID);
            }
        }).then((fn: () => void) => {
            if (cancelled) fn(); else unlisten = fn;
        });
        return () => {
            cancelled = true;
            unlisten?.();
        };
    }, [pushGenerated, t, toast]);

    // A new stream: forget the old transcript and stop the worker. Also the
    // unmount path (leaving the player must not leave whisper running).
    useEffect(() => {
        generatedSegments.current = new Map();
        generatedLang.current = null;
        selectGeneratedWhenReady.current = false;
        return () => stopGenerate();
    }, [video.state.stream, stopGenerate]);

    // Picking another track (or OFF) while generating stops the worker: the
    // CPU is not worth a track nobody is reading. Not before the generated
    // track has been observed selected once (it does not exist until the
    // first batch, and the selection lands a render after it is requested):
    // clearing the pending flag any earlier stopped the run on its own first
    // lines.
    useEffect(() => {
        if (generate.state === 'idle' || generate.state === 'error' || generate.state === 'done') return;
        if (video.state.selectedExtraSubtitlesTrackId === GENERATED_TRACK_ID) {
            selectGeneratedWhenReady.current = false;
            return;
        }
        const unchanged = video.state.selectedExtraSubtitlesTrackId === pendingFrom.current.extra &&
            video.state.selectedSubtitlesTrackId === pendingFrom.current.embedded;
        if (selectGeneratedWhenReady.current && unchanged) return;
        selectGeneratedWhenReady.current = false;
        stopGenerate();
    }, [generate.state, stopGenerate, video.state.selectedExtraSubtitlesTrackId, video.state.selectedSubtitlesTrackId]);

    const subtitlesAutoSync: 'unsupported' | 'unavailable' | 'ready' | 'running' = useMemo(() => {
        if (!getTauri()?.core?.invoke) return 'unsupported';
        if (autoSyncRunning) return 'running';
        const trackId = video.state.selectedExtraSubtitlesTrackId;
        return typeof trackId === 'string' && trackId === cuesTrackId ? 'ready' : 'unavailable';
    }, [autoSyncRunning, cuesTrackId, video.state.selectedExtraSubtitlesTrackId]);

    onFileDrop(CONSTANTS.SUPPORTED_LOCAL_SUBTITLES, (file: File, buffer: ArrayBuffer) => {
        videoRef.current.addLocalSubtitles(file.name, buffer);
    });

    useEffect(() => {
        if (video.state.stream !== null) {
            video.addExtraSubtitlesTracks(externalSubtitles);
        }
    }, [externalSubtitles, video.state.stream]);

    useEffect(() => {
        if (defaultTrackSelected.current) {
            return;
        }

        if (settings.subtitlesLanguage === null) {
            video.setSubtitlesTrack(null);
            video.setExtraSubtitlesTrack(null);
            defaultTrackSelected.current = true;
            return;
        }

        const savedTrack = player.streamState?.subtitleTrack;
        const savedTrackId = savedTrack?.id;
        const savedLanguage = savedTrack?.lang;
        const savedExternalTrack = Boolean(savedTrackId && savedTrack?.embedded === false);
        const embeddedTrack = savedTrackId ?
            findTrackById(video.state.subtitlesTracks, savedTrackId)
            :
            findTrackByLanguage(video.state.subtitlesTracks, savedLanguage ?? settings.subtitlesLanguage);
        const extraTrack = savedTrackId ?
            findTrackById(video.state.extraSubtitlesTracks, savedTrackId)
            :
            findTrackByLanguage(video.state.extraSubtitlesTracks, savedLanguage ?? settings.subtitlesLanguage);

        if (embeddedTrack?.id) {
            if (video.state.selectedSubtitlesTrackId !== embeddedTrack.id ||
                video.state.selectedExtraSubtitlesTrackId !== null) {
                video.setSubtitlesTrack(embeddedTrack.id);
            }

            defaultTrackSelected.current = true;
            return;
        }

        if (extraTrack?.id) {
            if (video.state.selectedExtraSubtitlesTrackId !== extraTrack.id ||
                video.state.selectedSubtitlesTrackId !== null) {
                video.setExtraSubtitlesTrack(extraTrack.id);
            }

            if (savedExternalTrack) {
                defaultTrackSelected.current = true;
            }
        }
    }, [
        player.streamState,
        settings.subtitlesLanguage,
        video.state.extraSubtitlesTracks,
        video.state.selectedExtraSubtitlesTrackId,
        video.state.selectedSubtitlesTrackId,
        video.state.subtitlesTracks,
    ]);

    useEffect(() => {
        if (video.state.stream === null) {
            return;
        }

        const delay = player.streamState?.subtitleDelay;
        if (typeof delay === 'number') {
            video.setSubtitlesDelay(delay);
        }

        const size = player.streamState?.subtitleSize;
        if (typeof size === 'number') {
            video.setSubtitlesSize(size);
        }

        const offset = player.streamState?.subtitleOffset;
        if (typeof offset === 'number') {
            applyOffset(offset);
        }
    }, [applyOffset, player.streamState, video.state.stream]);

    // The chrome toggled: re-apply the intended offset, lifted or not, riding a
    // short easeOut tween so the subtitles glide with the chrome fade instead of
    // jumping. Falls back to the settings default when nothing has been applied
    // yet.
    useEffect(() => {
        const base = intendedOffset.current ?? settingsRef.current.subtitlesOffset;
        if (typeof base === 'number') {
            writeOffset(liftOffset ? Math.max(base, chromeLiftPercent()) : base, true);
        }
    }, [liftOffset, writeOffset]);

    useEffect(() => () => offsetTween.current?.stop(), []);

    useEffect(() => {
        defaultTrackSelected.current = false;
        lastSelectedTrack.current = null;
    }, [video.state.stream]);

    useEffect(() => {
        if (!hasTracks) {
            closeSubtitlesMenu();
        }
    }, [closeSubtitlesMenu, hasTracks]);

    useEffect(() => {
        const onSubtitlesTrackLoaded = () => {
            toast.show({
                type: 'success',
                title: t('PLAYER_SUBTITLES_LOADED'),
                message: t('PLAYER_SUBTITLES_LOADED_EMBEDDED'),
                timeout: 3000,
            });
        };

        const onExtraSubtitlesTrackLoaded = (track: SubtitleTrack, cues: [number, number][]) => {
            loadedCues.current = { trackId: track.id, cues: Array.isArray(cues) ? cues : [] };
            setCuesTrackId(track.id);
            toast.show({
                type: 'success',
                title: t('PLAYER_SUBTITLES_LOADED'),
                message: track.exclusive ?
                    t('PLAYER_SUBTITLES_LOADED_EXCLUSIVE')
                    :
                    track.local ?
                        t('PLAYER_SUBTITLES_LOADED_LOCAL')
                        :
                        t('PLAYER_SUBTITLES_LOADED_ORIGIN', { origin: track.origin }),
                timeout: 3000,
            });
        };

        const onExtraSubtitlesTrackAdded = (track: SubtitleTrack) => {
            if (track.local) {
                videoRef.current.setExtraSubtitlesTrack(track.id);
            }
        };

        video.events.on('subtitlesTrackLoaded', onSubtitlesTrackLoaded);
        video.events.on('extraSubtitlesTrackLoaded', onExtraSubtitlesTrackLoaded);
        video.events.on('extraSubtitlesTrackAdded', onExtraSubtitlesTrackAdded);
        video.events.on('implementationChanged', applySubtitleStyle);

        return () => {
            video.events.off('subtitlesTrackLoaded', onSubtitlesTrackLoaded);
            video.events.off('extraSubtitlesTrackLoaded', onExtraSubtitlesTrackLoaded);
            video.events.off('extraSubtitlesTrackAdded', onExtraSubtitlesTrackAdded);
            video.events.off('implementationChanged', applySubtitleStyle);
        };
    }, [applySubtitleStyle, t, toast, video.events]);

    onShortcut('subtitlesDelay', (combo) => {
        combo === 1 ? increaseDelay() : decreaseDelay();
    }, [increaseDelay, decreaseDelay], !menusOpen);

    onShortcut('subtitlesSize', (combo) => {
        combo === 1 ? updateSize(1) : updateSize(-1);
    }, [updateSize], !menusOpen);

    onShortcut('toggleSubtitles', () => {
        const subtitlesEnabled = video.state.selectedSubtitlesTrackId !== null ||
            video.state.selectedExtraSubtitlesTrackId !== null;

        if (subtitlesEnabled) {
            if (video.state.selectedSubtitlesTrackId) {
                lastSelectedTrack.current = {
                    id: video.state.selectedSubtitlesTrackId,
                    embedded: true,
                };
            } else if (video.state.selectedExtraSubtitlesTrackId) {
                lastSelectedTrack.current = {
                    id: video.state.selectedExtraSubtitlesTrackId,
                    embedded: false,
                };
            }

            video.setSubtitlesTrack(null);
            video.setExtraSubtitlesTrack(null);
            return;
        }

        const savedTrack = player.streamState?.subtitleTrack ?? lastSelectedTrack.current;
        if (savedTrack?.id) {
            savedTrack.embedded ?
                video.setSubtitlesTrack(savedTrack.id)
                :
                video.setExtraSubtitlesTrack(savedTrack.id);
        }
    }, [
        player.streamState,
        video.state.selectedExtraSubtitlesTrackId,
        video.state.selectedSubtitlesTrackId,
    ], !menusOpen);

    onShortcut('subtitlesMenu', () => {
        closeMenus();
        if (hasTracks) {
            toggleSubtitlesMenu();
        }
    }, [closeMenus, hasTracks, toggleSubtitlesMenu]);

    const menuProps = useMemo(() => ({
        subtitlesLanguage: settings.subtitlesLanguage,
        interfaceLanguage: settings.interfaceLanguage,
        subtitlesTracks: video.state.subtitlesTracks,
        selectedSubtitlesTrackId: video.state.selectedSubtitlesTrackId,
        subtitlesOffset: video.state.subtitlesOffset,
        subtitlesSize: video.state.subtitlesSize,
        extraSubtitlesTracks: video.state.extraSubtitlesTracks,
        selectedExtraSubtitlesTrackId: video.state.selectedExtraSubtitlesTrackId,
        extraSubtitlesOffset: video.state.extraSubtitlesOffset,
        extraSubtitlesDelay: video.state.extraSubtitlesDelay,
        subtitlesDelay: video.state.subtitlesDelay,
        extraSubtitlesSize: video.state.extraSubtitlesSize,
        onSubtitlesTrackSelected: selectEmbeddedTrack,
        onExtraSubtitlesTrackSelected: selectExtraTrack,
        onSubtitlesOffsetChanged: changeOffset,
        onSubtitlesSizeChanged: changeSize,
        onExtraSubtitlesOffsetChanged: changeOffset,
        onExtraSubtitlesDelayChanged: changeDelay,
        onExtraSubtitlesSizeChanged: changeSize,
        subtitlesAutoSync,
        onSubtitlesAutoSync: autoSync,
        subtitlesGenerate: generate,
        onSubtitlesGenerateSelect: selectGenerate,
    }), [
        autoSync,
        generate,
        selectGenerate,
        changeDelay,
        changeOffset,
        changeSize,
        selectEmbeddedTrack,
        subtitlesAutoSync,
        selectExtraTrack,
        settings.interfaceLanguage,
        settings.subtitlesLanguage,
        video.state.extraSubtitlesDelay,
        video.state.subtitlesDelay,
        video.state.extraSubtitlesOffset,
        video.state.extraSubtitlesSize,
        video.state.extraSubtitlesTracks,
        video.state.selectedExtraSubtitlesTrackId,
        video.state.selectedSubtitlesTrackId,
        video.state.subtitlesOffset,
        video.state.subtitlesSize,
        video.state.subtitlesTracks,
    ]);

    return {
        streamSubtitles,
        allSubtitleTracks: allTracks,
        extraSubtitleTracks: video.state.extraSubtitlesTracks,
        selectedExtraSubtitleTrackId: video.state.selectedExtraSubtitlesTrackId,
        subtitlesMenuProps: menuProps,
    };
};

export default useSubtitles;
