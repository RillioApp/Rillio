// Copyright (C) 2017-2023 Smart code 203358507

import React from 'react';
import { useParams, useNavigate } from 'react-router';
import { useSearchParams } from 'react-router-dom';
import debounce from 'lodash.debounce';
import { useTranslation } from 'react-i18next';
import useRouteFocused from 'rillio/common/useRouteFocused';
import { serverFetch } from 'rillio/common/serverFetch';
import { rewriteServerUrl } from 'rillio/common/serverAddress';
import { useCore } from 'rillio/core';
import { useServices, useGamepad } from 'rillio/services';
import { useContentGamepadNavigation } from 'rillio/services/GamepadNavigation';
import { useSettings, useProfile, useFullscreen, useBinaryState, useToast, useStreamingServer, withCoreSuspender, usePlatform, onShortcut, useDiscord, EMPTY_DISCORD_TIMESTAMPS, getPlaybackDiscordActivity } from 'rillio/common';
import { toPath } from 'rillio-router';
import { Presence, ContextMenu } from 'rillio/components';
import { cn } from 'rillio/components/ui/cn';
import TopBar from './TopBar';
import Buffering from './Buffering';
import VolumeChangeIndicator from './VolumeChangeIndicator';
import Error from './Error';
import ControlBar from './ControlBar';
import NextVideoPopup from './NextVideoPopup';
import StatisticsMenu from './StatisticsMenu';
import OptionsMenu from './OptionsMenu';
import SubtitlesMenu from './SubtitlesMenu';
import AudioMenu from './AudioMenu';
import SpeedMenu, { MAX_SPEED, MIN_SPEED } from './SpeedMenu';
import SideDrawerButton from './SideDrawerButton';
import SideDrawer from './SideDrawer';
import usePlayer from './usePlayer';
import useStatistics from './useStatistics';
import useSlowDownload from './useSlowDownload';
import useNextEpisodePreload from './useNextEpisodePreload';
import useStreamingModeWatched from './useStreamingModeWatched';
import useCacheMetadata from './useCacheMetadata';
import NextEpisodePreloadPrompt from './NextEpisodePreloadPrompt';
import { useSkipSegments, activeSegment } from './skipIntro';
import SkipPill from './SkipPill/SkipPill';
import { pickAudioTrack } from './smartTracks';
import useVideo from './useVideo';
import useSubtitles from './useSubtitles';
import useDub from './useDub';
import useTimelineChapters from './timelineChapters';
import useVideoSnapshotBackdrop from './useVideoSnapshotBackdrop';
import useShaderBlurRect from './useShaderBlurRect';
import { SnapshotBackdropContext } from './SnapshotBackdrop';
import { ShaderBlurContext } from './ShaderBlurRect';
import Video from './Video';
import Indicator from './Indicator/Indicator';
import useMediaSession from './useMediaSession';

// The Google Cast SDK loads globally via a script tag; it is untyped here.
declare const cast: any;

// Player layer-stack classes, ported from the former Player/styles.less to Tailwind.
// The immersion opacity cascade and the active-slider grab cursor stay as global rules
// in styles/tailwind.css (they need html/body/ancestor selectors). Each chrome layer
// that fades under immersion carries the `player-immersion-fade` hook; the container
// toggles `data-immersed`, and `player-container` is the stable hook the global rule
// targets. Layers that reposition (nav / control / side-drawer / menu / indicator) are
// self-contained (no base inset-0) to avoid shorthand-vs-longhand ordering surprises.
const CONTAINER = 'player-container relative z-0 h-full w-full bg-black';
const LAYER = 'absolute inset-0 z-0';
const BACKGROUND_IMAGE = 'h-screen w-screen object-cover opacity-60';
const NAV_BAR_LAYER = "player-immersion-fade absolute left-(--safe-area-inset-left) right-(--safe-area-inset-right) top-0 z-0 overflow-visible before:pointer-events-none before:absolute before:inset-x-0 before:top-0 before:-z-10 before:h-32 before:bg-gradient-to-b before:from-black/35 before:to-transparent before:content-['']";
const CONTROL_BAR_LAYER = "player-immersion-fade absolute bottom-0 left-(--safe-area-inset-left) right-(--safe-area-inset-right) z-0 overflow-visible pb-[calc(0.5rem+var(--safe-area-inset-bottom))] before:pointer-events-none before:absolute before:inset-x-0 before:bottom-0 before:-z-10 before:h-40 before:bg-gradient-to-t before:from-black/35 before:to-transparent before:content-['']";
const SIDE_DRAWER_BUTTON_LAYER = 'player-immersion-fade fixed left-auto right-[-4rem] top-1/2 z-0 -translate-y-1/2 [@media(max-width:1000px)]:right-[-2rem]';
const INDICATOR_LAYER = 'absolute bottom-40 left-0 right-0 z-0';
// The house floating-panel material (identical to every app menu/dialog): glass-panel
// fill + a border-line hairline + shadow-elevated + the glass blur token, plus this
// layer's fixed bottom-right placement.
// Whether an event target sits inside the body-portalled sonner toaster. Used by
// the immersion handlers: the toaster is OUTSIDE the player container, so pointer
// moves onto/off toasts look like leaving/entering the player.
const isInToaster = (node: EventTarget | null): boolean =>
    node instanceof Element && node.closest('[data-sonner-toaster]') !== null;

const MENU_LAYER = 'player-immersion-fade absolute bottom-(--player-chrome-clearance) left-auto right-16 top-auto z-0 max-h-[calc(100%-13rem)] max-w-[calc(100%-4rem)] overflow-auto rounded-card border border-line bg-glass-panel shadow-elevated backdrop-blur-(--glass-blur) [@media(orientation:portrait)_and_(max-width:640px)]:bottom-44 [@media(orientation:portrait)_and_(max-width:640px)]:right-10';

// Replaced by smartTracks.pickAudioTrack: find-first took whatever the muxer
// listed, including commentary tracks; scoring rejects those and lets the
// container's default flag break ties.
const findTrackById = (tracks: any[], id: string) => tracks.find((track) => track.id === id);

const GAMEPAD_HANDLER_ID = 'player';

