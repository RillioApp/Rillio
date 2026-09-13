/**
 * One control for every "number you tune by ear" in the player (subtitle
 * delay, audio delay, playback speed): a slider for the fine end, -/+ for the
 * coarse steps. Dragging snaps to 0.05 (below that a delay nudge is invisible
 * on 24fps material), the buttons move 0.25 and repeat while held, and the
 * value itself is a small input for when you already know the number.
 * Unit-agnostic (seconds, x); the callers own any ms conversion. The label
 * row carries an optional action slot; the subtitles menu puts Sync there.
 *
 * Bounds: `min`/`max` fix the slider's range (speed: 0.1x..4x). Without them
 * (delays, which have no natural limit) the range is symmetric and grows in
 * 5-unit steps to keep the value on the bar, frozen while a drag is in
 * flight so the thumb never jumps under the cursor.
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Minus, Plus, RotateCcw } from 'lucide-react';
import { useInterval, useTimeout } from 'rillio/common';
import { Slider } from 'rillio/components';
import { IconButton, cn } from 'rillio/components/ui';

export const COARSE_STEP = 0.25;
export const FINE_STEP = 0.05;
const DEFAULT_HALF_RANGE = 5;

type Props = {
    className?: string;
    label: string;
    value: number | null;
    unit: string;
    min?: number;
    max?: number;
    disabled?: boolean;
    onChange: (value: number) => void;
    // The value the reset button returns to (0 for a delay, 1 for speed).
    resetValue: number;
    action?: React.ReactNode;
};

const RESET_BUTTON = 'size-6 shrink-0 bg-transparent opacity-100 hover:bg-fg/10 [&_svg]:size-3.5 [&_svg]:text-fg-muted hover:[&_svg]:text-fg';

const STEP_BUTTON = 'size-8 shrink-0 bg-(--overlay-color) opacity-100 hover:bg-(--overlay-color) hover:brightness-110 [&_svg]:size-4 [&_svg]:text-fg';
// Accent slider, like the seek bar: faint ice track, accent fill + thumb.
const TRACK = 'bg-ice/15 opacity-100';
const FILLED = 'bg-(--color-accent)';
const THUMB = 'bg-(--color-accent) transition-transform duration-150 group-hover:scale-[1.2]';

const snap = (raw: number) => Math.round(raw / FINE_STEP) * FINE_STEP;
const format = (raw: number) => raw.toFixed(2);

const FineStepper = ({ className, label, value, unit, min, max, disabled, onChange, resetValue, action }: Props) => {
    const { t } = useTranslation();
    const inactive = disabled || typeof value !== 'number';

    const settle = useCallback((raw: number) => {
        const snapped = snap(raw);
        const low = typeof min === 'number' ? Math.max(snapped, min) : snapped;
        return typeof max === 'number' ? Math.min(low, max) : low;
    }, [min, max]);

    // Latest value through a ref: held buttons and the drag both read it
    // without closing over a stale render.
    const latest = useRef(value);
    useEffect(() => {
        latest.current = value;
    }, [value]);

    // The slider's range. Fixed when bounded; otherwise symmetric around zero
    // and widened (never narrowed mid-drag) to keep the value on the bar.
    const dragging = useRef(false);
    const halfRange = useRef(DEFAULT_HALF_RANGE);
    // Bumped on release: the range is recomputed on render, and a drag that
    // ends on the value it already emitted would otherwise never re-render
    // (leaving a range grown for an old value in place).
    const [, settled] = useState(0);
    if (!dragging.current && typeof value === 'number') {
        halfRange.current = Math.max(DEFAULT_HALF_RANGE, Math.ceil(Math.abs(value) / DEFAULT_HALF_RANGE) * DEFAULT_HALF_RANGE);
    }
    const sliderMin = typeof min === 'number' ? min : -halfRange.current;
    const sliderMax = typeof max === 'number' ? max : halfRange.current;

    // Emit only when the snapped value actually moves: a drag delivers a value
    // per frame and the players resync on every write.
    const emitted = useRef<number | null>(null);
    const emit = useCallback((raw: number) => {
        const next = settle(raw);
        if (emitted.current !== next) {
            emitted.current = next;
            onChange(next);
        }
    }, [onChange, settle]);
    const onSlide = useCallback((raw: number) => {
        dragging.current = true;
        emit(raw);
    }, [emit]);
    const onComplete = useCallback((raw: number) => {
        emit(raw);
        dragging.current = false;
        emitted.current = null;
        settled((n) => n + 1);
    }, [emit]);

    // Press-and-hold repeat on the coarse buttons (250ms, then every 100ms).
    const interval = useInterval(100);
    const timeout = useTimeout(250);
    const cancel = useCallback(() => {
        interval.cancel();
        timeout.cancel();
    }, [interval, timeout]);
    const nudge = useCallback((delta: number) => {
        if (typeof latest.current !== 'number') return;
        onChange(settle(latest.current + delta));
    }, [onChange, settle]);
    const holdStart = useCallback((delta: number) => {
        cancel();
        timeout.start(() => interval.start(() => nudge(delta)));
    }, [cancel, interval, nudge, timeout]);
    const holdEnd = useCallback((delta: number) => {
        cancel();
        nudge(delta);
    }, [cancel, nudge]);

    // The input keeps its own text while the user types ("-0." must survive
    // a render), and re-syncs from the real value on blur or external change.
    const [text, setText] = useState(() => typeof value === 'number' ? format(value) : '');
    useEffect(() => {
        setText(typeof value === 'number' ? format(value) : '');
    }, [value]);
    const commit = useCallback((raw: string) => {
        const parsed = parseFloat(raw);
        if (Number.isFinite(parsed)) {
            onChange(settle(parsed));
        } else if (typeof value === 'number') {
            setText(format(value));
        }
    }, [onChange, settle, value]);

    const atMin = typeof value === 'number' && typeof min === 'number' && value <= min;
    const atMax = typeof value === 'number' && typeof max === 'number' && value >= max;

    return (
        <div className={cn('flex flex-col', className)}>
            <div className={'mb-2 flex h-6 items-center gap-1.5'}>
                <div className={cn('min-w-0 flex-1 truncate text-fg', inactive ? 'opacity-100' : 'opacity-60')}>{t(label)}</div>
                {inactive ?
                    <div className={'font-medium tabular-nums text-fg'}>--</div>
                    :
                    <input
                        type="number"
                        step={FINE_STEP}
                        min={min}
                        max={max}
                        inputMode="decimal"
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
                        className={'w-12 bg-transparent text-right font-medium tabular-nums text-fg outline-none [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none'}
                    />
                }
                <span className={'text-xs text-fg-muted'}>{unit}</span>
                <IconButton
                    disabled={inactive || value === resetValue}
                    className={RESET_BUTTON}
                    title={`${t('RESET')} (${format(resetValue)}${unit})`}
                    onClick={() => onChange(resetValue)}
                >
                    <RotateCcw />
                </IconButton>
                {action}
            </div>
            <div className={cn('flex items-center gap-2', inactive && 'opacity-40')}>
                <IconButton
                    disabled={inactive || atMin}
                    className={STEP_BUTTON}
                    title={`-${COARSE_STEP}${unit}`}
                    onMouseDown={() => holdStart(-COARSE_STEP)}
                    onMouseUp={() => holdEnd(-COARSE_STEP)}
                    onMouseLeave={cancel}
                >
                    <Minus />
                </IconButton>
                <Slider
                    className={'h-8 min-w-0 flex-1 [--thumb-size:0.9rem] [--track-size:0.3rem]'}
                    trackClassName={TRACK}
                    filledClassName={FILLED}
                    thumbClassName={THUMB}
                    value={typeof value === 'number' ? value : 0}
                    minimumValue={sliderMin}
                    maximumValue={sliderMax}
                    disabled={inactive}
                    onSlide={onSlide}
                    onComplete={onComplete}
                />
                <IconButton
                    disabled={inactive || atMax}
                    className={STEP_BUTTON}
                    title={`+${COARSE_STEP}${unit}`}
                    onMouseDown={() => holdStart(COARSE_STEP)}
                    onMouseUp={() => holdEnd(COARSE_STEP)}
                    onMouseLeave={cancel}
                >
                    <Plus />
                </IconButton>
            </div>
        </div>
    );
};

export default FineStepper;
