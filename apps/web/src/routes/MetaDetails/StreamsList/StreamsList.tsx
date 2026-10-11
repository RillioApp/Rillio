// Copyright (C) 2017-2023 Smart code 203358507

/**
 * StreamsList (Phase 3 clean-room rewrite).
 *
 * The stream panel container + state machine, view-layer rebuilt on Tailwind tokens
 * and the kit Select (addon filter) / Button. All logic is reused verbatim:
 * streamsByAddon / filteredStreams / selectableOptions memos, countLoadingAddons,
 * showInstallAddonsButton (profile-gated), backButtonOnClick (toPath deep link),
 * the addon-change scroll-to-top, and the episode-picker handoff. CuratedStreams is
 * the primary streams view; the legacy per-stream Popup row was retired (only its
 * placeholder was ever rendered here, now a local StreamPlaceholder).
 */

import React from 'react';
import { useNavigate } from 'react-router';
import { useTranslation } from 'react-i18next';
import { ChevronLeft, Puzzle } from 'lucide-react';
import { Image } from 'rillio/components';
import { usePlatform, useProfile } from 'rillio/common';
import { openModal } from 'rillio/common/modalEvents';
import { Button } from 'rillio/components/ui/button';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from 'rillio/components/ui/select';
import { cn } from 'rillio/components/ui/cn';
import CuratedStreams from './CuratedStreams';
import CachedStreams from './CachedStreams';
import useCachedStreams from './useCachedStreams';
import StreamPlaceholder from './StreamPlaceholder';
import SeasonEpisodePicker from '../EpisodePicker';

const toPath = require('rillio-router/toPath').default;

const ALL_ADDONS_KEY = 'ALL';

const emptyImage = require('/assets/images/empty.svg');

type Props = {
    className?: string;
    streams: any[];
    video?: any;
    type?: string;
    // Identity of what is on screen, so already-downloaded copies of THIS title
    // can be surfaced above the addon streams (see CachedStreams).
    metaId?: string | null;
    videoId?: string | null;
    libraryItem?: any;
    onEpisodeSearch?: (season: number, episode: number) => void;
};

