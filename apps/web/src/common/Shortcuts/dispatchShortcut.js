// Who hears a key press that matches a shortcut. Plain JS so jest loads it
// directly (jest-covered in tests/dispatchShortcut.spec.js); Shortcuts.tsx
// owns the document listener and calls this per keydown.
//
// `shortcuts` is the flat list from shortcuts.json. Every match goes to the
// listeners bound to that name (onShortcut users: the player's keys, the
// fullscreen key) and then to `onShortcut`, the app's own handler (tab
// digits, search, history, the shortcuts and gamepad guides).

const dispatchShortcut = (shortcuts, { ctrlKey, shiftKey, altKey, metaKey, code, key }, { listeners, onShortcut }) => {
    shortcuts.forEach(({ name, combos }) => combos.forEach((keys) => {
        const modifers = (keys.includes('Ctrl') === ctrlKey)
            && (keys.includes('Shift') === shiftKey)
            && !altKey
            && !metaKey;

        if (modifers && (keys.includes(code) || keys.includes(key.toUpperCase()))) {
            const combo = combos.indexOf(keys);
            const bound = listeners.get(name);
            if (bound !== undefined) {
                bound.forEach((listener) => listener(combo, key));
            }

            onShortcut(name, combo, key);
        }
    }));
};

module.exports = { dispatchShortcut };
