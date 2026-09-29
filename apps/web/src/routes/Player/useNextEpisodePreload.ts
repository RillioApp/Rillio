// Copyright (C) 2017-2026 Smart code 203358507

import React from 'react';
import useCacheDownload from 'rillio/common/useCacheDownload';
import useToast from 'rillio/common/Toast/useToast';
import { getPreloadPromptEnabled } from 'rillio/common/nextEpisodePreloadPrefs';

// The offer behind the toast (jest-covered in tests/preloadOffer.spec.js).
const { offerPreload, isActive } = require('./preloadOffer');

// Offers to preload the NEXT episode's torrent into the local cache while the
// current one plays, so the binge transition starts instantly. The prompt shows
// twice at most: once at episode start (through initial loading plus the first
// 30s of playback, whichever lasts longer) and once 10 minutes before the end.
// Accepting hides the prompt for the rest of the episode and shows a toast
// with a Cancel button; the download starts only once that toast has CLOSED
// (see preloadOffer.js), so a cancel never has anything on the server to undo
// (cancelling aborts for this episode only). Cancel on the prompt itself just
// hides the currently showing slot, nothing persisted: the T-minus-10min
// reminder can still appear later in the same episode, and the next episode
// prompts again; the Settings toggle is what turns it off globally.
// Torrent-only: a next episode without a derivable torrent stream simply
// never prompts.

// The start prompt stays up through initial loading plus this long into
// playback, whichever lasts longer.
const START_PROMPT_PLAYBACK_MS = 30000;
// How long the offer toast (and its Cancel) stays up before the download
// starts. Sonner holds it longer while the pointer is over it; the download
// waits for the actual close either way.
const CANCEL_TOAST_TIMEOUT_MS = 6000;
// The reminder prompt appears this close to the end of the episode.
const END_PROMPT_REMAINING_MS = 10 * 60 * 1000;
// An armed paused-start that is never consumed (user backed out mid-transition)
// must not leak into an unrelated playback hours later.
const PAUSED_START_TTL_MS = 30 * 60 * 1000;

type PendingPausedStart = {
    videoId: string;
    expiresAt: number;
};

type UseNextEpisodePreloadArgs = {
    player: {
        selected: { streamRequest?: ResourceRequest | null; metaRequest?: ResourceRequest | null } | null;
        nextVideo: { streams?: Stream[]; id?: string } | null;
    };
    video: {
        state: any;
        setPaused: (paused: boolean) => void;
    };
};

// The paused-start handoff must survive the player route replacing itself with
// the next episode's URL (React may remount the component on navigation), so it
// lives at module scope, not in component state. SPA-session-scoped by design.
let pendingPausedStart: PendingPausedStart | null = null;

const readPendingPausedStart = (): PendingPausedStart | null => {
    if (pendingPausedStart !== null && Date.now() > pendingPausedStart.expiresAt) {
        pendingPausedStart = null;
    }
    return pendingPausedStart;
};

