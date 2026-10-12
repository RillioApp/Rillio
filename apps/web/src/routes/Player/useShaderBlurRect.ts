// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Reports the open player panels' rects to the shell, which blurs the LIVE video
 * under them with a GPU shader inside mpv (the shell's `player_blur_rect` command
 * and src-tauri/src/shaders/panel-blur.glsl).
 *
 * WHY A SHADER: mpv renders into a NATIVE child window behind the transparent
 * WebView, so a panel's CSS `backdrop-filter` has nothing to sample and blurs
 * nothing at all - only the panel's own alpha darkening composites. The CPU
 * alternative (useVideoSnapshotBackdrop, built and then gated off) has mpv pull
 * each frame back off the GPU, through disk, through a decode: tens to hundreds of
 * ms late, which reads as lag rather than as a material. Blurring inside mpv's own
 * pipeline is live and costs nothing.
 *
 * OFF BY DEFAULT, AND THAT IS LOAD-BEARING. The shader hooks the exact render
 * pipeline that carries this app's native HDR and Dolby Vision passthrough - the
 * hardest-won thing in the project, and better than VLC's. So it does not turn on
 * until someone has watched real HDR content with it on a real HDR display. With
 * the flag off this hook returns null: no rect is measured, no rAF loop runs, the
 * shell is never called, and because the shell only loads the shader on the first
 * call that wants a blur, mpv's pipeline is bit-for-bit what it is today. Flipping
 * the constant below is the only step.
 *
 * FAILS QUIET. Outside the shell, or after a few consecutive errors (no video
 * geometry yet, an mpv that rejects the shader), this stops reporting and every
 * panel keeps the dark glass it has today. One console.warn marks it.
 */

import { useCallback, useEffect, useMemo, useRef } from 'react';
import { useTauriApi } from 'rillio/common/Platform/shell/isShell';

// The kill switch for the paragraph above. Flip to true to blur the video for
// real under the player's panels.
// ON since 2026-07-16 (Michael asked for the sidebar blur); the HDR safety
// analysis said structural-safe, and the live HDR verification happens now
// that it is on - if HDR/DV content ever looks off with a panel open, flip
// this back first.
export const SHADER_BLUR_ENABLED = true;

// Must match MAX_BLUR_RECTS in the shell and MAX_RECTS in the shader. Four covers
// every combination the Player can actually produce (the side drawer plus a menu);
// the shell rejects a longer list rather than silently dropping one.
const MAX_RECTS = 4;

// Consecutive failures after which we give up for this session. One transient
// error (a panel opened in the gap before mpv reports its geometry) must not
// disable the blur for good.
const MAX_ERRORS = 3;

// On <html> while a panel is actually being blurred: lightens the panels' fill,
// which at its unblurred alpha is doing all the legibility work on its own. See
// the `html.player-shader-blur` rule in styles/tailwind.css.
const BLUR_CLASS = 'player-shader-blur';

/** What a panel gets to register itself for the blur. */
export type ShaderBlurRegistry = {
    /** Register `element` as an open panel. Returns its unregister function. */
    register: (element: HTMLElement) => () => void;
};

type Rect = { x: number; y: number; width: number; height: number; corner: number };

