// Copyright (C) 2017-2026 Smart code 203358507

/**
 * Subtitles panel: three persistent columns (Languages | Variants | Settings). A
 * fixed-position, state-driven floating <div> (NOT a menu/popover primitive) whose close
 * rides native mousedown bubbling to the Player's onContainerMouseDown; see the researched
 * KEEP note at the menu-layer mount in Player.tsx for why no 2026 primitive fits. Restyled
 * onto Tailwind tokens + the kit Button; every track-shaping useMemo, the embedded-vs-extra
 * routing, and the Stepper/SubtitleVariant wiring are preserved verbatim.
 */

import React, { forwardRef, memo, useCallback, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { AudioLines, Loader2 } from 'lucide-react';
import { languages } from 'rillio/common';
import { SUBTITLES_SIZES, DEFAULT_SUBTITLES_LANGUAGE, LOCAL_SUBTITLES_LANGUAGE } from 'rillio/common/CONSTANTS';
import { Button } from 'rillio/components/ui';
import { cn } from 'rillio/components/ui';
import ShaderBlurRect from '../ShaderBlurRect';
import SnapshotBackdrop from '../SnapshotBackdrop';
import Stepper from './Stepper';
import SubtitleVariant from './SubtitleVariant';
import FineStepper from '../FineStepper';

const ORIGIN_PRIORITIES = ['LOCAL', 'EMBEDDED', 'EXCLUSIVE'];

const normalizeTracksLang = (tracks: any[]) => tracks.map((track) => ({
    ...track,
    lang: languages.toCode(track.lang),
}));

const sortByValues = (items: any[], values: any[]) => items.sort((a, b) => {
    const left = values.indexOf(a);
    const right = values.indexOf(b);
    if (left === -1 && right === -1) return 0;
    if (left === -1) return 1;
    if (right === -1) return -1;
    return left - right;
});

const HEADER = 'flex-none self-stretch px-8 py-6 font-bold text-fg';

const SubtitlesMenu = memo(forwardRef<HTMLDivElement, any>(function SubtitlesMenu(props, ref) {
    const { t } = useTranslation();

    const subtitlesTracks = useMemo(() => {
        return normalizeTracksLang(Array.isArray(props.subtitlesTracks) ? props.subtitlesTracks : []);
    }, [props.subtitlesTracks]);

    const extraSubtitlesTracks = useMemo(() => {
        return normalizeTracksLang(Array.isArray(props.extraSubtitlesTracks) ? props.extraSubtitlesTracks : []);
    }, [props.extraSubtitlesTracks]);

    const allSubtitles = useMemo(() => {
        return subtitlesTracks.concat(extraSubtitlesTracks);
    }, [subtitlesTracks, extraSubtitlesTracks]);

    const subtitlesLanguages = useMemo(() => {
        const userLanguage = languages.toCode(props.subtitlesLanguage) ?? DEFAULT_SUBTITLES_LANGUAGE;
        const interfaceLanguage = languages.toCode(props.interfaceLanguage) ?? DEFAULT_SUBTITLES_LANGUAGE;
        const priorities = [LOCAL_SUBTITLES_LANGUAGE, userLanguage, interfaceLanguage];
        const langs = [...new Set(allSubtitles.map(({ lang }) => lang))].sort((a, b) => a.localeCompare(b));
        return sortByValues(langs, priorities);
    }, [allSubtitles, props.subtitlesLanguage, props.interfaceLanguage]);

    const selectedSubtitlesLanguage = useMemo(() => {
        return typeof props.selectedSubtitlesTrackId === 'string' ?
            subtitlesTracks
                .reduce((selectedSubtitlesLanguage, { id, lang }) => {
                    if (id === props.selectedSubtitlesTrackId) {
                        return lang;
                    }
                    return selectedSubtitlesLanguage;
                }, null)
            :
            typeof props.selectedExtraSubtitlesTrackId === 'string' ?
                extraSubtitlesTracks
                    .reduce((selectedSubtitlesLanguage, { id, lang }) => {
                        if (id === props.selectedExtraSubtitlesTrackId) {
                            return lang;
                        }
                        return selectedSubtitlesLanguage;
                    }, null)
                :
                null;
    }, [subtitlesTracks, extraSubtitlesTracks, props.selectedSubtitlesTrackId, props.selectedExtraSubtitlesTrackId]);
    const subtitlesTracksForLanguage = useMemo(() => {
        const tracks = allSubtitles.filter(({ lang }) => lang === selectedSubtitlesLanguage);
        return sortByValues(tracks, ORIGIN_PRIORITIES);
    }, [allSubtitles, selectedSubtitlesLanguage]);
    const onMouseDown = useCallback((event: React.MouseEvent) => {
        (event.nativeEvent as any).subtitlesMenuClosePrevented = true;
    }, []);
    const subtitlesLanguageOnClick = useCallback((event: React.MouseEvent<HTMLElement>) => {
        const tracks = allSubtitles.filter(({ lang }) => lang === (event.currentTarget as HTMLElement).dataset.lang);
        const track = sortByValues(tracks, ORIGIN_PRIORITIES).shift();

        if (!track) {
            if (typeof props.onSubtitlesTrackSelected === 'function') {
                props.onSubtitlesTrackSelected(null);
            }
            if (typeof props.onExtraSubtitlesTrackSelected === 'function') {
                props.onExtraSubtitlesTrackSelected(null);
            }
        } else if (track.embedded) {
            if (typeof props.onSubtitlesTrackSelected === 'function') {
                props.onSubtitlesTrackSelected(track);
            }
        } else {
            if (typeof props.onExtraSubtitlesTrackSelected === 'function') {
                props.onExtraSubtitlesTrackSelected(track);
            }
        }
    }, [allSubtitles, props.onSubtitlesTrackSelected, props.onExtraSubtitlesTrackSelected]);
    const subtitlesTrackOnSelect = useCallback((track: any) => {
        if (track.embedded) {
            if (typeof props.onSubtitlesTrackSelected === 'function') {
                props.onSubtitlesTrackSelected(track);
            }
        } else {
            if (typeof props.onExtraSubtitlesTrackSelected === 'function') {
                props.onExtraSubtitlesTrackSelected(track);
            }
        }
    }, [props.onSubtitlesTrackSelected, props.onExtraSubtitlesTrackSelected]);
    // The delay in effect for whichever subtitle track is active: embedded
    // tracks are timed by mpv (subtitlesDelay), addon tracks by the web
    // renderer (extraSubtitlesDelay). Before this the control only knew about
    // addon tracks, so embedded subtitles could not be nudged at all.
    const activeSubtitlesDelay: number | null = useMemo(() => {
        if (typeof props.selectedSubtitlesTrackId === 'string') {
            return typeof props.subtitlesDelay === 'number' ? props.subtitlesDelay : null;
        }
        if (typeof props.selectedExtraSubtitlesTrackId === 'string') {
            return typeof props.extraSubtitlesDelay === 'number' ? props.extraSubtitlesDelay : null;
        }
        return null;
    }, [props.selectedSubtitlesTrackId, props.selectedExtraSubtitlesTrackId, props.subtitlesDelay, props.extraSubtitlesDelay]);
    const onSubtitlesDelayChanged = useCallback((value: number) => {
        // Seconds from the controls, ms to the video (one setter drives both
        // renderers, see useVideo.setSubtitlesDelay).
        if (activeSubtitlesDelay !== null && typeof props.onExtraSubtitlesDelayChanged === 'function') {
            props.onExtraSubtitlesDelayChanged(Math.round(value * 1000));
        }
    }, [activeSubtitlesDelay, props.onExtraSubtitlesDelayChanged]);
    const onSubtitlesSizeChanged = useCallback((value: number) => {
        if (typeof props.selectedSubtitlesTrackId === 'string') {
            if (props.subtitlesSize !== null && !isNaN(props.subtitlesSize)) {
                if (typeof props.onSubtitlesSizeChanged === 'function') {
                    props.onSubtitlesSizeChanged(value);
                }
            }
        } else if (typeof props.selectedExtraSubtitlesTrackId === 'string') {
            if (props.extraSubtitlesSize !== null && !isNaN(props.extraSubtitlesSize)) {
                if (typeof props.onExtraSubtitlesSizeChanged === 'function') {
                    props.onExtraSubtitlesSizeChanged(value);
                }
            }
        }
    }, [props.selectedSubtitlesTrackId, props.selectedExtraSubtitlesTrackId, props.subtitlesSize, props.extraSubtitlesSize, props.onSubtitlesSizeChanged, props.onExtraSubtitlesSizeChanged]);
    const onSubtitlesOffsetChanged = useCallback((value: number) => {
        if (typeof props.selectedSubtitlesTrackId === 'string') {
            if (props.subtitlesOffset !== null && !isNaN(props.subtitlesOffset)) {
                if (typeof props.onSubtitlesOffsetChanged === 'function') {
                    props.onSubtitlesOffsetChanged(value);
                }
            }
        } else if (typeof props.selectedExtraSubtitlesTrackId === 'string') {
            if (props.extraSubtitlesOffset !== null && !isNaN(props.extraSubtitlesOffset)) {
                if (typeof props.onExtraSubtitlesOffsetChanged === 'function') {
                    props.onExtraSubtitlesOffsetChanged(value);
                }
            }
        }
    }, [props.selectedSubtitlesTrackId, props.selectedExtraSubtitlesTrackId, props.subtitlesOffset, props.extraSubtitlesOffset, props.onSubtitlesOffsetChanged, props.onExtraSubtitlesOffsetChanged]);
    return (
        <div ref={ref} className={cn('flex h-[25rem] flex-row', props.className)} onMouseDown={onMouseDown}>
            <SnapshotBackdrop />
            <ShaderBlurRect />
            <div className={'flex w-64 flex-none flex-col self-stretch'}>
                <div className={HEADER}>{t('PLAYER_SUBTITLES_LANGUAGES')}</div>
                <div className={'flex-1 self-stretch overflow-y-auto px-4'}>
                    <Button
                        variant={'ghost'}
                        title={t('OFF')}
                        onClick={subtitlesLanguageOnClick}
                        className={cn(
                            'mb-2 flex h-14 w-full flex-row items-center rounded-card px-6 hover:bg-surface-hover',
                            selectedSubtitlesLanguage === null && 'bg-accent-soft',
                        )}
                    >
                        <div className={'flex-1 truncate text-left text-[1.1rem] text-fg'}>{t('OFF')}</div>
                        {selectedSubtitlesLanguage === null ? <div className={'ml-4 size-2 flex-none rounded-full bg-primary'} /> : null}
                    </Button>
                    {subtitlesLanguages.map((lang, index) => (
                        <Button
                            key={index}
                            variant={'ghost'}
                            title={languages.label(lang)}
                            data-lang={lang}
                            onClick={subtitlesLanguageOnClick}
                            className={cn(
                                'mb-2 flex h-14 w-full flex-row items-center rounded-card px-6 hover:bg-surface-hover',
                                selectedSubtitlesLanguage === lang && 'bg-accent-soft',
                            )}
                        >
                            <div className={'flex-1 truncate text-left text-[1.1rem] text-fg'}>
                                {lang === 'local' ? t('LOCAL') : languages.label(lang)}
                            </div>
                            {selectedSubtitlesLanguage === lang ? <div className={'ml-4 size-2 flex-none rounded-full bg-primary'} /> : null}
                        </Button>
                    ))}
                </div>
            </div>
            <div className={'flex w-64 flex-none flex-col self-stretch'}>
                <div className={HEADER}>{t('PLAYER_SUBTITLES_VARIANTS')}</div>
                {
                    subtitlesTracksForLanguage.length > 0 ?
                        <div className={'flex-1 self-stretch overflow-y-auto px-4'}>
                            {subtitlesTracksForLanguage.map((track, index) => (
                                <SubtitleVariant
                                    key={index}
                                    track={track}
                                    selected={props.selectedSubtitlesTrackId === track.id || props.selectedExtraSubtitlesTrackId === track.id}
                                    onSelect={subtitlesTrackOnSelect}
                                />
                            ))}
                        </div>
                        :
                        <div className={'flex-1 self-stretch p-4'}>
                            <div className={'max-h-[4.8em] font-medium text-fg'}>
                                {t('PLAYER_SUBTITLES_DISABLED')}
                            </div>
                        </div>
                }
            </div>
            <div className={'flex w-[17rem] flex-none flex-col self-stretch'}>
                <div className={HEADER}>{t('PLAYER_SUBTITLES_SETTINGS')}</div>
                <div className={'overflow-y-scroll'}>
                    <FineStepper
                        className={'px-6 pb-4'}
                        label={'DELAY'}
                        value={activeSubtitlesDelay !== null ? activeSubtitlesDelay / 1000 : null}
                        unit={'s'}
                        disabled={activeSubtitlesDelay === null}
                        onChange={onSubtitlesDelayChanged}
                        resetValue={0}
                        action={
                            // Auto sync: shell-only (the decoder lives there), and
                            // only for external tracks (embedded ones expose no cues).
                            props.subtitlesAutoSync !== 'unsupported' ?
                                <Button
                                    variant={'ghost'}
                                    size={'sm'}
                                    disabled={props.subtitlesAutoSync !== 'ready'}
                                    title={props.subtitlesAutoSync === 'unavailable' ? t('SUBTITLES_AUTO_SYNC_EXTERNAL_ONLY') : t('SUBTITLES_AUTO_SYNC_HINT')}
                                    onClick={props.onSubtitlesAutoSync}
                                    className={'h-6 gap-1 bg-(--overlay-color) px-2 text-[0.6875rem] text-fg opacity-100 hover:bg-(--overlay-color) hover:brightness-110 [&_svg]:size-3 [&_svg]:text-fg'}
                                >
                                    {props.subtitlesAutoSync === 'running' ? <Loader2 className={'animate-spin'} /> : <AudioLines />}
                                    {t(props.subtitlesAutoSync === 'running' ? 'SUBTITLES_AUTO_SYNC_RUNNING' : 'SUBTITLES_AUTO_SYNC')}
                                </Button>
                                :
                                null
                        }
                    />
                    <Stepper
                        className={'px-6 pb-4'}
                        label={'SIZE'}
                        value={props.selectedSubtitlesTrackId ? props.subtitlesSize : props.selectedExtraSubtitlesTrackId ? props.extraSubtitlesSize : null}
                        unit={'%'}
                        step={25}
                        min={SUBTITLES_SIZES[0]}
                        max={SUBTITLES_SIZES[SUBTITLES_SIZES.length - 1]}
                        disabled={Boolean((props.selectedSubtitlesTrackId && props.subtitlesSize === null) || (props.selectedExtraSubtitlesTrackId && props.extraSubtitlesSize === null))}
                        onChange={onSubtitlesSizeChanged}
                    />
                    <Stepper
                        className={'px-6 pb-4'}
                        label={'PLAYER_SUBTITLES_VERTICAL_POSITION'}
                        value={props.selectedSubtitlesTrackId ? props.subtitlesOffset : props.selectedExtraSubtitlesTrackId ? props.extraSubtitlesOffset : null}
                        unit={'%'}
                        step={1}
                        min={0}
                        max={100}
                        disabled={Boolean((props.selectedSubtitlesTrackId && props.subtitlesOffset === null) || (props.selectedExtraSubtitlesTrackId && props.extraSubtitlesOffset === null))}
                        onChange={onSubtitlesOffsetChanged}
                    />
                </div>
            </div>
        </div>
    );
}));

export default SubtitlesMenu;
