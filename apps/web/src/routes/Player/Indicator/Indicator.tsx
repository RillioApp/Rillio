// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Transient on-screen HUD that flashes a property change (subtitle delay / video
 * scale) for ~1s then fades. Diff-driven off videoState; the ignore-first-value and
 * 1s auto-hide are preserved. Restyled onto Tailwind tokens; the fade is the motion
 * Presence primitive (300ms, matching the old Transition fade duration here).
 */

import React, { useEffect, useMemo, useRef, useState } from 'react';
import { t } from 'i18next';
import { Presence } from 'rillio/components';
import { cn } from 'rillio/components/ui/cn';
import { useBinaryState } from 'rillio/common';

type Property = {
    label: string,
    format: (value: number | string) => string,
};

const VIDEO_SCALE_KEYS: Record<string, string> = {
    'contain': 'PLAYER_SCALE_FIT',
    'cover': 'PLAYER_SCALE_CROP',
    'fill': 'PLAYER_SCALE_STRETCH',
};

const PROPERTIES: Record<string, Property> = {
    'extraSubtitlesDelay': {
        label: 'SUBTITLES_DELAY',
        format: (value) => `${((value as number) / 1000).toFixed(2)}s`,
    },
    'audioDelay': {
        label: 'AUDIO_DELAY',
        format: (value) => `${((value as number) / 1000).toFixed(2)}s`,
    },
    'videoScale': {
        label: 'VIDEO_SCALE',
        format: (value) => t(VIDEO_SCALE_KEYS[String(value)] || String(value)),
    },
};

type VideoState = Record<string, number | string>;

type Props = {
    className: string,
    videoState: VideoState,
    disabled: boolean,
};

const Indicator = ({ className, videoState, disabled }: Props) => {
    const timeout = useRef<ReturnType<typeof setTimeout> | null>(null);
    const prevVideoState = useRef<VideoState>(videoState);
    const initialized = useRef<Set<string>>(new Set());

    const [shown, show, hide] = useBinaryState(false);
    const [current, setCurrent] = useState<string | null>(null);

    const label = useMemo(() => {
        const property = current && PROPERTIES[current];
        return property && t(property.label);
    }, [current]);

    const value = useMemo(() => {
        const property = current && PROPERTIES[current];
        const value = current && videoState[current];
        return property && value && property.format(value);
    }, [current, videoState]);

    useEffect(() => {
        for (const property of Object.keys(PROPERTIES)) {
            const prev = prevVideoState.current[property];
            const next = videoState[property];

            if (next && next !== prev) {
                if (!initialized.current.has(property)) {
                    initialized.current.add(property);
                } else {
                    setCurrent(property);
                    show();

                    timeout.current && clearTimeout(timeout.current);
                    timeout.current = setTimeout(hide, 1000);
                }
            }
        }

        prevVideoState.current = videoState;
    }, [videoState]);

    return (
        <Presence when={shown && !disabled} duration={300}>
            <div className={cn('absolute flex h-16 select-none items-center justify-center', className)}>
                <div className={'flex h-full flex-none items-center justify-center rounded-full border border-line bg-glass-panel px-8 text-center font-bold text-fg shadow-elevated backdrop-blur-(--glass-blur)'}>
                    {label} {value}
                </div>
            </div>
        </Presence>
    );
};

export default Indicator;
