// Copyright (C) 2017-2026 Smart code 203358507

// The next-episode preload, offered behind a toast with a Cancel button.
// Jest-covered in tests/preloadOffer.spec.js; useNextEpisodePreload wires it
// to the real toast and to /cache/download.
//
// The download starts only once that toast has CLOSED (auto-close, or
// dismissed without Cancel), driven by the toast's own close event, never by a
// parallel timer: sonner pauses the auto-close while the pointer is over the
// toast, so a timer would again leave the Cancel button on screen after the
// download began. While the button can be pressed nothing has been sent, so a
// cancel is purely local: no request, nothing to undo, always true.
//
// States: waiting (toast up) -> started | cancelled | dropped (the episode
// changed or the player went away before the toast closed; the offer was for
// a "next episode" that is no longer next).

// stream: the next episode's torrent stream. start(stream): sends the
// download. showOffer({ onCancel, onClose }) shows the toast and returns its
// id, or null when nothing was shown (a toast filter suppressed it).
// onCancelled(): the toast's Cancel cancelled a waiting offer (optional).
// isCurrent(): whether the offer is still for the episode after the one
// playing NOW (optional; the hook keys it to the episode it was made for).
// Checked at the moment the close would start the download, so a close that
// lands after the episode changed but before anyone called drop() (a passive
// effect cleanup runs after paint; a toast timer can fire first) starts
// nothing: staleness is decided by the key, not by who runs first.
const offerPreload = ({ stream, start, showOffer, onCancelled, isCurrent }) => {
    let state = 'waiting';
    const offer = {
        get state() {
            return state;
        },
        toastId: null,
        // The Cancel button. True when this call cancelled a waiting offer;
        // false when it was too late (already started) or already over.
        cancel() {
            if (state !== 'waiting') return false;
            state = 'cancelled';
            return true;
        },
        // The episode moved on: never start. Call BEFORE removing the toast,
        // whose close event would otherwise start the download.
        drop() {
            if (state === 'waiting') state = 'dropped';
        },
    };
    const closed = () => {
        if (state !== 'waiting') return;
        if (typeof isCurrent === 'function' && !isCurrent()) {
            state = 'dropped';
            return;
        }
        state = 'started';
        start(stream);
    };
    const cancelFromToast = () => {
        if (offer.cancel() && typeof onCancelled === 'function') onCancelled();
    };
    offer.toastId = showOffer({ onCancel: cancelFromToast, onClose: closed });
    if (offer.toastId === null) {
        // No toast, no Cancel to wait for.
        closed();
    }
    return offer;
};

// An accept is live (a second one must be refused) while it waits or runs.
const isActive = (offer) => offer !== null && offer !== undefined &&
    (offer.state === 'waiting' || offer.state === 'started');

module.exports = { offerPreload, isActive };
