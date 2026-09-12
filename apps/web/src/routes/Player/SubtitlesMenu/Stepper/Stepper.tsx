// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Subtitle settings stepper (delay / size / vertical position). Composed on the kit
 * IconButton (the same base the kit NumberStepper uses) rather than the standalone
 * NumberStepper, because this variant keeps a header label ABOVE a full-width
 * [- value +] row and shows "--" when no track is selected - neither of which the
 * inline NumberStepper models. The press-and-hold repeat (250ms delay, 100ms interval)
 * and min/max disable are preserved verbatim.
 */

import React, { useCallback, useEffect, useMemo, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { Minus, Plus } from 'lucide-react';
import { useInterval, useTimeout } from 'rillio/common';
import { IconButton } from 'rillio/components/ui';
import { cn } from 'rillio/components/ui';

const clamp = (value: number, min?: number, max?: number) => {
    const minClamped = typeof min === 'number' ? Math.max(value, min) : value;
    const maxClamped = typeof max === 'number' ? Math.min(minClamped, max) : minClamped;
    return maxClamped;
};

type Props = {
    className?: string;
    label: string;
    value: number | null;
    unit?: string;
    step: number;
    min?: number;
    max?: number;
    disabled?: boolean;
    onChange: (value: number) => void;
};

const STEP_BUTTON = 'size-14 bg-(--overlay-color) opacity-100 hover:bg-(--overlay-color) hover:brightness-110 [&_svg]:size-6 [&_svg]:text-fg';

const Stepper = ({ className, label, value, unit, step, min, max, disabled, onChange }: Props) => {
    const { t } = useTranslation();

    const localValue = useRef(value);

    const interval = useInterval(100);
    const timeout = useTimeout(250);

    const cancel = () => {
        interval.cancel();
        timeout.cancel();
    };

    const decreaseDisabled = useMemo(() => {
        return disabled || typeof value !== 'number' || (typeof min === 'number' && value <= min);
    }, [disabled, min, value]);

    const increaseDisabled = useMemo(() => {
        return disabled || typeof value !== 'number' || (typeof max === 'number' && value >= max);
    }, [disabled, max, value]);

    const valueLabel = useMemo(() => {
        return (disabled || typeof value !== 'number') ? '--' : `${value}${unit}`;
    }, [disabled, value, unit]);

    const updateValue = useCallback((delta: number) => {
        onChange(clamp((localValue.current as number) + delta, min, max));
    }, [onChange]);

    const onDecrementMouseDown = useCallback(() => {
        cancel();
        timeout.start(() => interval.start(() => updateValue(-step)));
    }, [updateValue]);

    const onDecrementMouseUp = useCallback(() => {
        cancel();
        updateValue(-step);
    }, [updateValue]);

    const onIncrementMouseDown = useCallback(() => {
        cancel();
        timeout.start(() => interval.start(() => updateValue(step)));
    }, [updateValue]);

    const onIncrementMouseUp = useCallback(() => {
        cancel();
        updateValue(step);
    }, [updateValue]);

    useEffect(() => {
        localValue.current = value;
    }, [value]);

    return (
        <div className={cn('flex flex-col', className)}>
            <div className={cn('mb-2 text-fg', disabled ? 'opacity-100' : 'opacity-60')}>
                {t(label)}
            </div>
            <div className={cn('flex items-center rounded-full bg-(--overlay-color)', disabled && 'opacity-40')}>
                <IconButton
                    disabled={decreaseDisabled}
                    className={STEP_BUTTON}
                    aria-label={`${t(label)} -${step}`}
                    onMouseDown={onDecrementMouseDown}
                    onMouseUp={onDecrementMouseUp}
                    onMouseLeave={cancel}
                >
                    <Minus />
                </IconButton>
                <div className={'flex-1 text-center font-medium text-fg'}>
                    {valueLabel}
                </div>
                <IconButton
                    disabled={increaseDisabled}
                    className={STEP_BUTTON}
                    aria-label={`${t(label)} +${step}`}
                    onMouseDown={onIncrementMouseDown}
                    onMouseUp={onIncrementMouseUp}
                    onMouseLeave={cancel}
                >
                    <Plus />
                </IconButton>
            </div>
        </div>
    );
};

export default Stepper;
