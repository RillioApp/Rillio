import React, { createContext, useCallback, useContext, useEffect, useRef } from 'react';
import shortcuts from './shortcuts.json';

const { dispatchShortcut } = require('./dispatchShortcut');

const SHORTCUTS = shortcuts.map(({ shortcuts }) => shortcuts).flat();

export type ShortcutName = string;
export type ShortcutListener = (combo: number, key: string) => void;

interface ShortcutsContext {
    grouped: ShortcutGroup[],
    on: (name: ShortcutName, listener: ShortcutListener) => void,
    off: (name: ShortcutName, listener: ShortcutListener) => void,
    claimKeyboard: () => () => void,
}

const ShortcutsContext = createContext<ShortcutsContext>({} as ShortcutsContext);

type Props = {
    children: JSX.Element,
    onShortcut: (name: ShortcutName, combo: number, key: string) => void,
};

const REPEAT_THROTTLE_MS = 130;

const isInputFocused = () => {
    const inputElements = ['INPUT', 'TEXTAREA', 'SELECT'];
    const activeElement = document.activeElement;

    return activeElement instanceof HTMLElement &&
        (inputElements.includes(activeElement.tagName) || activeElement.isContentEditable);
};

const ShortcutsProvider = ({ children, onShortcut }: Props) => {
    const listeners = useRef<Map<ShortcutName, Set<ShortcutListener>>>(new Map());
    const lastRepeatTime = useRef<Map<string, number>>(new Map());
    const keyboardClaims = useRef<Set<object>>(new Set());

    const onKeyDown = useCallback((event: KeyboardEvent) => {
        if (isInputFocused()) return;

        if (event.repeat) {
            const now = Date.now();
            const last = lastRepeatTime.current.get(event.code) ?? 0;
            if (now - last < REPEAT_THROTTLE_MS) return;
            lastRepeatTime.current.set(event.code, now);
        }

        dispatchShortcut(SHORTCUTS, event, {
            listeners: listeners.current,
            onShortcut,
            exclusive: keyboardClaims.current.size > 0,
        });
    }, [onShortcut]);

    // See useExclusiveKeyboard. Returns the release.
    const claimKeyboard = () => {
        const claim = {};
        keyboardClaims.current.add(claim);
        return () => {
            keyboardClaims.current.delete(claim);
        };
    };

    const on = (name: ShortcutName, listener: ShortcutListener) => {
        !listeners.current.has(name) && listeners.current.set(name, new Set());
        listeners.current.get(name)!.add(listener);
    };

    const off = (name: ShortcutName, listener: ShortcutListener) => {
        listeners.current.get(name)?.delete(listener);
    };

    useEffect(() => {
        document.addEventListener('keydown', onKeyDown);
        return () => document.removeEventListener('keydown', onKeyDown);
    }, [onKeyDown]);

    return (
        <ShortcutsContext.Provider value={{ grouped: shortcuts, on, off, claimKeyboard }}>
            {children}
        </ShortcutsContext.Provider>
    );
};

const useShortcuts = () => {
    return useContext(ShortcutsContext);
};

export {
    ShortcutsProvider,
    useShortcuts,
};
