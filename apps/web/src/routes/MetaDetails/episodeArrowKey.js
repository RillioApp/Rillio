// Copyright (C) 2017-2026 Smart code 203358507

// Which way, if any, a keydown steps the streams view's episode arrows.
// Plain JS so jest loads it directly (jest-covered in
// tests/episodeArrowKey.spec.js); EpisodeArrows.tsx owns the listener and
// reads the page state it passes in.
//
// `routeFocused` is useRouteFocused(): the listener sits on `document` and the
// router keeps this view mounted under the player, where Left/Right seek, so
// a covered view must not step (it did in 0.1.47: one press both seeked and
// changed the episode on the page below). `focusFree` is "nothing on the
// page holds focus, or one of the arrows does"; `dialogOpen` is "a dialog is
// on screen". Returns 'previous', 'next' or null (the key is not the arrows'
// to take).

const episodeArrowKey = (event, { routeFocused, focusFree, dialogOpen }) => {
    if (!routeFocused) return null;
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return null;
    if (event.defaultPrevented || event.repeat) return null;
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return null;
    if (!focusFree || dialogOpen) return null;
    return event.key === 'ArrowLeft' ? 'previous' : 'next';
};

module.exports = { episodeArrowKey };
