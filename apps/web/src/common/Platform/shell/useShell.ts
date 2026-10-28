import { useEffect, useState } from 'react';
import EventEmitter from 'eventemitter3';
import { useTauriApi } from './isShell';

const IPC = globalThis?.chrome?.webview;
const LEGACY_IPC = globalThis?.qt?.webChannelTransport;
if (LEGACY_IPC) LEGACY_IPC.onmessage = () => { /* empty */ };

// Desktop shell (Tauri): when running inside the native shell we carry the
// ShellVideo IPC over Tauri's invoke/event bridge instead of chrome.webview.
// The native side (apps/desktop src/shell.rs) drives libmpv, so playback is
// fully native (HEVC/HDR/10-bit) and the stream is fetched by mpv directly,
// no WebView codec gate, no CORS/Private-Network preflight.

const events = new EventEmitter();

enum ShellEventType {
    SIGNAL = 1,
    INIT = 3,
    INVOKE_METHOD = 6,
}

// The Tauri API the bridge sends through, kept by whichever useShell rendered
// last (they all see the same global). Module state so that the transport
// below can be ONE object for the app's lifetime.
let tauriApi: any = null;

const on = (name: string, listener: (arg: any) => void) => events.on(name, listener);
const off = (name: string, listener: (arg: any) => void) => events.off(name, listener);
const send = (method: string, ...args: (string | number | object)[]) => {
    if (tauriApi) {
        tauriApi.core.invoke('shell_send', { method, args })
            .catch((e: unknown) => console.error('Shell', 'shell_send failed', method, e));
        return;
    }

    try {
        IPC?.postMessage(JSON.stringify({
            id: 0,
            type: ShellEventType.INVOKE_METHOD,
            args: [method, ...args],
        }));
    } catch (e) {
        console.error('Shell', 'Failed to send event', e);
    }
};

// Stable identity: the shell player owns ONE subscription per transport
// (packages/video ShellChannel), so every render must hand it the same object.
const transport: ShellTransport = { on, off, send };

type ShellEvent = {
    id: number;
    type: ShellEventType;
};

type ShellEventInit = ShellEvent & {
    data: {
        transport: {
            properties: string[][],
        }
    };
};

type ShellEventSignal = ShellEvent & {
    args: string[];
};

type ShellMessage = {
    data: string;
};

const useShell = (): Shell => {
    // This bridge needs a WORKING Tauri API (core.invoke + event.listen), not
    // mere shell presence, so it keys on the reactive API accessor instead of
    // isShell(); the global can attach a tick after first render.
    const TAURI = useTauriApi();
    const USE_TAURI = !!TAURI;
    tauriApi = TAURI || null;
    const [state, setState] = useState<ShellState>({
        initialized: false,
        version: null,
        windowClosed: false,
        windowHidden: false,
    });
    const [capabilities, setCapabilities] = useState<ShellCapabilities>({
        gpuVideoProcessing: false,
    });

    // Snapshot of mpv's live media properties (codec/HDR/bitrate/hwdec/audio…)
    // for the player's Stats panel. Native shell only; null elsewhere.
    const getMpvStats = async (): Promise<Record<string, any> | null> => {
        if (!USE_TAURI) return null;
        try {
            return await TAURI.core.invoke('shell_mpv_stats');
        } catch (e) {
            console.error('Shell', 'shell_mpv_stats failed', e);
            return null;
        }
    };

    useEffect(() => {
        const onWindowVisibilityChanged = (data: WindowVisibility) => {
            setState((state) => ({
                ...state,
                windowClosed: data.visible === false && data.visibility === 0,
            }));
        };

        const onWindowStateChanged = (data: WindowState) => {
            setState((state) => ({
                ...state,
                windowHidden: data.state === 9,
            }));
        };

        on('win-visibility-changed', onWindowVisibilityChanged);
        on('win-state-changed', onWindowStateChanged);

        return () => {
            off('win-visibility-changed', onWindowVisibilityChanged);
            off('win-state-changed', onWindowStateChanged);
        };
    }, []);

    // Tauri desktop shell: inbound signals arrive as the `shell-signal` event,
    // and the one-shot `shell_init` reports version + capabilities. Each signal
    // is `{ event, payload }` where `event` is the method name ShellVideo waits
    // for (`mpv-prop-change` / `mpv-event-ended`) and `payload` its argument.
    useEffect(() => {
        if (!USE_TAURI) return;

        let unlisten: (() => void) | undefined;
        let cancelled = false;

        TAURI.event.listen('shell-signal', (event: { payload: { event: string; payload: any } }) => {
            const { event: name, payload } = event.payload;
            events.emit(name, payload);
        }).then((un: () => void) => {
            if (cancelled) un(); else unlisten = un;
        }).catch((e: unknown) => console.error('Shell', 'listen failed', e));

        TAURI.core.invoke('shell_init').then((res: { version: string; gpuVideoProcessing: boolean; ok: boolean }) => {
            setState((state) => ({ ...state, initialized: res.ok, version: res.version }));
            setCapabilities({ gpuVideoProcessing: !!res.gpuVideoProcessing });
        }).catch((e: unknown) => console.error('Shell', 'shell_init failed', e));

        return () => { cancelled = true; if (unlisten) unlisten(); };
    }, [TAURI]);

    useEffect(() => {
        if (USE_TAURI) return;

        const onMessage = (message: ShellMessage) => {
            try {
                const event = JSON.parse(message.data) as ShellEvent;

                if (event.type === ShellEventType.INIT) {
                    const { data } = event as ShellEventInit;
                    const [, [,,, version], [,,, gpuVideoProcessing] = []] = data.transport.properties;

                    setState((state) => ({
                        ...state,
                        initialized: true,
                        version,
                    }));
                    setCapabilities({
                        gpuVideoProcessing: gpuVideoProcessing === 'true',
                    });
                }

                if (event.type === ShellEventType.SIGNAL) {
                    const { args } = event as ShellEventSignal;
                    const [methodName, methodArg] = args;
                    events.emit(methodName, methodArg);
                }
            } catch (e) {
                console.error('Shell', 'Failed to handle event', e);
            }
        };

        IPC?.addEventListener('message', onMessage);
        IPC?.postMessage(JSON.stringify({
            id: 0,
            type: ShellEventType.INIT,
        }));

        return () => IPC?.removeEventListener('message', onMessage);
    }, [TAURI]);

    return {
        active: USE_TAURI || !!IPC,
        send,
        on,
        off,
        transport,
        getMpvStats,
        state,
        capabilities,
    };
};

export default useShell;
