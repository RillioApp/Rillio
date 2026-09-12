// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Player control bar. Clean-room restyle onto the kit IconButton + Tailwind tokens;
 * every handler, the closePrevented mousedown protocol, tabIndex=-1, the play/pause
 * icon-swap, volume tiering, scale-cycle, the mobile overflow popover and the touch
 * forwarding are preserved verbatim. The custom rillio Slider (SeekBar / VolumeSlider)
 * is kept. No <video> is owned here - all state arrives via props from Player.
 *
 * Disabled buttons are DIMMED only (opacity), never pointer-blocked: the onClick
 * handlers self-guard exactly as before, matching the old `.disabled` icon-dim look.
 */

import React, { forwardRef, useCallback, useEffect, useState } from 'react';
import { Play, Pause, SkipForward, VolumeX, Volume, Volume1, Volume2, MoreVertical, Activity, Gauge, Cast, Captions, AudioLines, ListVideo, Scaling, Minimize, Maximize, MoreHorizontal } from 'lucide-react';
import { t } from 'i18next';
import { useServices } from 'rillio/services';
import { useBinaryState, usePlatform, useFullscreen } from 'rillio/common';
import { IconButton } from 'rillio/components/ui';
import { cn } from 'rillio/components/ui';
import SeekBar from './SeekBar';
import formatTime from './SeekBar/formatTime';
import VolumeSlider from './VolumeSlider';

// 4rem square bare-glyph button in the control-bar idiom: ice glyph (blue-tinted
// off-white, not flat grey) that lifts to pure white on hover, press-scale, and a
// dim (not blocked) disabled. Icons use the player icon-size token so every
// control-bar glyph is identical (and smaller than the old 2.2rem).
const CB_BUTTON = 'size-10 shrink-0 rounded-full bg-transparent opacity-100 transition-colors duration-150 hover:bg-white/10 hover:opacity-100 active:scale-[0.97] [&_svg]:size-(--icon-size-player) [&_svg]:text-ice [&_svg]:transition-colors [&_svg]:duration-150 [&:hover_svg]:text-white';
const CB_ICON = 'size-(--icon-size-player)';

// Control-bar "islands": rounded-full translucent containers that group the icon
// clusters (transport left, menus right) over the video. A deliberate grouping
// surface (very low opacity black + backdrop blur), unlike decorative wrappers.
// Uniform padding all around (px == py) keeps the pill's endcap radius concentric
// with the circular button hover shapes; gap-2 gives each button breathing room.
// flex-none is load-bearing: without it a tight row compresses the island and its
// buttons below their circular size (the right island always had it).
const CB_ISLAND = 'flex flex-none flex-row items-center gap-2 rounded-full bg-glass-chrome p-1.5 backdrop-blur-(--glass-blur)';

type Props = {
    className?: string;
    paused?: boolean | null;
    time?: number | null;
    duration?: number | null;
    buffered?: number;
    volume?: number | null;
    muted?: boolean | null;
    playbackSpeed?: number | null;
    subtitlesTracks?: unknown[];
    audioTracks?: unknown[];
    metaItem?: { content?: { videos?: unknown[] } } | null;
    nextVideo?: unknown | null;
    stream?: unknown | null;
    thumbStreamUrl?: string | null;
    chapters?: { time: number, title?: string | null }[];
    videoScale?: string | null;
    videoScaleLabel?: string;
    onVideoScaleChanged?: () => void;
    onPlayRequested?: () => void;
    onPauseRequested?: () => void;
    onNextVideoRequested?: () => void;
    onMuteRequested?: () => void;
    onUnmuteRequested?: () => void;
    onVolumeChangeRequested?: (volume: number) => void;
    onSeekRequested?: (time: number) => void;
    onToggleSubtitlesMenu?: () => void;
    onToggleAudioMenu?: () => void;
    onToggleSpeedMenu?: () => void;
    onToggleSideDrawer?: () => void;
    onToggleOptionsMenu?: () => void;
    onToggleStatisticsMenu?: () => void;
    onMouseOver?: (event: React.MouseEvent) => void;
    onMouseMove?: (event: React.MouseEvent) => void;
    onTouchEnd?: (event: React.TouchEvent) => void;
};

