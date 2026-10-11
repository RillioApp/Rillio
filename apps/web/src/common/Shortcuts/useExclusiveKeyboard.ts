import { useEffect } from 'react';
import { useShortcuts } from './Shortcuts';
import { useRouteOnTop } from '../useRouteFocused';

// A route calls this to say "while I am the top route, every key is mine":
// the app's own shortcuts (tab digits, 0 for search, Backspace history,
// Ctrl+/ and Ctrl+G) then do nothing, see dispatchShortcut.js. Shortcuts the
// route binds itself with onShortcut keep working, and so does fullscreen.
// The player claims it; any other full-screen route can do the same.
const useExclusiveKeyboard = () => {
    const shortcuts = useShortcuts();
    const routeOnTop = useRouteOnTop();

    useEffect(() => {
        if (!routeOnTop) return;
        return shortcuts.claimKeyboard();
    }, [routeOnTop]);
};

export default useExclusiveKeyboard;
