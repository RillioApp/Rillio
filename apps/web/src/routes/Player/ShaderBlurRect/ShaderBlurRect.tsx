// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Registers its panel's bounds for the GPU video blur, and draws nothing.
 *
 * Sits inside each player panel next to <SnapshotBackdrop />, and for the same
 * reason: the panel element is the thing to measure, and a marker pinned to its
 * bounds is a single self-contained child, so no rect plumbing has to thread
 * through six component signatures. The registry arrives by context, which crosses
 * portals - the side drawer portals to <body> and still finds it.
 *
 * WHAT GETS MEASURED IS THE PANEL, NOT THIS ELEMENT. `absolute` pins the marker
 * to the panel (its nearest positioned ancestor), which makes the panel this
 * element's `offsetParent`; useShaderBlurRect measures THAT element's border box
 * and reads its radius. The marker's own rect is not the panel's: `inset-0` is
 * the padding box, one border width inside the visible edge, and it scrolls with
 * the panel's content. So a panel that hosts this must be positioned (every one
 * is: MENU_LAYER is `absolute`, the side drawer's sheet is `fixed`).
 *
 * Renders null when the shell is not blurring (the flag is off, or we are not in
 * the shell at all), which is every case today. See useShaderBlurRect.
 */

import React, { createContext, memo, useContext, useEffect, useRef } from 'react';

import type { ShaderBlurRegistry } from '../useShaderBlurRect';

// Provided by the Player route. Null (the default) means "nobody is blurring",
// which is what makes this component free everywhere else.
export const ShaderBlurContext = createContext<ShaderBlurRegistry | null>(null);

const ShaderBlurRect = memo(function ShaderBlurRect() {
    const registry = useContext(ShaderBlurContext);
    const ref = useRef<HTMLDivElement>(null);

    useEffect(() => {
        const element = ref.current;
        if (!registry || !element) {
            return undefined;
        }
        // register() returns its own unregister, which tells the shell to stop
        // blurring this panel as it unmounts.
        return registry.register(element);
    }, [registry]);

    if (!registry) {
        return null;
    }

    // Invisible and inert: the blur it asks for happens inside mpv, behind the
    // whole WebView, so there is nothing for this element to paint.
    return <div ref={ref} aria-hidden={true} className={'pointer-events-none absolute inset-0 rounded-[inherit]'} />;
});

export default ShaderBlurRect;
