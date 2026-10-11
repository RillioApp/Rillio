// While a route claims the keyboard for itself (the player), a shortcut still
// reaches the listeners bound to it (the player's own keys, fullscreen) but
// never the app's own handler (tab digits, search, history, the guides).

const { dispatchShortcut } = require('../src/common/Shortcuts/dispatchShortcut');
const groups = require('../src/common/Shortcuts/shortcuts.json');

const SHORTCUTS = groups.map(({ shortcuts }) => shortcuts).flat();

const press = (key, code = key, extra = {}) => ({
    key,
    code,
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    ...extra,
});

const APP_KEYS = [
    ['1', 'Digit1'], ['2', 'Digit2'], ['3', 'Digit3'], ['4', 'Digit4'], ['5', 'Digit5'], ['6', 'Digit6'],
    ['0', 'Digit0'],
    ['Backspace', 'Backspace'],
    ['Backspace', 'Backspace', { ctrlKey: true }],
    ['/', 'Slash', { ctrlKey: true }],
    ['g', 'KeyG', { ctrlKey: true }],
];

describe('dispatchShortcut', () => {
    it('hands a match to the bound listeners and to the app handler', () => {
        const seek = jest.fn();
        const onShortcut = jest.fn();
        const listeners = new Map([['seekForward', new Set([seek])]]);
        dispatchShortcut(SHORTCUTS, press('ArrowRight', 'ArrowRight', { shiftKey: true }), { listeners, onShortcut, exclusive: false });
        expect(seek).toHaveBeenCalledWith(1, 'ArrowRight');
        expect(onShortcut).toHaveBeenCalledWith('seekForward', 1, 'ArrowRight');
    });

    it('reaches the app handler for every app key while no route claims the keyboard', () => {
        const onShortcut = jest.fn();
        APP_KEYS.forEach(([key, code, extra]) => {
            dispatchShortcut(SHORTCUTS, press(key, code, extra), { listeners: new Map(), onShortcut, exclusive: false });
        });
        expect(onShortcut.mock.calls.map(([name, combo, key]) => `${name}:${combo}:${key}`)).toEqual([
            'navigateTabs:0:1', 'navigateTabs:0:2', 'navigateTabs:0:3', 'navigateTabs:0:4', 'navigateTabs:0:5', 'navigateTabs:0:6',
            'navigateSearch:0:0',
            'navigateHistory:0:Backspace',
            'navigateHistory:1:Backspace',
            'shortcuts:0:/',
            'gamepadGuide:0:g',
        ]);
    });

    it('keeps every key away from the app handler while a route claims the keyboard', () => {
        const onShortcut = jest.fn();
        APP_KEYS.forEach(([key, code, extra]) => {
            dispatchShortcut(SHORTCUTS, press(key, code, extra), { listeners: new Map(), onShortcut, exclusive: true });
        });
        expect(onShortcut).not.toHaveBeenCalled();
    });

    it('still reaches bound listeners while a route claims the keyboard', () => {
        const fullscreen = jest.fn();
        const exit = jest.fn();
        const leave = jest.fn();
        const onShortcut = jest.fn();
        const listeners = new Map([
            ['fullscreen', new Set([fullscreen])],
            ['exit', new Set([exit])],
            ['navigateHistory', new Set([leave])],
        ]);
        const context = { listeners, onShortcut, exclusive: true };
        dispatchShortcut(SHORTCUTS, press('f', 'KeyF'), context);
        dispatchShortcut(SHORTCUTS, press('Escape', 'Escape'), context);
        dispatchShortcut(SHORTCUTS, press('Backspace', 'Backspace'), context);
        expect(fullscreen).toHaveBeenCalledTimes(1);
        expect(exit).toHaveBeenCalledTimes(1);
        expect(leave).toHaveBeenCalledWith(0, 'Backspace');
        expect(onShortcut).not.toHaveBeenCalled();
    });
});
