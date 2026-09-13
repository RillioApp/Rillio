// Copyright (C) 2017-2026 Smart code 203358507

import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import debounce from 'lodash.debounce';
import useRouteFocused from 'rillio/common/useRouteFocused';
import { Slider } from 'rillio/components';
import { cn } from 'rillio/components/ui/cn';
import usePlayerThumb from './usePlayerThumb';
import formatTime from './formatTime';

// The seek bar's filled track + thumb are the accent color, with a hover-grown
// thumb carrying an inset accent glow. These were the only reasons SeekBar had its
// own .less (to reach the Slider's hashed part classes); they are now passed straight
// through the Slider's per-part className props.
// Track = faint light (blue-tinted ice at low alpha, not the old orange-at-20%);
// buffered = a slightly stronger ice (reads as "downloaded", still not grey);
// the filled range up to the scrubber and the thumb stay the accent #FFA033.
// The timecode display lives in the ControlBar's time island (not on the bar's
// sides); onSeekPreview streams the scrub position so the island live-updates.
const TRACK = 'bg-ice/10 opacity-100';
const BUFFERED = 'bg-ice/30';
const FILLED = 'bg-(--color-accent)';
const THUMB = 'bg-(--color-accent) transition-transform duration-150 group-hover:scale-[1.2]';

type Chapter = { time: number, title?: string | null };

// Two boundaries closer than this (fraction of the duration) merge: encoders
// sometimes emit a stray zero-length chapter, and two gap markers one pixel
// apart read as dirt on the bar.
const MIN_BOUNDARY_GAP = 0.01;

type Props = {
    className?: string;
    time: number | null;
    duration: number | null;
    buffered?: number;
    // The URL mpv is actually playing (video.state.stream.url): what the
    // shell's shadow player opens for trickplay thumbnails. Shell-only.
    thumbStreamUrl?: string | null;
    // The file's chapter marks (ms), from the player bridge. When present the
    // bar renders YouTube-style: gaps at each boundary, chapter title in the
    // hover preview.
    chapters?: Chapter[];
    onSeekRequested?: (time: number) => void;
    onSeekPreview?: (time: number | null) => void;
};

