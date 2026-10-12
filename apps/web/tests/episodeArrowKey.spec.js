// Left/Right step the streams view's episode arrows only while that view is
// the top route: the router keeps it mounted under the player, where the
// same keys seek.

const { episodeArrowKey } = require('../src/routes/MetaDetails/episodeArrowKey');

const key = (name, extra = {}) => ({
    key: name,
    defaultPrevented: false,
    repeat: false,
    altKey: false,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    ...extra,
});

const page = (extra = {}) => ({ routeFocused: true, focusFree: true, dialogOpen: false, ...extra });

describe('episodeArrowKey', () => {
    it('steps on a bare Left/Right while the route is on top and nothing else holds focus', () => {
        expect(episodeArrowKey(key('ArrowLeft'), page())).toBe('previous');
        expect(episodeArrowKey(key('ArrowRight'), page())).toBe('next');
    });

    it('does nothing while another route (the player) is on top', () => {
        expect(episodeArrowKey(key('ArrowLeft'), page({ routeFocused: false }))).toBeNull();
        expect(episodeArrowKey(key('ArrowRight'), page({ routeFocused: false }))).toBeNull();
    });

    it('leaves the key alone when something else on the page holds focus or a dialog is open', () => {
        expect(episodeArrowKey(key('ArrowRight'), page({ focusFree: false }))).toBeNull();
        expect(episodeArrowKey(key('ArrowRight'), page({ dialogOpen: true }))).toBeNull();
    });

    it('ignores other keys, repeats, modified and already-handled presses', () => {
        expect(episodeArrowKey(key('ArrowUp'), page())).toBeNull();
        expect(episodeArrowKey(key('ArrowRight', { repeat: true }), page())).toBeNull();
        expect(episodeArrowKey(key('ArrowRight', { shiftKey: true }), page())).toBeNull();
        expect(episodeArrowKey(key('ArrowRight', { ctrlKey: true }), page())).toBeNull();
        expect(episodeArrowKey(key('ArrowRight', { defaultPrevented: true }), page())).toBeNull();
    });
});