const ControlBar = forwardRef<HTMLDivElement, Props>(function ControlBar({
    className,
    paused,
    time,
    duration,
    buffered,
    volume,
    muted,
    playbackSpeed,
    subtitlesTracks,
    audioTracks,
    metaItem,
    nextVideo,
    stream,
    thumbStreamUrl,
    chapters,
    onPlayRequested,
    onPauseRequested,
    onNextVideoRequested,
    onMuteRequested,
    onUnmuteRequested,
    onVolumeChangeRequested,
    onSeekRequested,
    onToggleSubtitlesMenu,
    onToggleAudioMenu,
    onToggleSpeedMenu,
    onToggleSideDrawer,
    onToggleOptionsMenu,
    videoScale,
    videoScaleLabel,
    onVideoScaleChanged,
    onToggleStatisticsMenu,
    onTouchEnd,
    ...props
}, ref) {
    const { chromecast } = useServices();
    const platform = usePlatform();
    // Fullscreen lives here (next to the scale/fit control), not on the player's
    // top bar, that one duplicates the window header's fullscreen in the shell.
    const [fullscreen, requestFullscreen, exitFullscreen, , fullscreenSupported] = useFullscreen();
    const [chromecastServiceActive, setChromecastServiceActive] = useState(() => chromecast.active);
    const [buttonsMenuOpen, , , toggleButtonsMenu] = useBinaryState(false);
    // Timecode island state: the SeekBar streams its scrub position up so the
    // island live-updates mid-drag; remaining-time mode toggles on click.
    const [seekPreview, setSeekPreview] = useState<number | null>(null);
    const [remainingTimeMode, , , toggleRemainingTimeMode] = useBinaryState(false);
    const onSubtitlesButtonMouseDown = useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).subtitlesMenuClosePrevented = true;
    }, []);
    const onAudioButtonMouseDown = useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).audioMenuClosePrevented = true;
    }, []);
    const onSpeedButtonMouseDown = useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).speedMenuClosePrevented = true;
    }, []);
    const onVideosButtonMouseDown = useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).videosMenuClosePrevented = true;
    }, []);
    const onOptionsButtonMouseDown = useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).optionsMenuClosePrevented = true;
    }, []);
    const onStatisticsButtonMouseDown = useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).statisticsMenuClosePrevented = true;
    }, []);
    const onPlayPauseButtonClick = useCallback(() => {
        if (paused) {
            if (typeof onPlayRequested === 'function') {
                onPlayRequested();
            }
        } else {
            if (typeof onPauseRequested === 'function') {
                onPauseRequested();
            }
        }
    }, [paused, onPlayRequested, onPauseRequested]);
    const onNextVideoButtonClick = useCallback(() => {
        if (nextVideo !== null && typeof onNextVideoRequested === 'function') {
            onNextVideoRequested();
        }
    }, [nextVideo, onNextVideoRequested]);
    const onMuteButtonClick = useCallback(() => {
        if (muted) {
            if (typeof onUnmuteRequested === 'function') {
                onUnmuteRequested();
            }
        } else {
            if (typeof onMuteRequested === 'function') {
                onMuteRequested();
            }
        }
    }, [muted, onMuteRequested, onUnmuteRequested]);
    const onChromecastButtonClick = useCallback(() => {
        chromecast.transport.requestSession();
    }, []);
    useEffect(() => {
        const onStateChanged = () => {
            setChromecastServiceActive(chromecast.active);
        };
        chromecast.on('stateChanged', onStateChanged);
        return () => {
            chromecast.off('stateChanged', onStateChanged);
        };
    }, []);

    const VolumeIcon =
        (typeof muted === 'boolean' && muted) ? VolumeX :
            (volume === null || isNaN(volume as number)) ? VolumeX :
                volume === 0 ? VolumeX :
                    (volume as number) < 30 ? Volume :
                        (volume as number) < 70 ? Volume1 :
                            Volume2;

    return (
        <div
            ref={ref}
            {...props}
            onTouchStart={props.onMouseOver as unknown as React.TouchEventHandler}
            onTouchMove={props.onMouseMove as unknown as React.TouchEventHandler}
            onTouchEnd={onTouchEnd}
            className={cn('px-8 max-sm:px-0', className)}
        >
            <SeekBar
                className={'h-10 [--track-size:0.3rem] [--thumb-size:1rem] max-sm:mx-6'}
                time={time ?? null}
                duration={duration ?? null}
                buffered={buffered}
                thumbStreamUrl={thumbStreamUrl}
                chapters={chapters}
                onSeekRequested={onSeekRequested}
                onSeekPreview={setSeekPreview}
            />
            <div className={'flex flex-row items-center gap-3 px-3 pb-2 max-sm:relative max-sm:gap-[0.15rem] max-sm:overflow-visible max-sm:px-2'}>
                <div className={CB_ISLAND}>
                    <IconButton className={cn(CB_BUTTON, typeof paused !== 'boolean' && 'opacity-40')} title={paused ? t('PLAYER_PLAY') : t('PLAYER_PAUSE')} tabIndex={-1} onClick={onPlayPauseButtonClick}>
                        {typeof paused !== 'boolean' || paused ? <Play className={CB_ICON} /> : <Pause className={CB_ICON} />}
                    </IconButton>
                    {
                        nextVideo !== null ?
                            <IconButton className={CB_BUTTON} title={t('PLAYER_NEXT_VIDEO')} tabIndex={-1} onClick={onNextVideoButtonClick}>
                                <SkipForward className={CB_ICON} />
                            </IconButton>
                            :
                            null
                    }
                    <IconButton className={cn(CB_BUTTON, typeof muted !== 'boolean' && 'opacity-40')} title={muted ? t('PLAYER_UNMUTE') : t('PLAYER_MUTE')} tabIndex={-1} onClick={onMuteButtonClick}>
                        <VolumeIcon className={CB_ICON} />
                    </IconButton>
                    {
                        // The volume slider belongs to the transport island (it is
                        // part of the audio cluster, not a floating control).
                        !platform.isMobile ?
                            // Explicit width, not flex-basis: the flex-none island
                            // sizes to intrinsic content, and the Slider (absolute
                            // inner layers) has none - a basis collapses to 0 here.
                            <VolumeSlider
                                className={'mr-2 h-10 w-36 flex-none [--thumb-size:0.9rem] [--track-size:0.3rem]'}
                                volume={volume ?? null}
                                muted={muted ?? undefined}
                                onVolumeChangeRequested={onVolumeChangeRequested}
                            />
                            : null
                    }
                </div>
                {
                    // Timecode island: "current / duration" in its own pill after the
                    // transport cluster (not on the seek bar's sides). Shows the scrub
                    // position live while dragging; click toggles remaining-time mode.
                    <button
                        type="button"
                        tabIndex={-1}
                        onClick={toggleRemainingTimeMode}
                        className={cn(CB_ISLAND, 'cursor-pointer select-none self-stretch whitespace-nowrap px-4 text-base font-semibold tracking-tight tabular-nums text-ice transition-colors duration-150 hover:text-white')}
                    >
                        {
                            // Left side: the position. Remaining-time mode replaces IT
                            // (a countdown is a position, not a length); the scrub
                            // preview overrides both while dragging.
                            seekPreview !== null ?
                                formatTime(seekPreview)
                                :
                                remainingTimeMode && typeof duration === 'number' && !isNaN(duration) && typeof time === 'number' ?
                                    '-' + formatTime((duration - time) / ((playbackSpeed as number) || 1))
                                    :
                                    formatTime(time ?? null)
                        }
                        <span className="mx-1.5 font-normal text-ice-muted">/</span>
                        {/* Right side: always the total duration. */}
                        {formatTime(duration ?? null)}
                    </button>
                }
                <div className={'flex-1'} />
                <IconButton className={cn(CB_BUTTON, 'hidden max-sm:flex')} onClick={toggleButtonsMenu}>
                    <MoreVertical className={CB_ICON} />
                </IconButton>
                <div className={cn(
                    // Desktop: the right menus island (min-width sm only, so it never
                    // clashes with the mobile overflow-popover styling below).
                    'flex flex-none flex-row gap-2 sm:rounded-full sm:bg-glass-chrome sm:p-1.5 sm:backdrop-blur-(--glass-blur)',
                    // Mobile: an overflow MENU, so it takes the house floating-panel
                    // material (glass-panel + border-line + shadow-elevated + glass blur)
                    // rather than the lighter chrome island above.
                    'max-sm:absolute max-sm:bottom-[4.5rem] max-sm:right-0 max-sm:m-2 max-sm:max-w-[calc(100dvw-1rem)] max-sm:gap-[0.15rem] max-sm:overflow-x-auto max-sm:rounded-card max-sm:border max-sm:border-line max-sm:bg-glass-panel max-sm:p-2 max-sm:shadow-elevated max-sm:backdrop-blur-(--glass-blur)',
                    buttonsMenuOpen ? 'max-sm:flex' : 'max-sm:hidden',
                )}>
                    {/* Every icon-only control carries its name (title doubles as
                        the accessible name): these were unnamed to assistive tech,
                        which also made them unreachable for automation. */}
                    <IconButton className={cn(CB_BUTTON, !stream && 'opacity-40')} title={t('SETTINGS_SHORTCUT_MENU_STATISTICS')} tabIndex={-1} onMouseDown={onStatisticsButtonMouseDown} onClick={onToggleStatisticsMenu}>
                        <Activity className={CB_ICON} />
                    </IconButton>
                    <IconButton className={cn(CB_BUTTON, playbackSpeed === null && 'opacity-40')} title={t('SETTINGS_SHORTCUT_MENU_PLAYBACK_SPEED')} tabIndex={-1} onMouseDown={onSpeedButtonMouseDown} onClick={onToggleSpeedMenu}>
                        <Gauge className={CB_ICON} />
                    </IconButton>
                    <IconButton className={cn(CB_BUTTON, !chromecastServiceActive && 'opacity-40')} title={t('CAST')} tabIndex={-1} onClick={onChromecastButtonClick}>
                        <Cast className={CB_ICON} />
                    </IconButton>
                    <IconButton className={cn(CB_BUTTON, (!Array.isArray(subtitlesTracks) || subtitlesTracks.length === 0) && 'opacity-40')} title={t('SETTINGS_SHORTCUT_MENU_SUBTITLES')} tabIndex={-1} onMouseDown={onSubtitlesButtonMouseDown} onClick={onToggleSubtitlesMenu}>
                        <Captions className={CB_ICON} />
                    </IconButton>
                    <IconButton className={cn(CB_BUTTON, (!Array.isArray(audioTracks) || audioTracks.length === 0) && 'opacity-40')} title={t('SETTINGS_SHORTCUT_MENU_AUDIO')} tabIndex={-1} onMouseDown={onAudioButtonMouseDown} onClick={onToggleAudioMenu}>
                        <AudioLines className={CB_ICON} />
                    </IconButton>
                    {
                        (metaItem?.content?.videos?.length ?? 0) > 0 ?
                            <IconButton className={CB_BUTTON} title={t('SETTINGS_SHORTCUT_MENU_INFO')} tabIndex={-1} onMouseDown={onVideosButtonMouseDown} onClick={onToggleSideDrawer}>
                                <ListVideo className={CB_ICON} />
                            </IconButton>
                            :
                            null
                    }
                    <IconButton className={cn(CB_BUTTON, videoScale === null && 'opacity-40')} title={videoScaleLabel} tabIndex={-1} onClick={onVideoScaleChanged}>
                        <Scaling className={CB_ICON} />
                    </IconButton>
                    {
                        fullscreenSupported ?
                            <IconButton className={CB_BUTTON} title={fullscreen ? t('EXIT_FULLSCREEN') : t('ENTER_FULLSCREEN')} tabIndex={-1} onClick={fullscreen ? exitFullscreen : requestFullscreen}>
                                {fullscreen ? <Minimize className={CB_ICON} /> : <Maximize className={CB_ICON} />}
                            </IconButton>
                            :
                            null
                    }
                    <IconButton className={cn(CB_BUTTON, !stream && 'opacity-40')} title={t('MORE')} tabIndex={-1} onMouseDown={onOptionsButtonMouseDown} onClick={onToggleOptionsMenu}>
                        <MoreHorizontal className={CB_ICON} />
                    </IconButton>
                </div>
            </div>
        </div>
    );
});

export default ControlBar;