const SeekBar = ({ className, time, duration, buffered, thumbStreamUrl, chapters, onSeekRequested, onSeekPreview }: Props) => {
    const disabled = time === null || isNaN(time as number) || duration === null || isNaN(duration as number);
    const routeFocused = useRouteFocused();
    const [seekTime, setSeekTime] = useState<number | null>(null);

    // Trickplay: the fraction of the bar under the cursor. Plain hover only -
    // during a DRAG the window-grab model turns body pointer-events off, so
    // these mouse events stop firing and the preview follows seekTime instead.
    const barRef = useRef<HTMLDivElement | null>(null);
    const [hoverFraction, setHoverFraction] = useState<number | null>(null);
    const onBarMouseMove = useCallback((event: React.MouseEvent) => {
        const rect = barRef.current?.getBoundingClientRect();
        if (!rect || rect.width <= 0) return;
        setHoverFraction(Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width)));
    }, []);
    const onBarMouseLeave = useCallback(() => setHoverFraction(null), []);

    // Where the preview sits and what moment it shows: the drag position while
    // scrubbing (seekTime), else the hovered position.
    const previewTimeMs = !disabled ?
        (seekTime !== null ? seekTime : hoverFraction !== null ? hoverFraction * (duration as number) : null)
        :
        null;
    const previewFraction = !disabled && previewTimeMs !== null ? previewTimeMs / (duration as number) : null;
    const thumb = usePlayerThumb(thumbStreamUrl ?? null, previewTimeMs);

    // Chapter boundaries as bar fractions: sorted, interior-only (a mark at 0
    // or at the very end draws no gap), near-duplicates merged.
    const chapterMarks = React.useMemo(() => {
        if (disabled || !Array.isArray(chapters) || chapters.length === 0) return [];
        const marks: { fraction: number, title: string | null }[] = [];
        for (const chapter of [...chapters].sort((a, b) => a.time - b.time)) {
            const fraction = chapter.time / (duration as number);
            if (fraction <= MIN_BOUNDARY_GAP || fraction >= 1 - MIN_BOUNDARY_GAP) continue;
            if (marks.length > 0 && fraction - marks[marks.length - 1].fraction < MIN_BOUNDARY_GAP) continue;
            marks.push({ fraction, title: typeof chapter.title === 'string' && chapter.title.length > 0 ? chapter.title : null });
        }
        return marks;
    }, [chapters, duration, disabled]);
    // The chapter under the preview cursor (for the hover card's title line).
    // Untitled or number-only chapters ("Chapter 3") add nothing over the
    // timestamp already shown - skip them.
    // Chapter gaps as a mask on the Slider's bar layers: transparent 4px holes
    // at each boundary that the video actually shows through.
    const barMask = React.useMemo(() => {
        if (chapterMarks.length === 0) return undefined;
        const stops = ['black 0%'];
        for (const { fraction } of chapterMarks) {
            const at = `${(fraction * 100).toFixed(3)}%`;
            stops.push(
                `black calc(${at} - 2px)`,
                `transparent calc(${at} - 2px)`,
                `transparent calc(${at} + 2px)`,
                `black calc(${at} + 2px)`,
            );
        }
        stops.push('black 100%');
        return `linear-gradient(to right, ${stops.join(', ')})`;
    }, [chapterMarks]);

    const previewChapterTitle = React.useMemo(() => {
        if (previewFraction === null || chapterMarks.length === 0 || !Array.isArray(chapters)) return null;
        let title: string | null = null;
        for (const chapter of [...chapters].sort((a, b) => a.time - b.time)) {
            if (chapter.time / (duration as number) > previewFraction) break;
            title = typeof chapter.title === 'string' && chapter.title.length > 0 ? chapter.title : null;
        }
        return title !== null && !/^chapter\s*\d*$/i.test(title.trim()) ? title : null;
    }, [previewFraction, chapterMarks, chapters, duration]);

    const setSeekTimeAndPreview = useCallback((value: number | null) => {
        setSeekTime(value);
        if (typeof onSeekPreview === 'function') {
            onSeekPreview(value);
        }
    }, [onSeekPreview]);
    const resetTimeDebounced = useCallback(debounce(() => {
        setSeekTimeAndPreview(null);
    }, 1500), [setSeekTimeAndPreview]);
    const onSlide = useCallback((value: number) => {
        resetTimeDebounced.cancel();
        setSeekTimeAndPreview(value);
    }, [setSeekTimeAndPreview]);
    const onComplete = useCallback((value: number) => {
        resetTimeDebounced();
        setSeekTimeAndPreview(value);
        if (typeof onSeekRequested === 'function') {
            onSeekRequested(value);
        }
    }, [onSeekRequested, setSeekTimeAndPreview]);
    useLayoutEffect(() => {
        if (!routeFocused || disabled) {
            resetTimeDebounced.cancel();
            setSeekTimeAndPreview(null);
        }
    }, [routeFocused, disabled]);
    useEffect(() => {
        return () => {
            resetTimeDebounced.cancel();
        };
    }, []);

    return (
        <div
            ref={barRef}
            className={cn('relative flex flex-row items-center overflow-visible', className)}
            onMouseMove={onBarMouseMove}
            onMouseLeave={onBarMouseLeave}
        >
            {
                // Trickplay preview: a small frame + timestamp riding above the
                // cursor/scrub position. The IMAGE only renders when the shell's
                // shadow player has one (shell-only; approximate by keyframe).
                // pointer-events-none: it floats over the bar and must never
                // steal the hover/drag it is following. The overflow-visible on
                // the container is load-bearing against the global reset.
                // YouTube's shape: a bare rounded frame, and UNDER it a pill
                // with the timestamp and the chapter title side by side (the
                // pill is what shows when there is no frame yet).
                previewFraction !== null ?
                    <div
                        className={'pointer-events-none absolute bottom-full z-0 mb-3 flex -translate-x-1/2 flex-col items-center gap-2'}
                        style={{ left: `clamp(8rem, ${previewFraction * 100}%, calc(100% - 8rem))` }}
                    >
                        {
                            thumb !== null ?
                                <img src={thumb} alt={''} className={'block w-64 rounded-xl shadow-elevated'} draggable={false} />
                                :
                                null
                        }
                        <div className={'flex max-w-64 items-center gap-2 rounded-full bg-black/60 px-3 py-1 text-sm text-ice backdrop-blur-sm'}>
                            <span className={'tabular-nums'}>{formatTime(previewTimeMs as number)}</span>
                            {
                                previewChapterTitle !== null ?
                                    <span className={'truncate font-semibold'}>{previewChapterTitle}</span>
                                    :
                                    null
                            }
                        </div>
                    </div>
                    :
                    null
            }
            {
                // The hovered/scrubbed chapter grows, YouTube-style: a taller
                // translucent band over exactly that segment. Positioned in the
                // SLIDER's coordinate space (it is inset by mx-(--thumb-size)).
                // The gaps themselves are cut into the bar via the Slider's
                // barMask below - real holes, not painted notches (a notch over
                // the translucent track reads as a black rectangle on bright
                // video).
                chapterMarks.length > 0 && previewFraction !== null ?
                    <div className={'pointer-events-none absolute inset-y-0 left-(--thumb-size) right-(--thumb-size) z-[1]'}>
                        {
                            (() => {
                                const bounds = [0, ...chapterMarks.map(({ fraction }) => fraction), 1];
                                const index = bounds.findIndex((b, i) => previewFraction >= b && previewFraction < bounds[i + 1]);
                                if (index < 0) return null;
                                const [start, end] = [bounds[index], bounds[index + 1]];
                                return (
                                    <div
                                        className={'absolute top-1/2 h-[calc(var(--track-size)*2.2)] -translate-y-1/2 rounded-(--track-size) bg-ice/20 transition-[left,width] duration-100 ease-smooth'}
                                        style={{ left: `${start * 100}%`, width: `${(end - start) * 100}%` }}
                                    />
                                );
                            })()
                        }
                    </div>
                    :
                    null
            }
            <Slider
                className={'mx-(--thumb-size) flex-1 self-stretch'}
                trackClassName={TRACK}
                bufferedClassName={BUFFERED}
                filledClassName={FILLED}
                thumbClassName={THUMB}
                barMask={barMask}
                value={
                    !disabled ?
                        seekTime !== null ? seekTime : (time as number)
                        :
                        0
                }
                buffered={buffered}
                minimumValue={0}
                maximumValue={duration as number}
                disabled={disabled}
                onSlide={onSlide}
                onComplete={onComplete}
            />
        </div>
    );
};

export default SeekBar;
