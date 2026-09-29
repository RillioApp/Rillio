// Copyright (C) 2017-2024 Smart code 203358507

/**
 * useToast adapter (foundation kit) - re-implements the app's toast context over
 * Sonner while preserving the EXACT existing API surface, so every call site stays
 * unchanged (see common/Toast/ToastProvider.js for the contract this mirrors):
 *
 *   const toast = useToast();
 *   toast.show({ type, title, message, icon?, timeout?, action?, dataset? }) -> id | null
 *   toast.remove(id); toast.clear();
 *   toast.addFilter(fn); toast.removeFilter(fn);   // fn(item) -> true suppresses
 *
 * Fidelity notes:
 *  - type maps to Sonner: success/info/error pass through; 'alert' -> 'warning'.
 *  - filters run BEFORE toast() exactly like the old provider (any true = suppressed).
 *  - item.icon (stremio icon NAME) renders via the custom-icon slot; else a per-type
 *    default (checkmark / close / about / warning).
 *  - action { label, onSelect } -> Sonner { label, onClick }.
 *  - onClose fires on dismiss/auto-close with the legacy {type,dataset,...} payload.
 */

import React, { createElement, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { toast as sonnerToast } from 'sonner';
import { Check, X, Info, TriangleAlert, Download, type LucideIcon } from 'lucide-react';
import { Toaster } from './sonner';

const DEFAULT_TIMEOUT = 3000;

export type ToastType = 'success' | 'alert' | 'info' | 'error';

export type ToastItem = {
    type?: ToastType;
    title?: string;
    message?: string;
    icon?: string;
    timeout?: number;
    action?: { label: string; onSelect: () => void };
    dataset?: Record<string, unknown>;
    onSelect?: (event: { type: string; dataset?: Record<string, unknown> }) => void;
    onClose?: (event: { type: string; dataset?: Record<string, unknown> }) => void;
};

export type ToastFilter = (item: ToastItem) => boolean;

const DEFAULT_ICON: Record<ToastType, string> = {
    success: 'checkmark',
    error: 'close',
    info: 'about',
    alert: 'warning',
};

const TOAST_ICON: Record<string, LucideIcon> = {
    checkmark: Check,
    close: X,
    about: Info,
    warning: TriangleAlert,
    download: Download,
};

// Module-level filters, mirroring the closure array in the legacy provider.
const filters: ToastFilter[] = [];

function iconFor(item: ToastItem): ReactNode {
    const name = typeof item.icon === 'string' && item.icon.length > 0
        ? item.icon
        : DEFAULT_ICON[item.type ?? 'success'];
    const Cmp = TOAST_ICON[name] ?? Info;
    return createElement(Cmp, { className: 'size-5' });
}

function invoke(type: string, item: ToastItem, cb?: ToastItem['onSelect'] | ToastItem['onClose']) {
    if (typeof cb === 'function') {
        cb({ type, dataset: item.dataset });
    }
}

const toastApi = {
    addFilter(filter: ToastFilter) {
        filters.push(filter);
    },
    removeFilter(filter: ToastFilter) {
        const index = filters.indexOf(filter);
        if (index > -1) filters.splice(index, 1);
    },
    show(item: ToastItem): string | number | null {
        if (filters.some((filter) => filter(item))) {
            return null;
        }
        const duration = typeof item.timeout === 'number' && !isNaN(item.timeout) ? item.timeout : DEFAULT_TIMEOUT;
        const kind = item.type === 'alert' ? 'warning' : (item.type ?? 'success');
        const emitter: (message: ReactNode, data?: Record<string, unknown>) => string | number =
            kind === 'warning' ? sonnerToast.warning
                : kind === 'error' ? sonnerToast.error
                    : kind === 'info' ? sonnerToast.info
                        : sonnerToast.success;

        // The legacy contract's item.onSelect makes the WHOLE toast a click target
        // (the shell's "update available - click to install" rides on it). Sonner
        // has no toast-level onClick, so a full-bleed overlay goes in with the
        // title: the toast <li> is positioned, so inset-0 covers all of it. This
        // was declared in the type and then never wired - the update toast shipped
        // as a dead click the first release toasts were visible at all.
        // The overlay's handler needs the toast id to dismiss it, but the id only
        // exists once the emitter returns - a holder bridges the cycle.
        const handle: { id?: string | number } = {};
        const activate = () => {
            invoke('select', item, item.onSelect);
            if (handle.id !== undefined) sonnerToast.dismiss(handle.id);
        };
        const title: ReactNode = typeof item.onSelect === 'function'
            ? createElement(
                React.Fragment,
                null,
                item.title ?? '',
                // A real <button>, not an aria-hidden div: this is the ONLY control
                // on a click-anywhere toast (the update toast has no action button),
                // so it must be focusable and Enter/Space-activatable or a keyboard/
                // screen-reader user cannot take the update at all. aria-label names
                // it from the title since the visible label sits behind it.
                createElement('button', {
                    type: 'button',
                    className: 'absolute inset-0 cursor-pointer appearance-none border-0 bg-transparent p-0',
                    'aria-label': typeof item.title === 'string' && item.title.length > 0 ? item.title : 'Activate',
                    onClick: activate,
                }),
            )
            : (item.title ?? '');

        // onClose fires exactly once, however the toast goes (its action, auto-
        // close, a dismiss or clear, a swipe), and once it has fired the action
        // is dead. Sonner keeps a closing toast in the DOM, still clickable,
        // for its exit animation, so without this an action could run AFTER
        // the close its caller acted on (the next-episode preload starts its
        // download on close, and its action is Cancel).
        let closed = false;
        const close = () => {
            if (closed) return;
            closed = true;
            invoke('close', item, item.onClose);
        };
        handle.id = emitter(title, {
            description: item.message,
            duration,
            icon: iconFor(item),
            // An explicit action button must stay clickable above the overlay.
            action: item.action
                ? {
                    label: item.action.label,
                    onClick: () => {
                        if (closed) return;
                        item.action!.onSelect();
                        close();
                    },
                }
                : undefined,
            onDismiss: close,
            onAutoClose: close,
        });
        return handle.id;
    },
    remove(id: string | number) {
        sonnerToast.dismiss(id);
    },
    clear() {
        sonnerToast.dismiss();
    },
};

export type ToastApi = typeof toastApi;

// Debug handle: the production bundle is a single chunk with no webpack global,
// so a CDP session has no way to reach module exports. This is the supported
// hatch for firing/inspecting toasts in the running shell (page context is
// already fully trusted; this adds no new capability to it).
if (typeof window !== 'undefined') {
    (window as unknown as { __rillioToast?: ToastApi }).__rillioToast = toastApi;
}

/** Drop-in replacement for the legacy useToast() hook (returns a stable facade). */
export function useToast(): ToastApi {
    return toastApi;
}

/**
 * ToastProvider - parity wrapper for the legacy provider. Sonner is imperative and
 * global, so there is no context to thread; this simply renders children plus the
 * single <Toaster/>. Kept so existing `<ToastProvider>` mount points need no change.
 *
 * The Toaster is PORTALLED TO document.body, and that is load-bearing. This provider
 * mounts inside `#app`, which is `position: relative; z-index: 0` - a stacking
 * context. A z-index only competes with siblings INSIDE its own stacking context, so
 * sonner's z-index:999999999 does not escape `#app`: the whole toast layer flattens
 * to `#app`'s z-index 0. Radix portals every dialog to <body> at z-index 50, which
 * therefore paints above ALL of `#app` - scrim included. Left inline, any toast
 * raised while a modal is open (a Cached pause/delete error, a download starting
 * from Search) renders underneath the scrim and is never seen. Portalling makes the
 * toaster a sibling of the dialog portals, where its z-index finally means what it
 * says. Verified over CDP: elementFromPoint at the toast's centre returned the
 * dialog's `.fixed.inset-0` overlay before this, and the toast after.
 */
export function ToastProvider({ children }: { children?: ReactNode }) {
    return (
        <>
            {children}
            {createPortal(<Toaster />, document.body)}
        </>
    );
}

export default useToast;
