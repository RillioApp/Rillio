// Copyright (C) 2017-2026 Smart code 203358507

// The deliberate update path (Settings -> Check for updates). The shell's
// launch-time check only speaks through a toast: miss it and there is no way to
// install an update short of restarting the app, or downloading the installer
// by hand. This drives the same `install_update` command the toast does.
//
// Desktop shell only: `available` is false in a browser and on Android (which
// takes updates from the store), so the caller renders nothing there.

import { useCallback, useState } from 'react';
import { getTauri } from 'rillio/common/Platform/shell/isShell';

type Status =
    | { phase: 'idle' }
    | { phase: 'checking' }
    | { phase: 'up-to-date' }
    | { phase: 'available', version: string }
    | { phase: 'installing' }
    // `action` says WHICH step failed: an install failure used to read "Could
    // not check for updates". `message` is the shell's full cause chain.
    | { phase: 'failed', action: 'check' | 'install', message: string };

const useUpdateCheck = () => {
    const [status, setStatus] = useState<Status>({ phase: 'idle' });
    const available = getTauri()?.core?.invoke !== undefined;

    const check = useCallback(() => {
        const TAURI = getTauri();
        if (!TAURI?.core?.invoke) return;
        setStatus({ phase: 'checking' });
        TAURI.core.invoke('check_for_update')
            .then((version: unknown) => {
                setStatus(typeof version === 'string' && version.length > 0 ?
                    { phase: 'available', version }
                    :
                    { phase: 'up-to-date' }
                );
            })
            // A user who explicitly asked must be told the CHECK failed. Silently
            // showing "up to date" when we could not reach GitHub is a lie that
            // leaves them on an old build believing it is current.
            .catch((error: unknown) => {
                console.error('useUpdateCheck: the update check failed', error);
                setStatus({ phase: 'failed', action: 'check', message: String(error) });
            });
    }, []);

    const install = useCallback(() => {
        const TAURI = getTauri();
        if (!TAURI?.core?.invoke) return;
        setStatus({ phase: 'installing' });
        // The shell owns the rest: it hides this window, shows the update
        // splash, and relaunches. On failure it re-shows the window, so the
        // error state below is what the user comes back to.
        TAURI.core.invoke('install_update').catch((error: unknown) => {
            console.error('useUpdateCheck: installing the update failed', error);
            setStatus({ phase: 'failed', action: 'install', message: String(error) });
        });
    }, []);

    return { available, status, check, install };
};

export default useUpdateCheck;