const Player = () => {
    const { stream, streamTransportUrl, metaTransportUrl, type, id, videoId } = useParams();
    const urlParams = React.useMemo(() => ({
        stream,
        streamTransportUrl,
        metaTransportUrl,
        type,
        id,
        videoId
    }), [stream, streamTransportUrl, metaTransportUrl, type, id, videoId]);
    const [queryParams] = useSearchParams();
    const navigate = useNavigate();
    const { t } = useTranslation();
    const services = useServices();
    const core = useCore();
    const gamepad = useGamepad();
    const forceTranscoding = React.useMemo(() => {
        return queryParams.has('forceTranscoding');
    }, [queryParams]);
    const profile = useProfile();
    const [player, videoParamsChanged, streamStateChanged, timeChanged, seek, pausedChanged, ended, nextVideo] = usePlayer(urlParams);
    const [settings] = useSettings();
    const streamingServer = useStreamingServer();
    const statistics = useStatistics(player, streamingServer);
    const video = useVideo();
    const routeFocused = useRouteFocused();
    const platform = usePlatform();
    const toast = useToast();
    const discord = useDiscord();
    const discordTimestamps = React.useRef(EMPTY_DISCORD_TIMESTAMPS);

    const [seeking, setSeeking] = React.useState(false);

    const [casting, setCasting] = React.useState(() => {
        return services.chromecast.active && services.chromecast.transport.getCastState() === cast.framework.CastState.CONNECTED;
    });
    const playbackDevices = React.useMemo(() => streamingServer.playbackDevices !== null && streamingServer.playbackDevices.type === 'Ready' ? streamingServer.playbackDevices.content as PlaybackDevice[] : [], [streamingServer]);

    const playerRef = React.useRef<HTMLDivElement | null>(null);
    const bufferingRef = React.useRef<HTMLDivElement | null>(null);
    const errorRef = React.useRef<HTMLDivElement | null>(null);

    const [immersed, setImmersed] = React.useState(true);
    // The chrome ALWAYS hides 4s after the last activity (mouse move, click, or
    // key press) - hovering the bars no longer holds it awake (Michael's call).
    // The menus-open and slider-drag gates still keep it visible mid-interaction.
    const setImmersedDebounced = React.useCallback(debounce(setImmersed, 4000), []);
    const [fullscreen, , , toggleFullscreen, , setVideoElement] = useFullscreen();

    React.useEffect(() => {
        const el = video.containerRef.current?.querySelector('video');
        setVideoElement(el || null);
        return () => setVideoElement(null);
    }, [video.state.manifest]);

    const [optionsMenuOpen, , closeOptionsMenu, toggleOptionsMenu] = useBinaryState(false);
    const [subtitlesMenuOpen, , closeSubtitlesMenu, toggleSubtitlesMenu] = useBinaryState(false);
    const [audioMenuOpen, , closeAudioMenu, toggleAudioMenu] = useBinaryState(false);
    const [speedMenuOpen, , closeSpeedMenu, toggleSpeedMenu] = useBinaryState(false);
    const [statisticsMenuOpen, , closeStatisticsMenu, toggleStatisticsMenu] = useBinaryState(false);
    const [nextVideoPopupOpen, openNextVideoPopup, closeNextVideoPopup] = useBinaryState(false);
    const [sideDrawerOpen, , closeSideDrawer, toggleSideDrawer] = useBinaryState(false);

    const menusOpen = React.useMemo(() => {
        return optionsMenuOpen || subtitlesMenuOpen || audioMenuOpen || speedMenuOpen || statisticsMenuOpen || sideDrawerOpen || nextVideoPopupOpen;
    }, [optionsMenuOpen, subtitlesMenuOpen, audioMenuOpen, speedMenuOpen, statisticsMenuOpen, sideDrawerOpen, nextVideoPopupOpen]);

    // Real blurred video behind the open panels: mpv renders in a native window
    // the WebView's backdrop-filter cannot see, so the shell snapshots the frame
    // and SnapshotBackdrop (inside each panel) blurs it. Shell-only; null
    // everywhere else, which leaves the panels on their dark glass alone.
    const snapshotBackdrop = useVideoSnapshotBackdrop(menusOpen || sideDrawerOpen);

    // The other answer to the same problem, and the one that can actually be live:
    // instead of shipping frames to the web layer, hand the shell the panels' rects
    // and let an mpv GLSL shader blur the video where they sit. Null (so every
    // <ShaderBlurRect /> below renders nothing) unless its flag is on - it hooks
    // the pipeline that carries HDR/DV passthrough. See useShaderBlurRect.
    const shaderBlur = useShaderBlurRect();

    const closeMenus = React.useCallback(() => {
        closeOptionsMenu();
        closeSubtitlesMenu();
        closeAudioMenu();
        closeSpeedMenu();
        closeStatisticsMenu();
        closeSideDrawer();
    }, []);

    // Raw streaming-server statistics content for the expandable stats section
    // (downloaded/uploaded/file size), beyond the peers/speed summary.
    const statisticsDetails = React.useMemo(() => {
        return streamingServer.statistics?.type === 'Ready' ? streamingServer.statistics.content as Statistics : null;
    }, [streamingServer.statistics]);

    // Live streaming-server settings (the torrent profile lives here). Used by
    // the slow-download escalation to offer "Fast mode" via the EXISTING profile
    // mechanism, not a parallel flag.
    const streamingSettings = React.useMemo(() => {
        return streamingServer.settings?.type === 'Ready' ? streamingServer.settings.content as StreamingServerSettings : null;
    }, [streamingServer.settings]);

    // Sustained-slow detection + ephemeral speed test for the Initializing panel.
    const slowDownload = useSlowDownload({
        core,
        infoHash: statistics.infoHash,
        hasStatistics: statisticsDetails !== null,
        peers: statisticsDetails?.peers,
        speedBytesPerSec: statisticsDetails?.downloadSpeed,
        streamingSettings,
    });

    // Next-episode preload: prompt scheduling, per-series dismissal, the
    // /cache/download trigger, and the start-next-paused handoff after ended.
    const nextEpisodePreload = useNextEpisodePreload({ player, video });

    // Correlate this torrent with the title it is playing, while we hold the
    // full metadata: the Cache page shows real artwork instead of a filename.
    useCacheMetadata({
        infoHash: statistics.infoHash ?? null,
        serverUrl: typeof profile.settings.streamingServerUrl === 'string' ? profile.settings.streamingServerUrl : null,
        player,
    });

    // Streaming mode: report a torrent stream watched at >= 90% so the local
    // server can clean its cache entry up later (un-kept entries only).
    useStreamingModeWatched({
        infoHash: statistics.infoHash ?? null,
        serverUrl: typeof profile.settings.streamingServerUrl === 'string' ? profile.settings.streamingServerUrl : null,
        time: video.state.time,
        duration: video.state.duration,
    });

    // Skip intro/outro: known segments for this video (file chapters + the
    // AniSkip/TheIntroDB community databases), and whichever one the playhead
    // is currently inside - that one gets the pill.
    const skipSegments = useSkipSegments(
        player.selected?.metaRequest?.path?.id ?? null,
        player.selected?.streamRequest?.path?.id ?? null,
        video.state.chapters ?? [],
        video.state.duration,
    );
    const skipTarget = React.useMemo(
        () => activeSegment(skipSegments, video.state.time),
        [skipSegments, video.state.time],
    );

    // "Try a different source": back to this title's streams list. The player
    // route carries the meta type/id and the video id, which is exactly the
    // MetaDetails streams route (/metadetails/:type/:id/:videoId).
    const onTryDifferentSource = React.useCallback(() => {
        const segments = [type, id, videoId]
            .filter((segment): segment is string => typeof segment === 'string' && segment.length > 0)
            .map((segment) => encodeURIComponent(segment));
        if (segments.length === 0) {
            navigate(-1);
            return;
        }
        navigate(`/metadetails/${segments.join('/')}`);
    }, [type, id, videoId, navigate]);

    const overlayHidden = React.useMemo(() => {
        // Hides while PAUSED too (paused !== null only guards the not-yet-loaded
        // state, where the chrome must stay). Same 4s idle countdown either way.
        return immersed && !casting && video.state.paused !== null && !menusOpen;
    }, [immersed, casting, video.state.paused, menusOpen]);

    const {
        streamSubtitles,
        allSubtitleTracks,
        extraSubtitleTracks,
        selectedExtraSubtitleTrackId,
        subtitlesMenuProps,
    } = useSubtitles({
        player,
        video,
        settings,
        streamStateChanged,
        menusOpen,
        closeMenus,
        closeSubtitlesMenu,
        toggleSubtitlesMenu,
        // Lift the subtitles clear of the control bar while the chrome is up;
        // they drop back to the user's own offset when it fades. Suspended only
        // while the SUBTITLES menu is open (its offset slider must show and move
        // the REAL value, not the lifted one) - other panels (side drawer,
        // audio, ...) keep the lift, or opening them visibly shifts the subs.
        liftOffset: !overlayHidden && !subtitlesMenuOpen,
    });

    const dubAbout = React.useMemo(() => {
        const content = player?.metaItem?.content;
        const parts = [content?.name, content?.description].filter((p): p is string => typeof p === 'string' && p.length > 0);
        return parts.length > 0 ? parts.join('. ') : null;
    }, [player?.metaItem?.content]);
    const { dub, dubChosen, dubPlaying, onDubSelect } = useDub({ video, about: dubAbout });

    // Seek-bar segments: real chapter marks merged with subtitle silence gaps
    // (the selected EXTERNAL track's cues) and the shell's visual scene sweep,
    // per availability (see timelineChapters.ts).
    const selectedExtraSubtitleTrackUrl = React.useMemo(() => {
        const track = (extraSubtitleTracks as any[]).find((t: any) => t?.id === selectedExtraSubtitleTrackId);
        return typeof track?.url === 'string' ? track.url : null;
    }, [extraSubtitleTracks, selectedExtraSubtitleTrackId]);
    const timelineChapters = useTimelineChapters(
        video.state.chapters ?? [],
        typeof video.state.stream?.url === 'string' ? video.state.stream.url : null,
        selectedExtraSubtitleTrackUrl,
    );

    // Mirror the chrome's visibility onto <html>, the same way WindowControls
    // publishes `window-fullscreen`. The toast layer is portalled to <body>, so
    // the `.player-container[data-immersed]` cascade cannot reach it; this is the
    // only signal it can see to lift clear of the control bar. Cleared on unmount
    // so leaving the player never strands the class (the toaster outlives it).
    React.useEffect(() => {
        const root = document.documentElement;
        root.classList.toggle('player-chrome-visible', !overlayHidden);
        return () => root.classList.remove('player-chrome-visible');
    }, [overlayHidden]);

    const nextVideoPopupDismissed = React.useRef(false);
    const defaultAudioTrackSelected = React.useRef(false);
    const playingOnExternalDevice = React.useRef(false);
    const [error, setError] = React.useState<any>(null);

    const VIDEO_SCALES = ['contain', 'cover', 'fill'];
    const VIDEO_SCALE_LABELS: Record<string, string> = { contain: t('PLAYER_SCALE_FIT'), cover: t('PLAYER_SCALE_CROP'), fill: t('PLAYER_SCALE_STRETCH') };

    const playbackSpeed = React.useRef(video.state.playbackSpeed || 1);
    const pressTimer = React.useRef<ReturnType<typeof setTimeout> | null>(null);
    const longPress = React.useRef(false);
    // Intended paused state, so two fast clicks (a double-click) toggle
    // deterministically without racing React's re-render. See onVideoClick.
    const intendedPaused = React.useRef<boolean | null>(null);
    // Timestamp of the previous video click, for custom double-click detection.
    // The native dblclick uses the ~500ms OS window, which feels far too long.
    const lastVideoClickTime = React.useRef(0);
    const controlBarRef = React.useRef<HTMLDivElement | null>(null);

    const HOLD_DELAY = 400;

    const handleNextVideoNavigation = React.useCallback((deepLinks: any, bingeWatching: boolean, ended: boolean) => {
        if (ended) {
            if (bingeWatching) {
                if (deepLinks.player) {
                    navigate(toPath(deepLinks.player), { replace: true });
                } else if (deepLinks.metaDetailsStreams) {
                    navigate(toPath(deepLinks.metaDetailsStreams), { replace: true });
                }
            } else {
                navigate(-1);
            }

        } else {
            if (deepLinks.player) {
                navigate(toPath(deepLinks.player), { replace: true });
            } else if (deepLinks.metaDetailsStreams) {
                navigate(toPath(deepLinks.metaDetailsStreams), { replace: true });
            }
        }
    }, []);

    const onEnded = React.useCallback(() => {
        ended();
        if (player.nextVideo !== null) {
            nextVideo();

            // An accepted preload means the user already chose to continue:
            // auto-continue even with binge watching off, and arm the handoff
            // that starts the next episode paused once it loads.
            if (nextEpisodePreload.accepted) {
                nextEpisodePreload.armPausedStart();
            }
            const deepLinks = player.nextVideo.deepLinks;
            handleNextVideoNavigation(deepLinks, profile.settings.bingeWatching || nextEpisodePreload.accepted, true);
        } else {
            navigate(-1);
        }
    }, [player.nextVideo, profile.settings.bingeWatching, handleNextVideoNavigation, nextEpisodePreload.accepted, nextEpisodePreload.armPausedStart]);

    const onError = React.useCallback((error: any) => {
        console.error('Player', error);
        if (error.critical) {
            setError(error);
            // For a torrent stream the video layer only knows "loading failed";
            // the local engine knows WHY (disk full, torrent errored/paused
            // after a write failure, ...). Ask it and upgrade the message so
            // the user is not stuck staring at a generic error.
            const stream = player.selected !== null ? player.selected.stream : null;
            const serverUrl = profile.settings.streamingServerUrl;
            if (stream !== null && typeof stream.infoHash === 'string' && typeof serverUrl === 'string') {
                serverFetch(new URL(`${stream.infoHash}/stats.json`, serverUrl))
                    .then((resp) => resp.json())
                    .then((stats) => {
                        if (stats !== null && typeof stats.engineError === 'string' && stats.engineError.length > 0) {
                            // Disk full is user-fixable right here: deleting cached
                            // media frees the space, so point at the Cached page.
                            const diskFull = /not enough space|os error 112/i.test(stats.engineError);
                            setError((current: any) => current !== null ? {
                                ...current,
                                message: `${current.message}: ${stats.engineError}`,
                                freeSpace: diskFull,
                            } : current);
                        }
                    })
                    .catch(() => { /* the generic message stands */ });
            }
        } else {
            toast.show({
                type: 'error',
                title: t('ERROR'),
                message: error.message,
                timeout: 3000
            });
        }
    }, [player.selected, profile.settings.streamingServerUrl]);

    const onPlayRequested = React.useCallback(() => {
        playingOnExternalDevice.current = false;
        video.setPaused(false);
        setSeeking(false);
    }, []);

    const onPauseRequested = React.useCallback(() => {
        video.setPaused(true);
    }, []);

    const onMuteRequested = React.useCallback(() => {
        video.setMuted(true);
    }, []);

    const onUnmuteRequested = React.useCallback(() => {
        video.setMuted(false);
    }, []);

    const onVolumeChangeRequested = React.useCallback((volume: number) => {
        video.setVolume(volume);
    }, []);

    const onSeekRequested = React.useCallback((time: number) => {
        video.setTime(time);
        seek(time, video.state.duration, video.state.manifest?.name);
    }, [video.state.duration, video.state.manifest]);

    const onPlaybackSpeedChanged = React.useCallback((rate: number, skipUpdate?: boolean) => {
        video.setPlaybackSpeed(rate);

        if (skipUpdate) return;

        playbackSpeed.current = rate;

    }, []);

    const onVideoScaleChanged = React.useCallback(() => {
        const currentScale = video.state.videoScale || 'contain';
        const currentIndex = VIDEO_SCALES.indexOf(currentScale);
        const nextScale = VIDEO_SCALES[(currentIndex + 1) % VIDEO_SCALES.length];
        video.setVideoScale(nextScale);
    }, [video.state.videoScale]);

    const onAudioTrackSelected = React.useCallback((id: string) => {
        video.setAudioTrack(id);
        streamStateChanged({
            audioTrack: {
                id,
            },
        });
    }, [streamStateChanged]);

    // Session-scoped on purpose: an a/v offset is a property of one file's
    // mux, so it is not written into the library's stream state.
    const onAudioDelayChanged = React.useCallback((delay: number) => {
        video.setAudioDelay(delay);
    }, []);

    const onDismissNextVideoPopup = React.useCallback(() => {
        closeNextVideoPopup();
        nextVideoPopupDismissed.current = true;
    }, []);

    const onNextVideoRequested = React.useCallback(() => {
        if (player.nextVideo !== null) {
            nextVideo();

            const deepLinks = player.nextVideo.deepLinks;
            handleNextVideoNavigation(deepLinks, profile.settings.bingeWatching, false);
        }
    }, [player.nextVideo, handleNextVideoNavigation, profile.settings]);

    // Keep the intent ref in sync when paused changes from anywhere else
    // (keyboard, ended, load, mpv).
    React.useEffect(() => {
        if (video.state.paused !== null) {
            intendedPaused.current = video.state.paused;
        }
    }, [video.state.paused]);

    const onVideoClick = React.useCallback(() => {
        if (video.state.paused === null || longPress.current) {
            return;
        }
        const DOUBLE_CLICK_MS = 200;
        const current = intendedPaused.current !== null ? intendedPaused.current : video.state.paused;
        const now = Date.now();
        if (now - lastVideoClickTime.current < DOUBLE_CLICK_MS) {
            // Second click inside the window -> double-click: undo the pause
            // toggle the first click just did, and go fullscreen instead. Custom
            // 200ms window because the native dblclick's ~500ms OS window is too
            // long for a player. Net result: play state unchanged + fullscreen.
            lastVideoClickTime.current = 0;
            const reverted = !current; // state before the first click
            intendedPaused.current = reverted;
            if (reverted) {
                onPauseRequested();
            } else {
                onPlayRequested();
            }
            toggleFullscreen();
            return;
        }
        lastVideoClickTime.current = now;
        // Single click: toggle immediately for instant feedback (no debounce).
        // Driven off the ref so two fast clicks can't race React's re-render.
        const nextPaused = !current;
        intendedPaused.current = nextPaused;
        if (nextPaused) {
            onPauseRequested();
        } else {
            onPlayRequested();
        }
    }, [video.state.paused, onPlayRequested, onPauseRequested, toggleFullscreen]);

    const onContainerMouseDown = React.useCallback((event: React.MouseEvent) => {
        if (!(event.nativeEvent as any).optionsMenuClosePrevented) {
            closeOptionsMenu();
        }
        if (!(event.nativeEvent as any).subtitlesMenuClosePrevented) {
            closeSubtitlesMenu();
        }
        if (!(event.nativeEvent as any).audioMenuClosePrevented) {
            closeAudioMenu();
        }
        if (!(event.nativeEvent as any).speedMenuClosePrevented) {
            closeSpeedMenu();
        }
        if (!(event.nativeEvent as any).statisticsMenuClosePrevented) {
            closeStatisticsMenu();
        }

        closeSideDrawer();

        // A click is user activity too: reveal the chrome and restart the 4s idle
        // countdown, exactly like mouse movement does (onContainerMouseMove). The
        // slider-drag gate still holds: while a thumb is held, body pointer-events
        // are off so this never fires, and the CSS `active-slider-within` guard keeps
        // the control bar visible regardless of the immersion state.
        setImmersed(false);
        setImmersedDebounced(true);
    }, []);

    const onContainerMouseMove = React.useCallback(() => {
        // Unconditional restart: the old immersePrevented hover-keepalive is gone,
        // the countdown always runs (see the 4s debounce note above).
        setImmersed(false);
        setImmersedDebounced(true);
    }, []);

    const onContainerMouseLeave = React.useCallback((event: React.MouseEvent) => {
        // Moving onto a toast is NOT leaving the player. The toaster is portalled
        // to <body>, so the pointer entering it fires this leave handler; immersing
        // here drops the toast out from under the cursor (the chrome-clearance
        // offset animates away), the container underneath gets boundary/move
        // events, the chrome comes back, the toast lifts back under the cursor -
        // an infinite 200ms flicker. Treat it as ordinary activity instead; the
        // toaster hover-hold effect below keeps the chrome up from there.
        if (isInToaster(event.relatedTarget)) {
            setImmersed(false);
            setImmersedDebounced(true);
            return;
        }
        setImmersedDebounced.cancel();
        setImmersed(true);
    }, []);

    // While the pointer rests ON a toast, HOLD the chrome visible (cancel the 4s
    // idle countdown outright, do not just restart it): a stationary cursor gets
    // no further events, so a running countdown would expire, drop the toast out
    // from under it and start the same flicker loop the mouseleave guard above
    // prevents. The countdown restarts the moment the pointer leaves the toaster.
    // Document-level delegation because the toaster lives outside this tree and
    // outlives it.
    React.useEffect(() => {
        const onOver = (event: MouseEvent) => {
            if (isInToaster(event.target) && !isInToaster(event.relatedTarget)) {
                setImmersedDebounced.cancel();
                setImmersed(false);
            }
        };
        const onOut = (event: MouseEvent) => {
            if (isInToaster(event.target) && !isInToaster(event.relatedTarget)) {
                setImmersed(false);
                setImmersedDebounced(true);
            }
        };
        document.addEventListener('mouseover', onOver);
        document.addEventListener('mouseout', onOut);
        return () => {
            document.removeEventListener('mouseover', onOver);
            document.removeEventListener('mouseout', onOut);
        };
    }, []);

    // Keyboard input is user activity too: any key while the route is focused
    // reveals the chrome and restarts the same 4s countdown.
    React.useEffect(() => {
        if (!routeFocused) {
            return;
        }
        const onAnyKey = () => {
            setImmersed(false);
            setImmersedDebounced(true);
        };
        window.addEventListener('keydown', onAnyKey);
        return () => window.removeEventListener('keydown', onAnyKey);
    }, [routeFocused]);

    const onBarMouseMove = React.useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).immersePrevented = true;
    }, []);

    const onPlayPause = React.useCallback(() => {
        if (!menusOpen && !nextVideoPopupOpen && video.state.paused !== null) {
            if (video.state.paused) {
                onPlayRequested();
                setSeeking(false);
            } else {
                onPauseRequested();
            }
        }
    }, [menusOpen, nextVideoPopupOpen, video.state.paused]);

    const onSeekPrev = React.useCallback((event?: { shiftKey?: boolean }) => {
        if (!menusOpen && !nextVideoPopupOpen && video.state.time !== null) {
            const seekDuration = event?.shiftKey ? settings.seekShortTimeDuration : settings.seekTimeDuration;
            const seekTime = video.state.time - seekDuration;
            setSeeking(true);
            onSeekRequested(Math.max(seekTime, 0));
        }
    }, [menusOpen, nextVideoPopupOpen, video.state.time]);

    const onSeekNext = React.useCallback((event?: { shiftKey?: boolean }) => {
        if (!menusOpen && !nextVideoPopupOpen && video.state.time !== null) {
            const seekDuration = event?.shiftKey ? settings.seekShortTimeDuration : settings.seekTimeDuration;
            setSeeking(true);
            onSeekRequested(video.state.time + seekDuration);
        }
    }, [menusOpen, nextVideoPopupOpen, video.state.time]);

    const onVolumeUp = React.useCallback(() => {
        if (!menusOpen && !nextVideoPopupOpen && video.state.volume !== null) {
            onVolumeChangeRequested(Math.min(video.state.volume + 5, 200));
        }
    }, [menusOpen, nextVideoPopupOpen, video.state.volume]);

    const onVolumeDown = React.useCallback(() => {
        if (!menusOpen && !nextVideoPopupOpen && video.state.volume !== null) {
            onVolumeChangeRequested(Math.max(video.state.volume - 5, 0));
        }
    }, [menusOpen, nextVideoPopupOpen, video.state.volume]);

    const onGamepadSeekAndVol = React.useCallback((axis?: string) => {
        switch(axis) {
            case 'left': {
                onSeekPrev();
                break;
            }
            case 'right': {
                onSeekNext();
                break;
            }
            case 'up': {
                onVolumeUp();
                break;
            }
            case 'down': {
                onVolumeDown();
                break;
            }
        }
    }, [onSeekPrev, onSeekNext, onVolumeUp, onVolumeDown]);

    useContentGamepadNavigation(playerRef, GAMEPAD_HANDLER_ID);

    React.useEffect(() => {
        gamepad?.on('buttonX', GAMEPAD_HANDLER_ID, onPlayPause);
        gamepad?.on('analogRight', GAMEPAD_HANDLER_ID, onGamepadSeekAndVol);

        return () => {
            gamepad?.off('buttonX', GAMEPAD_HANDLER_ID);
            gamepad?.off('analogRight', GAMEPAD_HANDLER_ID);
        };
    }, [onPlayPause, onGamepadSeekAndVol]);

    React.useEffect(() => {
        setError(null);
        video.unload();

        if (player.selected && player.stream?.type === 'Ready' && streamingServer.settings?.type !== 'Loading') {
            video.load({
                stream: {
                    ...player.stream.content,
                    subtitles: streamSubtitles
                },
                autoplay: true,
                time: player.libraryItem !== null &&
                    player.selected.streamRequest !== null &&
                    player.selected.streamRequest.path !== null &&
                    player.libraryItem.state.video_id === player.selected.streamRequest.path.id ?
                    player.libraryItem.state.timeOffset
                    :
                    0,
                forceTranscoding: forceTranscoding || casting,
                maxAudioChannels: settings.surroundSound ? 32 : 2,
                hardwareDecoding: settings.hardwareDecoding,
                assSubtitlesStyling: settings.assSubtitlesStyling,
                gpuVideoProcessing: settings.gpuVideoProcessing && platform.shell.capabilities.gpuVideoProcessing,
                videoMode: settings.videoMode,
                platform: platform.name,
                streamingServerURL: streamingServer.baseUrl ?
                    casting ?
                        streamingServer.baseUrl
                        :
                        streamingServer.selected!.transportUrl
                    :
                    null,
                seriesInfo: player.seriesInfo,
            }, {
                chromecastTransport: services.chromecast.active ? services.chromecast.transport : null,
                shellTransport: platform.shell.active ? platform.shell : null,
            });
        }
    }, [streamingServer.baseUrl, player.selected, player.stream, streamSubtitles, forceTranscoding, casting]);

    React.useEffect(() => {
        !seeking && timeChanged(video.state.time, video.state.duration, video.state.manifest?.name);
    }, [video.state.time, video.state.duration, video.state.manifest, seeking]);

    React.useEffect(() => {
        if (playingOnExternalDevice.current && video.state.paused === false) {
            onPauseRequested();
        } else if (video.state.paused !== null) {
            pausedChanged(video.state.paused);
        }
    }, [video.state.paused]);

    React.useEffect(() => {
        videoParamsChanged(video.state.videoParams);
    }, [video.state.videoParams]);

    React.useEffect(() => {
        if (player.nextVideo !== null && !nextVideoPopupDismissed.current) {
            if (video.state.time !== null && video.state.duration !== null && video.state.time < video.state.duration && (video.state.duration - video.state.time) <= settings.nextVideoNotificationDuration) {
                openNextVideoPopup();
            } else {
                closeNextVideoPopup();
            }
        }
    }, [player.nextVideo, video.state.time, video.state.duration]);

    // Auto audio track selection: the track the user explicitly chose last time
    // wins; otherwise the best-scoring track in the preferred audio language
    // (commentary/descriptive tracks never win - see smartTracks).
    React.useEffect(() => {
        if (!defaultAudioTrackSelected.current) {
            const savedTrackId = player.streamState?.audioTrack?.id;
            const savedTrack = savedTrackId ? findTrackById(video.state.audioTracks, savedTrackId) : null;
            const audioTrack = savedTrack ?? pickAudioTrack(video.state.audioTracks, settings.audioLanguage);

            if (audioTrack && audioTrack.id) {
                video.setAudioTrack(audioTrack.id);
                defaultAudioTrackSelected.current = true;
            }
        }
    }, [video.state.audioTracks, player.streamState]);

    React.useEffect(() => {
        defaultAudioTrackSelected.current = false;
        nextVideoPopupDismissed.current = false;
        playingOnExternalDevice.current = false;
    }, [video.state.stream]);

    React.useEffect(() => {
        if (!Array.isArray(video.state.audioTracks) || video.state.audioTracks.length === 0) {
            closeAudioMenu();
        }
    }, [video.state.audioTracks]);

    React.useEffect(() => {
        if (video.state.playbackSpeed === null) {
            closeSpeedMenu();
        }
    }, [video.state.playbackSpeed]);

    React.useEffect(() => {
        const toastFilter = (item: any) => item?.dataset?.type === 'CoreEvent';
        toast.addFilter(toastFilter);
        const onCastStateChange = () => {
            setCasting(services.chromecast.active && services.chromecast.transport.getCastState() === cast.framework.CastState.CONNECTED);
        };
        const onChromecastServiceStateChange = () => {
            onCastStateChange();
            if (services.chromecast.active) {
                services.chromecast.transport.on(
                    cast.framework.CastContextEventType.CAST_STATE_CHANGED,
                    onCastStateChange
                );
            }
        };
        const onCoreEvent = (name: string) => {
            if (name === 'PlayingOnDevice') {
                playingOnExternalDevice.current = true;
                onPauseRequested();
            }
        };
        services.chromecast.on('stateChanged', onChromecastServiceStateChange);
        core.on('event', onCoreEvent);
        onChromecastServiceStateChange();
        return () => {
            toast.removeFilter(toastFilter);
            services.chromecast.off('stateChanged', onChromecastServiceStateChange);
            core.off('event', onCoreEvent);
            if (services.chromecast.active) {
                services.chromecast.transport.off(
                    cast.framework.CastContextEventType.CAST_STATE_CHANGED,
                    onCastStateChange
                );
            }
        };
    }, []);

    React.useEffect(() => {
        if (settings.pauseOnMinimize && (platform.shell.state.windowClosed || platform.shell.state.windowHidden)) {
            onPauseRequested();
        }
    }, [settings.pauseOnMinimize, platform.shell.state.windowClosed, platform.shell.state.windowHidden]);

    React.useEffect(() => {
        if (video.state.stream === null || typeof player?.title !== 'string') {
            discordTimestamps.current = EMPTY_DISCORD_TIMESTAMPS;
            discord.setActivity(null);
            return;
        }

        const metaItem = player.metaItem?.type === 'Ready' ? player.metaItem.content : null;
        const { activity, timestamps } = getPlaybackDiscordActivity({
            title: player.title,
            image: metaItem?.poster || metaItem?.background || null,
            paused: video.state.paused,
            time: video.state.time,
            duration: video.state.duration,
            timestamps: discordTimestamps.current,
        });

        discordTimestamps.current = timestamps;
        discord.setActivity(activity);
    }, [discord.setActivity, player?.title, player.metaItem, video.state.duration, video.state.paused, video.state.stream, video.state.time]);

    React.useEffect(() => {
        return () => {
            discord.setActivity(null);
        };
    }, [discord.setActivity]);

    useMediaSession(video.state, player, fullscreen, onPlayRequested, onPauseRequested, onNextVideoRequested);

    React.useEffect(() => {
        const onMediaKey = (action: string) => {
            switch (action) {
                case 'play-pause':
                    if (video.state.paused !== null) {
                        video.state.paused ? onPlayRequested() : onPauseRequested();
                    }
                    break;
                case 'play':
                    onPlayRequested();
                    break;
                case 'pause':
                    onPauseRequested();
                    break;
                case 'next-track':
                    if (player.nextVideo !== null) {
                        video.setTime(0);
                        onNextVideoRequested();
                    }
                    break;
            }
        };
        platform.shell.on('media-key', onMediaKey);
        return () => platform.shell.off('media-key', onMediaKey);
    }, [video.state.paused, player.nextVideo, onPlayRequested, onPauseRequested, onNextVideoRequested]);

    onShortcut('seekForward', (combo: number) => {
        if (video.state.time !== null) {
            const seekDuration = combo === 1 ? settings.seekShortTimeDuration : settings.seekTimeDuration;
            setSeeking(true);
            onSeekRequested(video.state.time + seekDuration);
        }
    }, [video.state.time, onSeekRequested], !menusOpen);

    onShortcut('seekBackward', (combo: number) => {
        if (video.state.time !== null) {
            const seekDuration = combo === 1 ? settings.seekShortTimeDuration : settings.seekTimeDuration;
            setSeeking(true);
            onSeekRequested(video.state.time - seekDuration);
        }
    }, [video.state.time, onSeekRequested], !menusOpen);

    onShortcut('mute', () => {
        video.state.muted === true ? onUnmuteRequested() : onMuteRequested();
    }, [video.state.muted], !menusOpen);

    onShortcut('volume', (combo: number) => {
        if (video.state.volume !== null) {
            const volume = combo === 0 ? Math.min(video.state.volume + 5, 200) : Math.max(video.state.volume - 5, 0);
            onVolumeChangeRequested(volume);
        }
    }, [video.state.volume], !menusOpen);

    onShortcut('audioMenu', () => {
        closeMenus();
        if (video.state?.audioTracks?.length > 0) {
            toggleAudioMenu();
        }
    }, [video.state.audioTracks, toggleAudioMenu]);

    onShortcut('infoMenu', () => {
        closeMenus();
        if (player.metaItem?.type === 'Ready') {
            toggleSideDrawer();
        }
    }, [player.metaItem, toggleSideDrawer]);

    onShortcut('speedMenu', () => {
        closeMenus();
        if (video.state.playbackSpeed !== null) {
            toggleSpeedMenu();
        }
    }, [video.state.playbackSpeed, toggleSpeedMenu]);

    onShortcut('speed', (combo: number) => {
        if (video.state.playbackSpeed !== null) {
            // Same bounds as the speed menu's custom control (0.1x..4x).
            const speed = combo === 0 ? Math.max(video.state.playbackSpeed - 0.25, MIN_SPEED) : Math.min(video.state.playbackSpeed + 0.25, MAX_SPEED);
            onPlaybackSpeedChanged(speed);
        }
    }, [video.state.playbackSpeed, onPlaybackSpeedChanged], !menusOpen);

    onShortcut('statisticsMenu', () => {
        closeMenus();
        if (player.selected?.stream) {
            toggleStatisticsMenu();
        }
    }, [player.selected, toggleStatisticsMenu]);

    onShortcut('playNext', () => {
        closeMenus();
        if (player.nextVideo !== null) {
            nextVideo();
            const deepLinks = player.nextVideo.deepLinks;
            handleNextVideoNavigation(deepLinks, false, false);
        }
    }, [player.nextVideo, handleNextVideoNavigation]);

    onShortcut('exit', () => {
        closeMenus();
        !settings.escExitFullscreen && navigate(-1);
    }, [settings.escExitFullscreen]);

    React.useLayoutEffect(() => {
        if (menusOpen) {
            if (pressTimer.current !== null) {
                clearTimeout(pressTimer.current);
            }
            pressTimer.current = null;
            longPress.current = false;
        }

        const onKeyDown = (e: KeyboardEvent) => {
            if (e.code !== 'Space' || e.repeat) return;
            if (menusOpen || e.ctrlKey || e.metaKey || e.altKey) return;

            // Space is the player's key here, so its default behavior must not
            // ALSO run: with any chrome button still focused from a click,
            // that default is a re-click of the button (a pause button click +
            // the pause toggle below = a double toggle; a window control =
            // minimize/close). Preventing the keydown also cancels the
            // browser's keyup click synthesis on the focused element.
            e.preventDefault();

            longPress.current = false;

            pressTimer.current = setTimeout(() => {
                longPress.current = true;
                onPlaybackSpeedChanged(2, true);
            }, HOLD_DELAY);
        };

        const onKeyUp = (e: KeyboardEvent) => {
            if (e.code !== 'Space' && e.code !== 'ArrowRight' && e.code !== 'ArrowLeft') return;
            if (e.ctrlKey || e.metaKey || e.altKey) return;

            if (e.code === 'ArrowRight' || e.code === 'ArrowLeft') {
                setSeeking(false);
                return;
            }
            if (e.code === 'Space') {
                if (pressTimer.current !== null) {
                    clearTimeout(pressTimer.current);
                }
                pressTimer.current = null;
                if (longPress.current) {
                    onPlaybackSpeedChanged(playbackSpeed.current);
                } else if (!menusOpen && video.state.paused !== null) {
                    if (video.state.paused) {
                        onPlayRequested();
                        setSeeking(false);
                    } else {
                        onPauseRequested();
                    }
                }
                longPress.current = false;
            }
        };

        const onWheel = ({ deltaY }: WheelEvent) => {
            if (menusOpen || video.state.volume === null) return;

            if (deltaY > 0) {
                onVolumeChangeRequested(Math.max(video.state.volume - 5, 0));
            } else {
                if (video.state.volume < 100) {
                    onVolumeChangeRequested(Math.min(video.state.volume + 5, 100));
                }
            }
        };

        const onMouseDownHold = (e: MouseEvent) => {
            if (e.button !== 0) return; // left mouse button only
            if (menusOpen) return;
            if (controlBarRef.current && controlBarRef.current.contains(e.target as Node)) return;

            longPress.current = false;

            pressTimer.current = setTimeout(() => {
                longPress.current = true;
                onPlaybackSpeedChanged(2, true);
            }, HOLD_DELAY);
        };

        const onMouseUp = (e: MouseEvent) => {
            if (e.button !== 0) return;

            if (pressTimer.current !== null) {
                clearTimeout(pressTimer.current);
            }

            if (longPress.current) {
                onPlaybackSpeedChanged(playbackSpeed.current);
            }
        };

        const onBlur = () => {
            if (pressTimer.current !== null) {
                clearTimeout(pressTimer.current);
            }
            pressTimer.current = null;
            if (longPress.current) {
                onPlaybackSpeedChanged(playbackSpeed.current);
                longPress.current = false;
            }
            setSeeking(false);
        };

        if (routeFocused) {
            window.addEventListener('keyup', onKeyUp);
            window.addEventListener('keydown', onKeyDown);
            window.addEventListener('wheel', onWheel);
            window.addEventListener('mousedown', onMouseDownHold);
            window.addEventListener('mouseup', onMouseUp);
            window.addEventListener('blur', onBlur);
        }
        return () => {
            window.removeEventListener('keyup', onKeyUp);
            window.removeEventListener('keydown', onKeyDown);
            window.removeEventListener('wheel', onWheel);
            window.removeEventListener('mousedown', onMouseDownHold);
            window.removeEventListener('mouseup', onMouseUp);
            window.removeEventListener('blur', onBlur);
        };
    }, [routeFocused, menusOpen, video.state.volume, video.state.paused]);

    React.useEffect(() => {
        video.events.on('error', onError);
        video.events.on('ended', onEnded);

        return () => {
            video.events.off('error', onError);
            video.events.off('ended', onEnded);
        };
    }, [onEnded]);

    React.useLayoutEffect(() => {
        return () => {
            setImmersedDebounced.cancel();
        };
    }, []);

    return (
        <div ref={playerRef} className={CONTAINER} data-immersed={overlayHidden ? '' : undefined}
            onMouseDown={onContainerMouseDown}
            onMouseMove={onContainerMouseMove}
            onMouseOver={onContainerMouseMove}
            onMouseLeave={onContainerMouseLeave}>
            <Video
                ref={video.containerRef}
                className={LAYER}
                onClick={onVideoClick}
            />
            {
                !video.state.loaded ?
                    <div className={LAYER}>
                        <img className={BACKGROUND_IMAGE} src={player?.metaItem?.content?.background} />
                    </div>
                    :
                    null
            }
            {
                (video.state.buffering || !video.state.loaded || dub.waiting) && !error ?
                    <Buffering
                        ref={bufferingRef}
                        className={LAYER}
                        logo={player?.metaItem?.content?.logo}
                        title={player?.selected?.stream?.name ?? null}
                        progress={statistics.progress}
                        infoHash={statistics.infoHash}
                        loaded={video.state.loaded}
                        hasStatistics={statisticsDetails !== null}
                        peers={statistics.peers}
                        speed={statistics.speed}
                        completed={statistics.completed}
                        escalated={slowDownload.escalated}
                        connectionSlow={slowDownload.connectionSlow}
                        fastModeAvailable={slowDownload.fastModeAvailable}
                        onTryDifferentSource={onTryDifferentSource}
                        onSwitchToFastMode={slowDownload.switchToFastMode}
                        // Held for the AI dub: the wait says so, with what is ready.
                        note={dub.waiting || (dubChosen && !dubPlaying) ?
                            { heading: t('PLAYER_GENERATING_DUB'), detail: t('PLAYER_GENERATING_DUB_READY', { seconds: Math.round(dub.aheadS) }) } :
                            null}
                    />
                    :
                    null
            }
            {
                error !== null ?
                    <Error
                        ref={errorRef}
                        className={LAYER}
                        stream={video.state.stream}
                        onTryDifferentSource={onTryDifferentSource}
                        {...error}
                    />
                    :
                    null
            }
            {
                menusOpen ?
                    <div className={LAYER} />
                    :
                    null
            }
            {
                video.state.volume !== null && overlayHidden ?
                    <VolumeChangeIndicator
                        muted={video.state.muted}
                        volume={video.state.volume}
                    />
                    :
                    null
            }
            <ContextMenu on={[video.containerRef, bufferingRef, errorRef]} autoClose>
                <OptionsMenu
                    className={MENU_LAYER}
                    stream={player?.selected?.stream}
                    playbackDevices={playbackDevices}
                    extraSubtitlesTracks={extraSubtitleTracks}
                    selectedExtraSubtitlesTrackId={selectedExtraSubtitleTrackId}
                />
            </ContextMenu>
            <TopBar
                className={NAV_BAR_LAYER}
                title={player.title !== null ? player.title : ''}
                hdrInfo={video.state.hdrInfo}
                onMouseMove={onBarMouseMove}
                onMouseOver={onBarMouseMove}
            />
            {
                // Hidden while the drawer or any menu is open: the chevron is an
                // opener, and open panels sit exactly where it points.
                player.metaItem?.type === 'Ready' && !sideDrawerOpen && !menusOpen ?
                    <SideDrawerButton
                        className={SIDE_DRAWER_BUTTON_LAYER}
                        onClick={toggleSideDrawer}
                    />
                    :
                    null
            }
            <ControlBar
                ref={controlBarRef}
                // Fades under the open side drawer: a Chromium compositing
                // limitation keeps the drawer's backdrop-filter from sampling
                // the chrome (it sits on its own composited layer for the
                // immersion fades), so it stays razor sharp through the frost.
                // It is unusable behind the drawer anyway - hide it.
                className={cn(
                    CONTROL_BAR_LAYER,
                    'transition-opacity duration-200',
                    sideDrawerOpen && 'pointer-events-none opacity-0',
                )}
                paused={video.state.paused}
                time={video.state.time}
                duration={video.state.duration}
                buffered={video.state.buffered}
                volume={video.state.volume}
                muted={video.state.muted}
                playbackSpeed={video.state.playbackSpeed}
                subtitlesTracks={allSubtitleTracks}
                audioTracks={video.state.audioTracks}
                metaItem={player.metaItem}
                nextVideo={player.nextVideo}
                stream={player.selected !== null ? player.selected.stream : null}
                thumbStreamUrl={
                    // rewriteServerUrl: the core serializes stream urls with the
                    // SYMBOLIC server port; on a fallback-port boot the trickplay
                    // shadow must be handed the bound one or every preview request
                    // dies on a dead socket (playback itself rides rillio:// and
                    // never sees this url).
                    typeof video.state.stream?.url === 'string' ? rewriteServerUrl(video.state.stream.url) : null
                }
                chapters={timelineChapters}
                onPlayRequested={onPlayRequested}
                onPauseRequested={onPauseRequested}
                onNextVideoRequested={onNextVideoRequested}
                onMuteRequested={onMuteRequested}
                onUnmuteRequested={onUnmuteRequested}
                onVolumeChangeRequested={onVolumeChangeRequested}
                onSeekRequested={onSeekRequested}
                onToggleOptionsMenu={toggleOptionsMenu}
                onToggleSubtitlesMenu={toggleSubtitlesMenu}
                onToggleAudioMenu={toggleAudioMenu}
                onToggleSpeedMenu={toggleSpeedMenu}
                videoScale={video.state.videoScale}
                videoScaleLabel={VIDEO_SCALE_LABELS[video.state.videoScale || 'contain']}
                onVideoScaleChanged={onVideoScaleChanged}
                onToggleStatisticsMenu={toggleStatisticsMenu}
                onToggleSideDrawer={toggleSideDrawer}
                onMouseMove={onBarMouseMove}
                onMouseOver={onBarMouseMove}
                onTouchEnd={onContainerMouseLeave}
            />
            <Indicator
                className={INDICATOR_LAYER}
                videoState={video.state}
                disabled={subtitlesMenuOpen}
            />
            {
                // Not gated on the chrome: the pill exists precisely for the
                // faded-out mid-intro moment. Menus take priority over it (they
                // occupy its corner), and casting has no local seek to serve.
                skipTarget !== null && !menusOpen && !casting ?
                    <SkipPill
                        segment={skipTarget}
                        onSkip={(segment) => onSeekRequested(Math.round(segment.endSec * 1000))}
                    />
                    :
                    null
            }
            {
                nextVideoPopupOpen ?
                    <NextVideoPopup
                        className={MENU_LAYER}
                        metaItem={player.metaItem !== null && player.metaItem.type === 'Ready' ? player.metaItem.content : null}
                        nextVideo={player.nextVideo}
                        onDismiss={onDismissNextVideoPopup}
                        onNextVideoRequested={onNextVideoRequested}
                    />
                    :
                    null
            }
            {
                nextEpisodePreload.promptVisible && !menusOpen && error === null ?
                    <NextEpisodePreloadPrompt
                        onAccept={nextEpisodePreload.accept}
                        onDismiss={nextEpisodePreload.dismiss}
                        onMouseMove={onBarMouseMove}
                        onMouseOver={onBarMouseMove}
                    />
                    :
                    null
            }
            {/*
              * The five player menu-layers below (Statistics / Subtitles / Audio / Speed /
              * Options) stay hand-rolled state-driven <div> panels rather than a menu/popover
              * primitive, and this is a researched KEEP, not inertia. Three constraints defeat
              * every 2026 trigger-menu primitive (Radix / Base UI / Ariakit):
              *   1. Fixed-position, NOT trigger-anchored: MENU_LAYER pins them bottom-right,
              *      never anchored to the ControlBar button that opens them, so a primitive's
              *      one real value (anchored positioning) is unused.
              *   2. Native-DOM-bubble close: they are DOM children of player-container and close
              *      via native mousedown bubbling to onContainerMouseDown (also the immersion
              *      driver), gated by the per-menu closePrevented nativeEvent flags. Any primitive
              *      that portals to body severs that native bubble and breaks the coupling.
              *   3. Single close arbiter: Radix DismissableLayer / Base UI outside-press fire
              *      their own close, racing the Player's menusOpen-gated close. The Player must be
              *      the sole arbiter. (Ariakit alone can be reduced to disable all of this, but
              *      then it only adds role="menu" plus a dependency while leaving close +
              *      immersion + positioning entirely in place - strictly worse.)
              * Revisit only if these menus ever stop needing container-mousedown close.
              */}
            {/*
              * Everything below carries the blurred-video backdrop: SnapshotBackdrop (a CPU
              * frame the shell ships us, gated off) and ShaderBlurRect (the panel's rect,
              * handed to a GPU shader in mpv, also gated off) - one layer each, inside each
              * panel. Both providers are scoped to exactly these panels rather than the
              * whole route: the context-menu OptionsMenu above is opened by right-click, is
              * not part of menusOpen, and so has nothing to blur - the context defaults
              * (null) leave it on plain dark glass, as today.
              */}
            <SnapshotBackdropContext.Provider value={snapshotBackdrop}>
                <ShaderBlurContext.Provider value={shaderBlur}>
                    <Presence when={statisticsMenuOpen}>
                        <StatisticsMenu
                            className={MENU_LAYER}
                            {...statistics}
                            details={statisticsDetails}
                        />
                    </Presence>
                    {
                        player.metaItem?.type === 'Ready' ?
                            <SideDrawer
                                open={sideDrawerOpen}
                                onClose={closeSideDrawer}
                                metaItem={player.metaItem?.content}
                                seriesInfo={player.seriesInfo as SeriesInfo}
                                selected={player.selected?.streamRequest?.path?.id as string}
                            />
                            :
                            null
                    }
                    <Presence when={subtitlesMenuOpen}>
                        <SubtitlesMenu
                            className={MENU_LAYER}
                            {...subtitlesMenuProps}
                        />
                    </Presence>
                    <Presence when={audioMenuOpen}>
                        <AudioMenu
                            className={MENU_LAYER}
                            audioTracks={video.state.audioTracks}
                            selectedAudioTrackId={video.state.selectedAudioTrackId}
                            audioDelay={video.state.audioDelay}
                            onAudioTrackSelected={onAudioTrackSelected}
                            onAudioDelayChanged={onAudioDelayChanged}
                            dub={dub}
                            dubChosen={dubChosen}
                            dubPlaying={dubPlaying}
                            onDubSelect={onDubSelect}
                        />
                    </Presence>
                    <Presence when={speedMenuOpen}>
                        <SpeedMenu
                            className={MENU_LAYER}
                            playbackSpeed={video.state.playbackSpeed}
                            onPlaybackSpeedChanged={onPlaybackSpeedChanged}
                        />
                    </Presence>
                    <Presence when={optionsMenuOpen}>
                        <OptionsMenu
                            className={MENU_LAYER}
                            stream={player.selected?.stream}
                            playbackDevices={playbackDevices}
                            extraSubtitlesTracks={extraSubtitleTracks}
                            selectedExtraSubtitlesTrackId={selectedExtraSubtitleTrackId}
                        />
                    </Presence>
                </ShaderBlurContext.Provider>
            </SnapshotBackdropContext.Provider>
        </div>
    );
};

const PlayerFallback = () => (
    <div className={CONTAINER} />
);

export default withCoreSuspender(Player, PlayerFallback);
