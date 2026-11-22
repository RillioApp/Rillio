// Copyright (C) 2017-2026 Smart code 203358507

// What one `player_thumb` answer means for the seek bar (thumbs.rs
// ThumbAnswer): the shell runs the shadow player, so it is the one that knows
// whether a frame is coming. This side keeps no budget of its own: it asks
// again while the answer is `pending` and the position is still hovered, and
// stops asking for the whole stream once it is `never`.
//
// The old contract (a frame or null, and the web giving up after a fixed 30
// tries) gave up while the shell was still making the frame: a hover that
// waited behind a sweep sample, or on a shadow that was opening, never showed
// the frame it got a moment later.
//
// Plain CommonJS so jest can require it (tests/thumbAnswer.spec.js);
// usePlayerThumb uses it.

const readThumbAnswer = (answer) => {
    if (answer !== null && typeof answer === 'object') {
        if (answer.state === 'frame' && typeof answer.dataUrl === 'string') {
            return { frame: answer.dataUrl, askAgain: false, never: null };
        }
        if (answer.state === 'pending') {
            return { frame: null, askAgain: true, never: null };
        }
        if (answer.state === 'never' && typeof answer.reason === 'string') {
            return { frame: null, askAgain: false, never: answer.reason };
        }
    }
    // FAIL LOUD: an answer this side does not understand is a contract break
    // between the shell and the web build, not a frame that is "coming".
    throw new Error(`player_thumb: unexpected answer ${JSON.stringify(answer)}`);
};

module.exports = { readThumbAnswer };
