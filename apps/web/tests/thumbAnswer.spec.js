// The seek bar keeps asking for a preview exactly while the shell says one is
// coming, and stops for the stream once the shell says it never will.

const { readThumbAnswer } = require('../src/routes/Player/ControlBar/SeekBar/thumbAnswer');

describe('readThumbAnswer', () => {
    it('shows a frame and stops asking', () => {
        expect(readThumbAnswer({ state: 'frame', dataUrl: 'data:image/jpeg;base64,AA' }))
            .toEqual({ frame: 'data:image/jpeg;base64,AA', askAgain: false, never: null });
    });

    it('keeps asking while pending, with no budget of its own', () => {
        // However many times the shell says pending, the answer is the same:
        // the web never gives up on a frame the shell is still making.
        for (let i = 0; i < 1000; i++) {
            expect(readThumbAnswer({ state: 'pending' })).toEqual({ frame: null, askAgain: true, never: null });
        }
    });

    it('stops for good, with the reason, when the shell says never', () => {
        expect(readThumbAnswer({ state: 'never', reason: 'thumbs: profile 5 without RPU' }))
            .toEqual({ frame: null, askAgain: false, never: 'thumbs: profile 5 without RPU' });
    });

    it('fails loud on an answer it does not understand (the old null included)', () => {
        for (const bad of [null, 'data:image/jpeg;base64,AA', {}, { state: 'frame' }, { state: 'never' }, { state: 'later' }]) {
            expect(() => readThumbAnswer(bad)).toThrow(/unexpected answer/);
        }
    });
});
