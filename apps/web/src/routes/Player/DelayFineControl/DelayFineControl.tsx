/**
 * Fine adjustment for a delay (subtitles or audio), sitting under the coarse
 * 0.25s Stepper: a 0.05s nudge pair around a typed value. The coarse stepper
 * gets you near; this lands the exact frame. Seconds in, seconds out (the
 * callers own the ms conversion, same as the Stepper).
 */

import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Minus, Plus } from 'lucide-react';
import { IconButton, cn } from 'rillio/components/ui';

// 0.05s: below this a nudge is invisible on 24fps material (a frame is 42ms)
// and the value stops being readable as a number.
export const FINE_STEP = 0.05;

// Keep typed/nudged values on the 0.05 grid and free of float noise
// (0.1 + 0.05 !== 0.15 in JS).
const snap = (seconds: number) => Math.round(seconds / FINE_STEP) * FINE_STEP;
const format = (seconds: number) => snap(seconds).toFixed(2);

type Props = {
    className?: string;
    value: number | null;
    disabled?: boolean;
    onChange: (seconds: number) => void;
};

const NUDGE_BUTTON = 'size-10 shrink-0 bg-(--overlay-color) opacity-100 hover:bg-(--overlay-color) hover:brightness-110 [&_svg]:size-4 [&_svg]:text-fg';

const DelayFineControl = ({ className, value, disabled, onChange }: Props) => {
    const { t } = useTranslation();
    const inactive = disabled || typeof value !== 'number';

    // The input keeps its own text while the user types ("-0." must survive
    // a render), and re-syncs from the real value on blur or external change.
    const [text, setText] = useState(() => typeof value === 'number' ? format(value) : '');
    useEffect(() => {
        setText(typeof value === 'number' ? format(value) : '');
    }, [value]);

    const commit = useCallback((raw: string) => {
        const parsed = parseFloat(raw);
        if (Number.isFinite(parsed)) {
            onChange(snap(parsed));
        } else if (typeof value === 'number') {
            setText(format(value));
        }
    }, [onChange, value]);

    const nudge = useCallback((direction: -1 | 1) => {
        if (typeof value !== 'number') return;
        onChange(snap(value + direction * FINE_STEP));
    }, [onChange, value]);

    return (
        <div className={cn('flex flex-col', className)}>
            <div className={cn('mb-2 text-sm text-fg', inactive ? 'opacity-100' : 'opacity-60')}>
                {t('DELAY_FINE')}
            </div>
            <div className={cn('flex items-center gap-2', inactive && 'opacity-40')}>
                <IconButton disabled={inactive} className={NUDGE_BUTTON} onClick={() => nudge(-1)} title={`-${FINE_STEP}s`}>
                    <Minus />
                </IconButton>
                <div className={'flex h-10 min-w-0 flex-1 items-center rounded-full bg-(--overlay-color) px-4'}>
                    <input
                        type="number"
                        step={FINE_STEP}
                        inputMode="decimal"
                        disabled={inactive}
                        value={text}
                        onChange={(event) => setText(event.target.value)}
                        onBlur={(event) => commit(event.target.value)}
                        onKeyDown={(event) => {
                            // Enter commits without leaving the field; the player's
                            // own shortcuts must not see keys typed in here.
                            event.stopPropagation();
                            if (event.key === 'Enter') commit((event.target as HTMLInputElement).value);
                        }}
                        onKeyUp={(event) => event.stopPropagation()}
                        className={'min-w-0 flex-1 bg-transparent text-center font-medium tabular-nums text-fg outline-none [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none'}
                    />
                    <span className={'ml-1 text-sm text-fg-muted'}>s</span>
                </div>
                <IconButton disabled={inactive} className={NUDGE_BUTTON} onClick={() => nudge(1)} title={`+${FINE_STEP}s`}>
                    <Plus />
                </IconButton>
            </div>
        </div>
    );
};

export default DelayFineControl;
