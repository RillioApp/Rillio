type WindowVisibility = {
    visible: boolean;
    visibility: number;
    isFullscreen: boolean;
};

type WindowState = {
    state: number;
};

type MediaStatus = {
    paused: boolean;
};

interface Shell {
    active: boolean,
    capabilities: ShellCapabilities,
    state: ShellState,
    on: (name: string, listener: (arg: any) => void) => void;
    off: (name: string, listener: (arg: any) => void) => void;
    send: (method: string, ...args: (string | number | object)[]) => void;
    // The same on/off/send as ONE object that never changes identity for the
    // app's lifetime: what the shell player takes, because its one IPC
    // subscription is owned per transport (packages/video ShellChannel).
    transport: ShellTransport;
    getMpvStats: () => Promise<Record<string, any> | null>;
}

type ShellTransport = {
    on: (name: string, listener: (arg: any) => void) => void;
    off: (name: string, listener: (arg: any) => void) => void;
    send: (method: string, ...args: (string | number | object)[]) => void;
};

type ShellCapabilities = {
    gpuVideoProcessing: boolean;
};

type ShellState = {
    initialized: boolean;
    version: string | null;
    windowClosed: boolean;
    windowHidden: boolean;
};
