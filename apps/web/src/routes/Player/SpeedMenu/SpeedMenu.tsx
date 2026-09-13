// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Playback-speed panel. Fixed-position, state-driven floating <div> (opened from Player
 * state, not a menu/popover trigger) whose close rides native mousedown bubbling to the
 * Player's onContainerMouseDown; see the researched KEEP note at the menu-layer mount in
 * Player.tsx for why no 2026 primitive fits. One control (FineStepper): a slider for
 * anything between 0.1x and 4x in 0.05 steps, -/+ for 0.25 jumps, a typed value. The
 * old 0.25x..2x preset list is gone - every preset is one or two taps away on it.
 */

import React, { forwardRef, memo, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from 'rillio/components/ui';
import ShaderBlurRect from '../ShaderBlurRect';
import SnapshotBackdrop from '../SnapshotBackdrop';
import FineStepper from '../FineStepper';

// The control's bounds; the keyboard shortcut in Player.tsx clamps to the same.
export const MIN_SPEED = 0.1;
export const MAX_SPEED = 4;

type Props = {
    className?: string;
    playbackSpeed?: number | null;
    onPlaybackSpeedChanged?: (value: number) => void;
};

const SpeedMenu = memo(forwardRef<HTMLDivElement, Props>(function SpeedMenu({ className, playbackSpeed, onPlaybackSpeedChanged }, ref) {
    const { t } = useTranslation();
    const onMouseDown = useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).speedMenuClosePrevented = true;
    }, []);
    const onChange = useCallback((value: number) => {
        if (typeof onPlaybackSpeedChanged === 'function') {
            onPlaybackSpeedChanged(value);
        }
    }, [onPlaybackSpeedChanged]);
    return (
        <div ref={ref} className={cn('w-72', className)} onMouseDown={onMouseDown}>
            <SnapshotBackdrop />
            <ShaderBlurRect />
            <FineStepper
                className={'px-6 pb-5 pt-5'}
                label={'PLAYBACK_SPEED'}
                value={typeof playbackSpeed === 'number' ? playbackSpeed : null}
                unit={'x'}
                min={MIN_SPEED}
                max={MAX_SPEED}
                disabled={typeof playbackSpeed !== 'number'}
                onChange={onChange}
                resetValue={1}
            />
        </div>
    );
}));

export default SpeedMenu;