const useNextEpisodePreload = ({ player, video }: UseNextEpisodePreloadArgs) => {
    const toast = useToast();
    const downloadToCache = useCacheDownload();

    const currentVideoId = player.selected?.streamRequest?.path?.id ?? null;
    const seriesMetaId = player.selected?.metaRequest?.path?.id ?? null;

    // The preloadable stream: the core injects the binge-group matched stream as
    // the next video's ONLY stream (crates/core models/player.rs
    // next_video_update), so mirror core's Video::stream() rule (exactly one
    // stream) and require an infoHash, /cache/download is torrent-only.
    const nextStream = React.useMemo(() => {
        const streams = player.nextVideo?.streams;
        if (!Array.isArray(streams) || streams.length !== 1) {
            return null;
        }
        return typeof streams[0].infoHash === 'string' ? streams[0] : null;
    }, [player.nextVideo]);

    // answered = accepted for this episode; it silences both prompt slots
    // (start + T-minus-10min). Cancel never sets it.
    const [answered, setAnswered] = React.useState(false);
    const [accepted, setAccepted] = React.useState(false);
    // Cancel pressed while the end-window reminder was showing. Without this
    // the cancelled prompt would reappear on the next tick, inEndWindow stays
    // true for the remaining minutes. Episode-scoped, reset on a new episode.
    const [endWindowDismissed, setEndWindowDismissed] = React.useState(false);
    // The accepted preload: waiting behind its toast, started, cancelled or
    // dropped (see preloadOffer.js). null = nothing accepted this episode.
    const offerRef = React.useRef<any>(null);
    const [startWindowOpen, setStartWindowOpen] = React.useState(true);
    const startHideTimer = React.useRef<ReturnType<typeof setTimeout> | null>(null);
    // Guards the paused-start against a stale loaded=true from the PREVIOUS
    // episode: consume only after observing not-loaded for the current one.
    const sawUnloaded = React.useRef(false);
    const prevVideoId = React.useRef<string | null>(currentVideoId);

    const clearStartHideTimer = () => {
        if (startHideTimer.current !== null) {
            clearTimeout(startHideTimer.current);
            startHideTimer.current = null;
        }
    };

    // An offer still waiting behind its toast when the episode changes or the
    // player unmounts is DROPPED, not carried over: it was for the episode
    // after the one playing then, which is no longer "next" (and after an
    // auto-next it is the one now playing, which the player fetches anyway).
    // Dropped before its toast is removed, because that removal fires the
    // close that would otherwise start it. A preload that already started is
    // left alone: it is an ordinary pinned download by then.
    const dropOffer = () => {
        const offer = offerRef.current;
        offerRef.current = null;
        if (offer === null || offer.state !== 'waiting') return;
        offer.drop();
        if (offer.toastId !== null) toast.remove(offer.toastId);
    };

    // A new episode gets a fresh prompt evaluation.
    React.useEffect(() => {
        setAnswered(false);
        setAccepted(false);
        setEndWindowDismissed(false);
        setStartWindowOpen(true);
        sawUnloaded.current = false;
        dropOffer();
        clearStartHideTimer();
    }, [currentVideoId]);

    React.useEffect(() => () => {
        clearStartHideTimer();
        dropOffer();
    }, []);

    // The start window closes 30s after playback becomes possible (loaded), so
    // it spans the whole initial loading phase plus the first 30s of playback.
    React.useEffect(() => {
        if (video.state.loaded === true && startWindowOpen && startHideTimer.current === null) {
            startHideTimer.current = setTimeout(() => {
                startHideTimer.current = null;
                setStartWindowOpen(false);
            }, START_PROMPT_PLAYBACK_MS);
        }
    }, [video.state.loaded, startWindowOpen]);

    React.useEffect(() => {
        if (video.state.loaded !== true) {
            sawUnloaded.current = true;
        }
    }, [video.state.loaded, currentVideoId]);

    // A pending paused-start armed for one episode must not fire if the user
    // ends up playing something else instead.
    React.useEffect(() => {
        if (prevVideoId.current === currentVideoId) {
            return;
        }
        prevVideoId.current = currentVideoId;
        const pending = readPendingPausedStart();
        if (pending !== null && currentVideoId !== null && currentVideoId !== pending.videoId) {
            pendingPausedStart = null;
        }
    }, [currentVideoId]);

    // The accepted-preload transition: on the first loaded signal of the next
    // episode, start it paused and explain why. If it is not buffered yet the
    // normal Initializing screen shows in the meantime, then we still pause.
    React.useEffect(() => {
        const pending = readPendingPausedStart();
        if (pending !== null &&
            currentVideoId !== null &&
            currentVideoId === pending.videoId &&
            video.state.loaded === true &&
            sawUnloaded.current) {
            pendingPausedStart = null;
            video.setPaused(true);
            toast.show({
                type: 'success',
                title: 'Next episode is ready, just paused',
                timeout: 4000,
            });
        }
    }, [video.state.loaded, currentVideoId]);

    // Enablement is read from localStorage once per episode; flipping the
    // Settings toggle applies from the next episode on.
    const eligible = React.useMemo(() => {
        return nextStream !== null &&
            seriesMetaId !== null &&
            getPreloadPromptEnabled();
    }, [nextStream, seriesMetaId, currentVideoId]);

    const remainingMs = typeof video.state.time === 'number' && typeof video.state.duration === 'number' && video.state.duration > 0 ?
        video.state.duration - video.state.time
        :
        null;
    const inEndWindow = remainingMs !== null && remainingMs > 0 && remainingMs <= END_PROMPT_REMAINING_MS;

    const promptVisible = eligible && !answered &&
        (startWindowOpen || (inEndWindow && !endWindowDismissed));

    // Accept hides the prompt immediately (answered silences both prompt
    // slots), arms the accepted state and offers a toast with a Cancel button.
    // The download starts when that toast CLOSES without Cancel (auto-close, a
    // dismiss, a swipe, another caller clearing toasts): from then on Cancel
    // can no longer be pressed (the toast adapter kills its action once the
    // toast closes). So Cancel only ever withdraws an offer nothing has acted
    // on yet: local, synchronous, no request, and "Preload cancelled" is always
    // true. A second accept while one waits or runs is ignored.
    const accept = React.useCallback(() => {
        if (isActive(offerRef.current)) {
            return;
        }
        setAnswered(true);
        setAccepted(true);
        offerRef.current = offerPreload({
            stream: nextStream,
            // useCacheDownload POSTs { infoHash, fileIdx } to /cache/download and
            // pins the torrent; it owns the started/failed toasts.
            start: (stream: Stream) => {
                if (!downloadToCache(stream)) {
                    // Should be unreachable: the prompt only shows for a torrent
                    // stream. useCacheDownload raises its own error toast.
                    console.error('useNextEpisodePreload: accepted but the next stream is not downloadable', stream);
                }
            },
            showOffer: ({ onCancel, onClose }: { onCancel: () => void; onClose: () => void }) => toast.show({
                type: 'success',
                title: 'Preloading next episode',
                message: 'Starting in a few seconds.',
                timeout: CANCEL_TOAST_TIMEOUT_MS,
                action: { label: 'Cancel', onSelect: onCancel },
                onClose,
            }),
            // Nothing was sent, so there is nothing to undo: disarm and say so.
            // No re-prompt this episode (answered stays true).
            onCancelled: () => {
                setAccepted(false);
                pendingPausedStart = null;
                toast.show({ type: 'success', title: 'Preload cancelled', timeout: 3000 });
            },
        });
    }, [nextStream, downloadToCache, toast]);

    // Cancel from the prompt: just hide whatever slot is currently showing,
    // nothing persisted. Closing the start window early never blocks the
    // T-minus-10min reminder; hiding the end-window reminder only applies
    // while it is actually showing (both can be true near the end of a short
    // episode, then a single Cancel closes both, the prompt was one).
    const dismiss = React.useCallback(() => {
        clearStartHideTimer();
        setStartWindowOpen(false);
        if (inEndWindow) {
            setEndWindowDismissed(true);
        }
    }, [inEndWindow]);

    // Called by Player.onEnded right before navigating to the next episode when
    // the preload was accepted, so THAT load starts paused. Only a preload that
    // actually started: one still waiting behind its toast is dropped by the
    // episode change, so nothing is "ready" to announce.
    const armPausedStart = React.useCallback(() => {
        if (offerRef.current === null || offerRef.current.state !== 'started') {
            return;
        }
        if (player.nextVideo !== null && typeof player.nextVideo.id === 'string') {
            pendingPausedStart = {
                videoId: player.nextVideo.id,
                expiresAt: Date.now() + PAUSED_START_TTL_MS,
            };
        }
    }, [player.nextVideo]);

    return { promptVisible, accepted, accept, dismiss, armPausedStart };
};

export default useNextEpisodePreload;
