import { DependencyList, useCallback, useEffect } from 'react';
import { ShortcutListener, ShortcutName, useShortcuts } from './Shortcuts';
import { useRouteOnTop } from '../useRouteFocused';

const onShortcut = (name: ShortcutName, listener: ShortcutListener, deps: DependencyList, enabled = true) => {
    const shortcuts = useShortcuts();
    // The service listens on document, so a shortcut bound from inside a route
    // would still fire while that route sits mounted under another one. Bound
    // outside any route (the fullscreen key) this is always true.
    const routeOnTop = useRouteOnTop();

    const listenerCallback = useCallback(listener, deps);

    useEffect(() => {
        if (!enabled || !routeOnTop) return;
        shortcuts.on(name, listenerCallback);
        return () => shortcuts.off(name, listenerCallback);
    }, [listenerCallback, enabled, routeOnTop]);
};

export default onShortcut;
