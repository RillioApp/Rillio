// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Audio-track picker. Fixed-position, state-driven single-select <div> panel whose close
 * rides native mousedown bubbling to the Player's onContainerMouseDown (see the researched
 * KEEP note at the menu-layer mount in Player.tsx for why no 2026 primitive fits). Restyled
 * onto Tailwind tokens + the kit Button. Same rows, same dispatch.
 */

import React, { forwardRef, memo, MouseEvent, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { languages } from 'rillio/common';
import { Button } from 'rillio/components/ui';
import { cn } from 'rillio/components/ui';
import ShaderBlurRect from '../ShaderBlurRect';
import SnapshotBackdrop from '../SnapshotBackdrop';
import Stepper from '../SubtitlesMenu/Stepper';
import DelayFineControl from '../DelayFineControl';

type Props = {
    className?: string;
    selectedAudioTrackId: string | null;
    audioTracks: AudioTrack[];
    // ms, null until the player reports it (no stream / non-shell playback).
    audioDelay: number | null;
    onAudioTrackSelected: (id: string) => void;
    onAudioDelayChanged: (delayMs: number) => void;
};

const AudioMenu = memo(forwardRef<HTMLDivElement, Props>(function AudioMenu({ className, selectedAudioTrackId, audioTracks, audioDelay, onAudioTrackSelected, onAudioDelayChanged }, ref) {
    const { t } = useTranslation();

    const onAudioTrackClick = useCallback(({ currentTarget }: MouseEvent) => {
        const id = currentTarget.getAttribute('data-id')!;
        onAudioTrackSelected && onAudioTrackSelected(id);
    }, [onAudioTrackSelected]);

    // Seconds from the controls, ms to the video (mirrors the subtitles menu).
    const onDelayChanged = useCallback((seconds: number) => {
        if (typeof audioDelay === 'number') {
            onAudioDelayChanged(Math.round(seconds * 1000));
        }
    }, [audioDelay, onAudioDelayChanged]);
    const delaySeconds = typeof audioDelay === 'number' ? audioDelay / 1000 : null;

    const onMouseDown = (event: MouseEvent) => {
        (event.nativeEvent as any).audioMenuClosePrevented = true;
    };

    return (
        <div ref={ref} className={cn('flex flex-row', className)} onMouseDown={onMouseDown}>
            <SnapshotBackdrop />
            <ShaderBlurRect />
            <div className={'flex max-h-[32rem] w-72 flex-none flex-col self-stretch'}>
                <div className={'flex-none px-8 py-6 font-bold text-fg'}>
                    {t('AUDIO_TRACKS')}
                </div>
                <div className={'flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto px-4 pb-4'}>
                    {
                        audioTracks.map(({ id, label, lang }, index) => {
                            const selected = selectedAudioTrackId === id;
                            return (
                                <Button
                                    key={index}
                                    variant={'ghost'}
                                    title={label}
                                    data-id={id}
                                    onClick={onAudioTrackClick}
                                    className={cn(
                                        'flex h-16 w-full flex-none gap-4 rounded-card px-6 hover:bg-surface-hover',
                                        selected && 'bg-accent-soft',
                                    )}
                                >
                                    <div className={'flex flex-1 flex-col gap-1 overflow-hidden text-left'}>
                                        <div className={'truncate text-[1.1rem] leading-6 text-fg'}>
                                            {languages.label(lang)}
                                        </div>
                                        <div className={'truncate text-[0.9rem] text-fg-muted'}>
                                            {label}
                                        </div>
                                    </div>
                                    {selected ? <div className={'size-2 flex-none rounded-full bg-primary'} /> : null}
                                </Button>
                            );
                        })
                    }
                </div>
                {/* A/V sync lives with the audio track it shifts: coarse 0.25s
                    steps to get close, 0.05s fine steps to land it. Positive
                    delays the audio (sound too EARLY -> go positive). */}
                <div className={'flex-none border-t border-line pt-4'}>
                    <Stepper
                        className={'px-6 pb-3'}
                        label={'AUDIO_DELAY_TITLE'}
                        value={delaySeconds}
                        unit={'s'}
                        step={0.25}
                        disabled={delaySeconds === null}
                        onChange={onDelayChanged}
                    />
                    <DelayFineControl
                        className={'px-6 pb-4'}
                        value={delaySeconds}
                        disabled={delaySeconds === null}
                        onChange={onDelayChanged}
                    />
                </div>
            </div>
        </div>
    );
}));

export default AudioMenu;
