// The next-episode preload starts only once its "Preloading next episode"
// toast has CLOSED, so its Cancel button can never be pressed after the
// download began: cancel is purely local and never needs a request.

const { offerPreload, isActive } = require('../src/routes/Player/preloadOffer');

const STREAM = { infoHash: 'a'.repeat(40), fileIdx: 2 };

// A stand-in for the toast: records the callbacks the offer registered, so a
// test can press Cancel or close it the way the real adapter does (an action
// click runs onCancel, then onClose; auto-close and dismiss run onClose).
const fakeToast = (id = 'toast-1') => {
    const toast = { shown: 0, onCancel: null, onClose: null };
    toast.show = ({ onCancel, onClose }) => {
        toast.shown += 1;
        toast.onCancel = onCancel;
        toast.onClose = onClose;
        return id;
    };
    toast.pressCancel = () => { toast.onCancel(); toast.onClose(); };
    toast.close = () => toast.onClose();
    return toast;
};

describe('next-episode preload offer', () => {
    it('sends nothing while the offer toast is open', () => {
        const toast = fakeToast();
        const start = jest.fn();
        const offer = offerPreload({ stream: STREAM, start, showOffer: toast.show });
        expect(toast.shown).toBe(1);
        expect(start).not.toHaveBeenCalled();
        expect(offer.state).toBe('waiting');
    });

    it('cancel sends nothing, ever: not on the click, not when the toast then closes', () => {
        const toast = fakeToast();
        const start = jest.fn();
        const offer = offerPreload({ stream: STREAM, start, showOffer: toast.show });
        toast.pressCancel();
        expect(start).not.toHaveBeenCalled();
        expect(offer.state).toBe('cancelled');
        // A later close event (sonner's own dismiss) changes nothing.
        toast.close();
        expect(start).not.toHaveBeenCalled();
    });

    it('the toast closing without Cancel starts the download exactly once', () => {
        const toast = fakeToast();
        const start = jest.fn();
        const offer = offerPreload({ stream: STREAM, start, showOffer: toast.show });
        toast.close();
        toast.close();
        expect(start).toHaveBeenCalledTimes(1);
        expect(start).toHaveBeenCalledWith(STREAM);
        expect(offer.state).toBe('started');
    });

    it('a Cancel that arrives after the start does not claim to have cancelled', () => {
        const toast = fakeToast();
        const start = jest.fn();
        const offer = offerPreload({ stream: STREAM, start, showOffer: toast.show });
        toast.close();
        expect(offer.cancel()).toBe(false);
        expect(offer.state).toBe('started');
        expect(start).toHaveBeenCalledTimes(1);
    });

    it('a cancel before the close reports that it cancelled', () => {
        const toast = fakeToast();
        const offer = offerPreload({ stream: STREAM, start: jest.fn(), showOffer: toast.show });
        expect(offer.cancel()).toBe(true);
        expect(offer.cancel()).toBe(false);
    });

    // The episode changed or the player unmounted while the toast was up: the
    // offer was for a next episode that is no longer next. The hook drops it,
    // then removes the toast, whose close must not start anything.
    it('a dropped offer never starts, even when its toast closes afterwards', () => {
        const toast = fakeToast();
        const start = jest.fn();
        const offer = offerPreload({ stream: STREAM, start, showOffer: toast.show });
        offer.drop();
        toast.close();
        expect(start).not.toHaveBeenCalled();
        expect(offer.state).toBe('dropped');
    });

    // A toast filter suppressed the offer: there is no Cancel to press, so
    // nothing to wait for.
    it('an offer whose toast was never shown starts at once', () => {
        const start = jest.fn();
        const offer = offerPreload({ stream: STREAM, start, showOffer: () => null });
        expect(start).toHaveBeenCalledTimes(1);
        expect(offer.state).toBe('started');
    });

    it('a second accept is refused while an offer is waiting or started', () => {
        const toast = fakeToast();
        expect(isActive(null)).toBe(false);
        const offer = offerPreload({ stream: STREAM, start: jest.fn(), showOffer: toast.show });
        expect(isActive(offer)).toBe(true);
        toast.close();
        expect(isActive(offer)).toBe(true);
        const cancelled = offerPreload({ stream: STREAM, start: jest.fn(), showOffer: fakeToast().show });
        cancelled.cancel();
        expect(isActive(cancelled)).toBe(false);
    });
});