const useShaderBlurRect = (): ShaderBlurRegistry | null => {
    const TAURI = useTauriApi();
    // Resolved once, here: with the flag off `invoke` is undefined, so `register`
    // is inert, the loop never starts and nothing below ever reaches the shell.
    const invoke = SHADER_BLUR_ENABLED ? TAURI?.core?.invoke : undefined;

    // The registered MARKER elements (see ShaderBlurRect); `measure` resolves
    // each to the panel it is pinned to.
    const panels = useRef<Set<HTMLElement>>(new Set()).current;
    // Each panel's corner radius, keyed by the PANEL element and read the first
    // time it is measured. Weak, so a closed panel's entry goes with its element.
    const corners = useRef<WeakMap<HTMLElement, number>>(new WeakMap()).current;
    const frame = useRef<number | null>(null);
    const sent = useRef<string | null>(null);
    const errors = useRef(0);

    const measure = useCallback((): Rect[] => {
        const rects: Rect[] = [];
        panels.forEach((element) => {
            // NO BLUR WHILE A PANEL ANIMATES (Michael's call): mpv repositions
            // the blur only when it renders a video frame - 24fps steps against
            // a 240Hz panel animation - so tracking the slide always trailed
            // visibly, and predicting the resting rect (transform-stripping,
            // then WAAPI jump-to-end sampling) never survived contact with
            // Radix's animations. Instead the frost simply waits: it snaps in
            // on the settled panel one tick after the animation ends, and drops
            // the instant a close animation starts.
            // The registered element is ShaderBlurRect's MARKER div inside the
            // panel; the slide/zoom animation runs on an ANCESTOR (the Radix
            // content element), so walk up - the marker's own subtree never
            // sees it (that miss shipped once: rects streamed every frame,
            // animated, straight through this gate). KEYFRAME ANIMATIONS ONLY:
            // ancestors constantly run CSS TRANSITIONS (the immersion fades,
            // and the ones triggered by BLUR_CLASS itself - a feedback loop
            // that flapped the blur on/off every frame and lagged the player),
            // while the panel enter/exit moves are CSSAnimations.
            let animating = false;
            for (let node: HTMLElement | null = element; node !== null && !animating; node = node.parentElement) {
                animating = node.getAnimations().some((animation) =>
                    animation instanceof CSSAnimation && animation.playState === 'running');
            }
            if (animating) return;
            // MEASURE THE PANEL, NOT THE MARKER. The marker is `absolute
            // inset-0`, which pins it to the panel's PADDING box: one border
            // width inside the edge the viewer sees (the panel paints its fill
            // and border out to its border box), and, because every panel is
            // also its own scroll container, it rides the panel's content when
            // that scrolls. The marker's offsetParent is by definition the box
            // it is pinned to - the panel - and that element's own rect is the
            // visible panel, borders included, whatever its content does.
            const panel = element.offsetParent;
            // Not laid out (display: none): there is no panel on screen.
            if (!(panel instanceof HTMLElement)) return;
            const rect = panel.getBoundingClientRect();
            // A panel mid-fade can also measure to nothing; there is no video
            // to blur under a zero-sized rect.
            if (rect.width <= 0 || rect.height <= 0) return;
            if (rects.length >= MAX_RECTS) return;
            let corner = corners.get(panel);
            if (corner === undefined) {
                // The panel's OWN outer radius, read off the element rather
                // than hardcoded: the menus are rounded-card while the side
                // drawer is flush and square, and this stays true if either
                // ever changes. Read once per panel: radii do not animate, and
                // reading one per frame would force a style flush per panel.
                corner = parseFloat(getComputedStyle(panel).borderTopLeftRadius) || 0;
                corners.set(panel, corner);
            }
            rects.push({
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                corner,
            });
        });
        return rects;
    }, [panels, corners]);

    const push = useCallback(() => {
        if (!invoke || errors.current >= MAX_ERRORS) return;
        const rects = measure();
        // Physical px per CSS px. The rects are CSS px and the shader works in
        // the window's physical px; this is the exact ratio between the two
        // (measured: a box painted at a CSS rect lands on rect x
        // devicePixelRatio at every window size). It was the viewport size
        // before, for the shell to divide its window by, but innerWidth is a
        // whole number of CSS px and so is not the window: at a window size
        // that is not a whole number of CSS px the quotient was off by up to a
        // few px at the far edge. Read per push, so a move to a display with
        // another scaling is picked up, and part of the de-dupe key below for
        // the same reason.
        const scale = window.devicePixelRatio;
        // The panels slide and fade in, so the loop below re-measures every frame;
        // only an actual change is worth an IPC hop. (The shell throttles and
        // de-dupes on its side too - this just keeps the bridge quiet.)
        const key = JSON.stringify({ rects, scale });
        if (key === sent.current) return;
        sent.current = key;

        document.documentElement.classList.toggle(BLUR_CLASS, rects.length > 0);
        invoke('player_blur_rect', { rects, scale }).catch((e: unknown) => {
            errors.current += 1;
            if (errors.current === 1) {
                console.warn('Player', 'GPU panel blur unavailable, falling back to plain glass', e);
            }
            if (errors.current >= MAX_ERRORS) {
                document.documentElement.classList.remove(BLUR_CLASS);
            }
        });
    }, [invoke, measure]);

    // Measure every frame while any panel is open, and only while one is. The
    // panels slide (the drawer) and zoom (the menus) in via CSS animations, and a
    // transform moves an element's viewport rect WITHOUT resizing it - so a
    // ResizeObserver would report the panel's final size while it is still
    // offscreen, and pin the blur to the wrong place for the whole animation.
    // Measuring per frame is what makes the blur track the panel instead. It costs
    // a handful of getBoundingClientRect calls, only while something is open, and
    // `push` drops anything that has not actually moved.
    const tick = useCallback(() => {
        push();
        frame.current = panels.size > 0 ? requestAnimationFrame(tick) : null;
    }, [push, panels]);

    const start = useCallback(() => {
        if (!invoke || frame.current !== null || panels.size === 0) return;
        frame.current = requestAnimationFrame(tick);
    }, [invoke, panels, tick]);

    const register = useCallback((element: HTMLElement) => {
        panels.add(element);
        start();
        return () => {
            panels.delete(element);
            // Do NOT stop the loop here: the last panel closing is exactly when the
            // shell still has to be told to stop blurring. The loop sees an empty
            // set, pushes `rects: []`, and only then lets itself end.
            start();
        };
    }, [panels, corners, start]);

    useEffect(() => {
        if (!invoke) return undefined;
        // A window resize moves the anchored panels without any React re-render.
        // (The per-frame loop covers it while one is open; this catches the edge
        // where a resize lands in the same frame the loop is winding down.)
        window.addEventListener('resize', push);
        return () => {
            window.removeEventListener('resize', push);
            if (frame.current !== null) cancelAnimationFrame(frame.current);
            frame.current = null;
            // Leaving the Player with a panel open must not leave the video blurred
            // under nothing.
            document.documentElement.classList.remove(BLUR_CLASS);
            invoke('player_blur_rect', { rects: [], scale: window.devicePixelRatio })
                .catch(() => { /* already tearing down; the shell logs its own side */ });
        };
    }, [invoke, push]);

    // Null with the flag off (or outside the shell) so a panel's <ShaderBlurRect />
    // renders nothing at all.
    return useMemo(() => (invoke ? { register } : null), [invoke, register]);
};

export default useShaderBlurRect;
