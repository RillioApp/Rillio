// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Audio-track picker. Fixed-position, state-driven single-select <div> panel whose close
 * rides native mousedown bubbling to the Player's onContainerMouseDown (see the researched
 * KEEP note at the menu-layer mount in Player.tsx for why no 2026 primitive fits). Restyled
 * onto Tailwind tokens + the kit Button. Same rows, same dispatch.
 */

import React, { forwardRef, memo, MouseEvent, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Loader2, Sparkles } from 'lucide-react';
import { languages } from 'rillio/common';
import { Button } from 'rillio/components/ui';
import { cn } from 'rillio/components/ui';
import ShaderBlurRect from '../ShaderBlurRect';
import SnapshotBackdrop from '../SnapshotBackdrop';
import FineStepper from '../FineStepper';
import type { DubSource, DubState } from '../useDub';

type Props = {
    className?: string;
    selectedAudioTrackId: string | null;
    audioTracks: AudioTrack[];
    // ms, null until the player reports it (no stream / non-shell playback).
    audioDelay: number | null;
    onAudioTrackSelected: (id: string) => void;
    onAudioDelayChanged: (delayMs: number) => void;
    // The AI dub row (shell only): its phase, whether it is the chosen track,
    // whether its audio is the one heard yet, its click.
    dub?: DubState;
    dubChosen?: boolean;
    dubPlaying?: boolean;
    onDubSelect?: () => void;
    // The dub's translation source: the viewer's choice, the one in effect,
    // whether loaded English subtitles exist to be it, and the change.
    dubSource?: DubSource;
    dubSourceInUse?: DubSource;
    dubSubtitlesUsable?: boolean;
    onDubSourceChange?: (source: DubSource) => void;
};

const GIGABYTE = 1e9;
const DUB_SOURCES: { source: DubSource, label: string }[] = [
    { source: 'ai', label: 'AUDIO_DUB_SOURCE_AI' },
    { source: 'subtitles', label: 'AUDIO_DUB_SOURCE_SUBTITLES' },
];

const AudioMenu = memo(forwardRef<HTMLDivElement, Props>(function AudioMenu({ className, selectedAudioTrackId, audioTracks, audioDelay, onAudioTrackSelected, onAudioDelayChanged, dub, dubChosen, dubPlaying, onDubSelect, dubSource, dubSourceInUse, dubSubtitlesUsable, onDubSourceChange }, ref) {
    const { t } = useTranslation();

    // The dub track's own row carries its state; the plain list never shows it twice.
    const listedTracks = audioTracks.filter((track) => !track.generated);
    // The sub-line tells the listener what they hear: while the player holds
    // on audio not made yet it is buffering (with the seconds ready ahead);
    // while the audio flows, the dub runs ahead.
    const working = dub !== undefined && (dub.state === 'preparing' || dub.state === 'running');
    const buffering = working && (dub.state === 'preparing' || dub.waiting || !dubPlaying);
    const dubDetail = dub === undefined ? null :
        dub.state === 'needs-pack' ? t('AUDIO_DUB_DOWNLOAD', { size: `${((dub.packBytes ?? 0) / GIGABYTE).toFixed(1)} GB` }) :
        dub.state === 'downloading' ? t('AUDIO_DUB_DOWNLOADING', { percent: Math.round((dub.progress ?? 0) * 100) }) :
        buffering ? t('AUDIO_DUB_PREPARING', { seconds: Math.round(dub.aheadS) }) :
        working ? t('AUDIO_DUB_RUNNING') :
        dub.state === 'done' ? t('AUDIO_DUB_DONE') :
        dub.state === 'error' ? dub.detail :
        null;
    // The spinner means "the audio is not flowing yet".
    const dubBusy = dub !== undefined && (dub.state === 'downloading' || buffering);

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
                        listedTracks.map(({ id, label, lang }, index) => {
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
                    {
                        // The AI dub: a track row of its own. Picking it downloads
                        // the pack (size shown first), starts the dub, or comes back
                        // to its track; picking any other track stops the worker.
                        dub?.supported ?
                            <Button
                                variant={'ghost'}
                                title={t('AUDIO_DUB_HINT')}
                                onClick={onDubSelect}
                                className={cn(
                                    'flex h-16 w-full flex-none gap-4 rounded-card px-6 hover:bg-surface-hover',
                                    dubChosen && 'bg-accent-soft',
                                )}
                            >
                                {
                                    dubBusy ?
                                        <Loader2 className={'size-4 flex-none animate-spin text-fg'} />
                                        :
                                        <Sparkles className={'size-4 flex-none text-fg'} />
                                }
                                <div className={'flex flex-1 flex-col gap-1 overflow-hidden text-left'}>
                                    <div className={'truncate text-[1.1rem] leading-6 text-fg'}>
                                        {t('AUDIO_DUB_AI')}
                                    </div>
                                    {
                                        dubDetail ?
                                            <div className={'truncate text-[0.9rem] text-fg-muted'}>
                                                {dubDetail}
                                            </div>
                                            :
                                            null
                                    }
                                </div>
                                {dubChosen ? <div className={'size-2 flex-none rounded-full bg-primary'} /> : null}
                            </Button>
                            :
                            null
                    }
                    {
                        // Where the dub's lines come from: the recognizer and the
                        // translator, or the external subtitles the viewer loaded.
                        dub?.supported && dubSource !== undefined && onDubSourceChange ?
                            <div className={'flex flex-none flex-col gap-2 px-6 pt-1 pb-2'}>
                                <div className={'text-[0.6875rem] font-medium uppercase tracking-wider text-fg-muted'}>
                                    {t('AUDIO_DUB_SOURCE')}
                                </div>
                                <div className={'flex gap-2'}>
                                    {
                                        DUB_SOURCES.map(({ source, label }) => {
                                            const unavailable = source === 'subtitles' && !dubSubtitlesUsable;
                                            return (
                                                <button
                                                    key={source}
                                                    type={'button'}
                                                    aria-pressed={dubSource === source}
                                                    title={unavailable ? t('AUDIO_DUB_SOURCE_SUBTITLES_MISSING') : undefined}
                                                    onClick={() => onDubSourceChange(source)}
                                                    className={cn(
                                                        'rounded-full px-3 py-1.5 text-sm font-medium text-fg transition hover:brightness-110',
                                                        dubSource === source ? 'bg-accent-soft' : 'bg-surface',
                                                        unavailable && 'opacity-50',
                                                    )}
                                                >
                                                    {t(label)}
                                                </button>
                                            );
                                        })
                                    }
                                </div>
                                {
                                    // The choice is kept, and says why it is not in effect yet.
                                    dubSource === 'subtitles' && dubSourceInUse === 'ai' ?
                                        <div className={'text-xs leading-relaxed text-fg-muted'}>
                                            {t('AUDIO_DUB_SOURCE_SUBTITLES_MISSING')}
                                        </div>
                                        :
                                        null
                                }
                            </div>
                            :
                            null
                    }
                </div>
                {/* A/V sync lives with the audio track it shifts: coarse 0.25s
                    steps to get close, 0.05s fine steps to land it. Positive
                    delays the audio (sound too EARLY -> go positive). */}
                <div className={'flex-none border-t border-line pt-4'}>
                    <FineStepper
                        className={'px-6 pb-4'}
                        label={'AUDIO_DELAY_TITLE'}
                        value={delaySeconds}
                        unit={'s'}
                        disabled={delaySeconds === null}
                        onChange={onDelayChanged}
                        resetValue={0}
                    />
                </div>
            </div>
        </div>
    );
}));

export default AudioMenu;