const StreamsList = ({ className, video, type, metaId, videoId, libraryItem, onEpisodeSearch, ...props }: Props) => {
    const { t } = useTranslation();
    const platform = usePlatform();
    const profile = useProfile();
    const navigate = useNavigate();
    const streamsContainerRef = React.useRef<HTMLDivElement>(null);
    const [selectedAddon, setSelectedAddon] = React.useState(ALL_ADDONS_KEY);
    const onAddonSelected = React.useCallback((value: string) => {
        streamsContainerRef.current?.scrollTo({ top: 0, left: 0, behavior: platform.name === 'ios' ? 'smooth' : 'instant' });
        setSelectedAddon(value);
    }, [platform]);
    const showInstallAddonsButton = React.useMemo(() => {
        return !profile || profile.auth === null || profile.auth?.user?.isNewUser === true && !video?.upcoming;
    }, [profile, video]);
    const backButtonOnClick = React.useCallback(() => {
        if (video.deepLinks && typeof video.deepLinks.metaDetailsVideos === 'string') {
            const navigateTo = `${video.deepLinks.metaDetailsVideos}${
                typeof video.season === 'number'
                    ? `?${new URLSearchParams({ 'season': video.season })}`
                    : ''}`;
            navigate(toPath(navigateTo), { replace: true });
        } else {
            navigate(-1);
        }
    }, [video]);
    const countLoadingAddons = React.useMemo(() => {
        return props.streams.filter((stream) => stream.content.type === 'Loading').length;
    }, [props.streams]);
    const streamsByAddon = React.useMemo(() => {
        return props.streams
            .filter((streams) => streams.content.type === 'Ready')
            .reduce((streamsByAddon, streams) => {
                streamsByAddon[streams.addon.transportUrl] = {
                    addon: streams.addon,
                    streams: streams.content.content.map((stream) => ({
                        ...stream,
                        addonName: streams.addon.manifest.name
                    }))
                };

                return streamsByAddon;
            }, {});
    }, [props.streams]);
    const filteredStreams = React.useMemo(() => {
        return selectedAddon === ALL_ADDONS_KEY ?
            Object.values(streamsByAddon).map(({ streams }: any) => streams).flat(1)
            :
            streamsByAddon[selectedAddon] ?
                streamsByAddon[selectedAddon].streams
                :
                [];
    }, [streamsByAddon, selectedAddon]);
    const addonOptions = React.useMemo(() => {
        return [
            { value: ALL_ADDONS_KEY, label: t('ALL_ADDONS') },
            ...Object.keys(streamsByAddon).map((transportUrl) => ({
                value: transportUrl,
                label: streamsByAddon[transportUrl].addon.manifest.name,
            }))
        ];
    }, [streamsByAddon]);

    const handleEpisodePicker = React.useCallback((season: number, episode: number) => {
        onEpisodeSearch?.(season, episode);
    }, [onEpisodeSearch]);

    // Copies of this title already on disk. Rendered OUTSIDE the addon-state
    // branches below: whether addons are loading, empty or erroring has nothing
    // to do with whether the file is already here, and "no streams" while the
    // movie sits fully downloaded was the worst version of that.
    const cachedStreams = useCachedStreams(metaId ?? null, videoId ?? null);
    // Whether the curated carousel will render (it is the preferred home for
    // the on-device tiles; see the fallback below).
    const hasCuratedStreams = filteredStreams.length > 0;

    return (
        <div className={cn('flex flex-col', className)}>
            <div className="z-[2] mx-4 mt-4 flex items-center gap-x-2">
                {
                    video ?
                        <React.Fragment>
                            <Button
                                variant="ghost"
                                tabIndex={0}
                                onClick={backButtonOnClick}
                                className="size-11 flex-none p-0 text-fg-muted hover:text-fg"
                                title={t('BACK')}
                            >
                                <ChevronLeft className="size-6" />
                            </Button>
                            <div className="min-w-[45%] overflow-hidden text-ellipsis whitespace-nowrap text-fg">
                                {typeof video.season === 'number' && typeof video.episode === 'number'
                                    ? `S${video.season}E${video.episode}${video.title ? ` ${video.title}` : ''}`
                                    : (video.title ?? '')}
                            </div>
                        </React.Fragment>
                        :
                        null
                }
                {
                    Object.keys(streamsByAddon).length > 1 ?
                        <Select value={selectedAddon} onValueChange={onAddonSelected}>
                            <SelectTrigger className="ml-auto h-8 flex-none bg-white/5 px-3 text-xs text-fg-muted hover:bg-surface-hover">
                                <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                                {addonOptions.map((option) => (
                                    <SelectItem key={option.value} value={option.value}>{option.label}</SelectItem>
                                ))}
                            </SelectContent>
                        </Select>
                        :
                        null
                }
            </div>
            {
                // Fallback placement: with no curated carousel to sit in (no
                // addons, or every one of them errored) the on-device copy still
                // has to be reachable - that case is exactly when it matters most.
                cachedStreams.length > 0 && !hasCuratedStreams ?
                    <div className="flex flex-wrap justify-center gap-1 px-4 pt-2">
                        <CachedStreams streams={cachedStreams} libraryItem={libraryItem} videoId={videoId ?? null} />
                    </div>
                    :
                    null
            }
            {
                props.streams.length === 0 ?
                    <div className="flex flex-col items-center self-stretch overflow-y-auto p-4">
                        {
                            type === 'series' ?
                                <SeasonEpisodePicker className="flex-none" onSubmit={handleEpisodePicker} />
                                : null
                        }
                        <Image className="mb-4 h-40 w-40 max-w-full flex-none object-contain object-center opacity-90" src={emptyImage} alt={' '} />
                        <div className="mb-8 flex-none text-center text-[1.4rem] text-fg">{t('ERR_NO_ADDONS_FOR_STREAMS')}</div>
                    </div>
                    :
                    props.streams.every((streams) => streams.content.type === 'Err') ?
                        <div className="flex flex-col items-center self-stretch overflow-y-auto p-4">
                            {
                                type === 'series' ?
                                    <SeasonEpisodePicker className="flex-none" onSubmit={handleEpisodePicker} />
                                    : null
                            }
                            {
                                video?.upcoming ?
                                    <div className="mb-8 flex-none text-center text-[1.4rem] text-fg">{t('UPCOMING')}...</div>
                                    : null
                            }
                            <Image className="mb-4 h-40 w-40 max-w-full flex-none object-contain object-center opacity-90" src={emptyImage} alt={' '} />
                            <div className="mb-8 flex-none text-center text-[1.4rem] text-fg">{t('NO_STREAM')}</div>
                            {
                                showInstallAddonsButton ?
                                    <Button
                                        onClick={() => openModal('addons')}
                                        title={t('ADDON_CATALOGUE_MORE')}
                                        className="mx-auto my-4 h-16 max-w-[50%] gap-4 px-8 text-base font-bold"
                                    >
                                        <Puzzle className="size-8" />
                                        <span className="max-h-[3.6em] text-center">{t('ADDON_CATALOGUE_MORE')}</span>
                                    </Button>
                                    :
                                    null
                            }
                        </div>
                        :
                        filteredStreams.length === 0 ?
                            <div ref={streamsContainerRef} className="mt-4 flex flex-1 flex-col self-stretch overflow-y-auto">
                                <StreamPlaceholder />
                                <StreamPlaceholder />
                            </div>
                            :
                            <React.Fragment>
                                <div className="mt-4 flex flex-1 flex-col self-stretch overflow-y-auto" ref={streamsContainerRef}>
                                    <CuratedStreams
                                        streams={filteredStreams}
                                        leading={
                                            <CachedStreams
                                                streams={cachedStreams}
                                                libraryItem={libraryItem}
                                                videoId={videoId ?? null}
                                            />
                                        }
                                    />
                                    {
                                        // Streams exist, so addons are already installed: a quiet
                                        // little link, not the yellow call-to-action.
                                        showInstallAddonsButton ?
                                            <Button variant="ghost" className={'mx-auto mt-1 h-7 gap-1.5 bg-white/5 px-3 text-xs font-medium text-fg-muted hover:bg-white/10 hover:text-fg'} title={t('ADDON_CATALOGUE_MORE')} onClick={() => openModal('addons')}>
                                                <Puzzle className={'size-3.5'} />
                                                {t('ADDON_CATALOGUE_MORE')}
                                            </Button>
                                            :
                                            null
                                    }
                                </div>
                                {
                                    countLoadingAddons > 0 ?
                                        <div className="z-[1] m-8 flex flex-col items-center justify-center gap-4">
                                            <div className="text-base text-fg">
                                                {countLoadingAddons} {t('MOBILE_ADDONS_LOADING')}
                                            </div>
                                            <span className="h-[0.3em] w-[90%] rounded bg-accent" />
                                        </div>
                                        :
                                        null
                                }
                            </React.Fragment>
            }
        </div>
    );
};

export default StreamsList;
