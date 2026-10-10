// Copyright (C) 2017-2023 Smart code 203358507

/**
 * MetaDetails route shell (Phase 3 clean-room rewrite).
 *
 * View layer rebuilt on Tailwind semantic tokens; every hook / core.transport
 * dispatch is reused verbatim (useMetaDetails / useSeason / useMetaExtensionTabs,
 * AddToLibrary / RemoveFromLibrary / MarkAsWatched / ToggleLibraryItemNotifications,
 * useContentGamepadNavigation, useNavigateWithOrigin). Layout mirrors the old
 * styles.less: a fixed backdrop image layer with a gradient scrim, a HorizontalNavBar,
 * an optional VerticalNavBar for meta-extension tabs, and a single scrolling
 * main-column holding the 50vh [details | hero] band then the full-width
 * StreamsList / VideosList. The meta-extension addon opens in the shared ModalDialog.
 */

import React from 'react';
import { useParams, useLocation, useNavigate } from 'react-router';
import { useTranslation } from 'react-i18next';
import { useCore } from 'rillio/core';
import { useContentGamepadNavigation } from 'rillio/services/GamepadNavigation';
import { withCoreSuspender } from 'rillio/common';
import { useNavigateWithOrigin } from 'rillio-router';
import { VerticalNavBar, HorizontalNavBar, DelayedRenderer, EmptyState, Image, MetaPreview, ModalDialog } from 'rillio/components';
import StreamsList from './StreamsList';
import VideosList from './VideosList';
import HeroMedia from './HeroMedia';
import SimilarRow from './SimilarRow';
import EpisodeArrows from './EpisodeArrows';
import useMetaDetails from './useMetaDetails';
import useSeason from './useSeason';
import useMetaExtensionTabs from './useMetaExtensionTabs';

const GAMEPAD_HANDLER_ID = 'metadetails';

// The meta-details message states (no meta selected / no addons / not found) all
// share this centered empty-illustration block (the shared EmptyState primitive).
const MetaMessage = ({ label }: { label: string }) => (
    <EmptyState
        className="flex-1 justify-center self-stretch px-8 py-16"
        imageClassName="mb-4 max-w-full"
        labelClassName="flex-none self-stretch text-[2rem] text-fg"
        label={label}
    />
);

const MetaDetails = () => {
    const { type, id, videoId } = useParams();
    const location = useLocation();
    const navigate = useNavigate();
    const { getStoredOrigin } = useNavigateWithOrigin();
    const contentRef = React.useRef<HTMLDivElement>(null);
    const { t } = useTranslation();
    const core = useCore();
    const urlParams = React.useMemo(() => ({
        type,
        id,
        videoId
    }), [type, id, videoId]);
    const metaDetails = useMetaDetails(urlParams);
    const [season, setSeason] = useSeason(urlParams);
    const [tabs, metaExtension, clearMetaExtension] = useMetaExtensionTabs(metaDetails.metaExtensions);
    const [metaPath, streamPath] = React.useMemo(() => {
        return metaDetails.selected !== null ?
            [metaDetails.selected.metaPath, metaDetails.selected.streamPath]
            :
            [null, null];
    }, [metaDetails.selected]);
    const video = React.useMemo(() => {
        return streamPath !== null && metaDetails.metaItem !== null && metaDetails.metaItem.content.type === 'Ready' ?
            metaDetails.metaItem.content.content.videos.reduce((result, video) => {
                if (video.id === streamPath.id) {
                    return video;
                }

                return result;
            }, null)
            :
            null;
    }, [metaDetails.metaItem, streamPath]);
    const episodes = React.useMemo(() => {
        return metaDetails.metaItem !== null && metaDetails.metaItem.content.type === 'Ready' ?
            metaDetails.metaItem.content.content.videos
            :
            [];
    }, [metaDetails.metaItem]);
    const addToLibrary = React.useCallback(() => {
        if (metaDetails.metaItem === null || metaDetails.metaItem.content.type !== 'Ready') {
            return;
        }

        core.transport.dispatch({
            action: 'Ctx',
            args: {
                action: 'AddToLibrary',
                args: metaDetails.metaItem.content.content
            }
        });
    }, [metaDetails]);
    const removeFromLibrary = React.useCallback(() => {
        if (metaDetails.metaItem === null || metaDetails.metaItem.content.type !== 'Ready') {
            return;
        }

        core.transport.dispatch({
            action: 'Ctx',
            args: {
                action: 'RemoveFromLibrary',
                args: metaDetails.metaItem.content.content.id
            }
        });
    }, [metaDetails]);
    const toggleWatched = React.useCallback(() => {
        if (metaDetails.metaItem === null || metaDetails.metaItem.content.type !== 'Ready') {
            return;
        }

        core.transport.dispatch({
            action: 'MetaDetails',
            args: {
                action: 'MarkAsWatched',
                args: !metaDetails.metaItem.content.content.watched
            }
        });
    }, [metaDetails]);
    const toggleNotifications = React.useCallback(() => {
        if (metaDetails.libraryItem) {
            core.transport.dispatch({
                action: 'Ctx',
                args: {
                    action: 'ToggleLibraryItemNotifications',
                    args: [metaDetails.libraryItem._id, !metaDetails.libraryItem.state.noNotif],
                }
            });
        }
    }, [metaDetails.libraryItem]);
    const seasonOnSelect = React.useCallback((event) => {
        setSeason(event.value);
    }, [setSeason]);
    const handleEpisodeSearch = React.useCallback((season, episode) => {
        const searchVideoHash = encodeURIComponent(`${urlParams.id}:${season}:${episode}`);
        const url = location.pathname;
        const searchVideoPath = (urlParams.videoId === undefined || urlParams.videoId === null || urlParams.videoId === '') ?
            url + (!url.endsWith('/') ? '/' : '') + searchVideoHash
            : url.replace(encodeURIComponent(urlParams.videoId), searchVideoHash);
        navigate(searchVideoPath, { replace: true });
    }, [urlParams, location]);

    const renderBackgroundImageFallback = React.useCallback(() => null, []);
    const renderBackground = React.useMemo(() => !!(
        metaPath &&
        metaDetails?.metaItem &&
        metaDetails.metaItem.content.type !== 'Loading' &&
        typeof metaDetails.metaItem.content.content?.background === 'string' &&
        metaDetails.metaItem.content.content.background.length > 0
    ), [metaPath, metaDetails]);
    const originPath = React.useMemo(() => getStoredOrigin(), [getStoredOrigin]);
    const trailerYtIds = React.useMemo(() => {
        const ts = metaDetails.metaItem?.content?.content?.trailerStreams;
        return Array.isArray(ts) ?
            ts.map((t: { ytId?: string }) => t.ytId).filter((id: unknown) => typeof id === 'string' && id.length > 0)
            :
            [];
    }, [metaDetails.metaItem]);

    useContentGamepadNavigation(contentRef, GAMEPAD_HANDLER_ID);
    return (
        <div
            className="relative box-border flex h-full w-full flex-col"
            style={{ paddingLeft: 'var(--safe-area-inset-left)', paddingRight: 'var(--safe-area-inset-right)' }}
        >
            {
                renderBackground ?
                    <div className="fixed inset-0 z-[-1] bg-bg">
                        <Image
                            className="pointer-events-none block h-full w-full object-cover object-[center_top] opacity-[0.16] max-sm:object-center"
                            src={metaDetails.metaItem.content.content.background}
                            renderFallback={renderBackgroundImageFallback}
                            alt={' '}
                        />
                        <div className="absolute inset-0 bg-[linear-gradient(to_bottom,color-mix(in_srgb,var(--color-bg)_40%,transparent)_0%,color-mix(in_srgb,var(--color-bg)_88%,transparent)_50%,var(--color-bg)_100%)]" />
                    </div>
                    :
                    null
            }
            <HorizontalNavBar
                className="z-[1] flex-none self-stretch"
                backButton={true}
                fullscreenButton={true}
                navMenu={true}
                originPath={originPath}
            />
            <div ref={contentRef} className="z-0 flex min-h-0 flex-1 flex-row self-stretch">
                {
                    tabs.length > 0 ?
                        <VerticalNavBar
                            className="flex-none"
                            tabs={tabs}
                            selected={metaExtension !== null ? metaExtension.url : null}
                        />
                        :
                        null
                }
                <div className="flex min-w-0 flex-1 flex-col self-stretch overflow-y-auto px-8 pb-10 pt-2 max-sm:px-4 max-sm:pb-6">
                    {/* Single column, except the meta band: on screens wide enough
                        the trailer/cover carousel sits to the RIGHT of the
                        title/description group, vertically centered against it;
                        below 75rem it disappears entirely (no stacked fallback -
                        Michael's calls, 2026-08-16). Streams/episodes and Similar
                        flow full-width beneath. */}
                    {/* Everything above Similar is a full-viewport hero: the block
                        (meta pair + streams/episodes) centers vertically in the
                        first screenful; Similar lives below the fold. */}
                    <div className="flex min-h-full min-w-0 flex-none flex-col justify-center self-stretch">
                        {/* The meta group + trailer travel as a CENTERED pair once
                            the screen outgrows them (an ultrawide left-anchored
                            pair read as lopsided - Michael, 2026-08-16). */}
                        <div className="flex flex-row items-center justify-center gap-10 self-stretch">
                            <div className="flex w-full min-w-0 max-w-[56rem] shrink-0 flex-col">
                                {
                                    metaPath === null ?
                                        <DelayedRenderer delay={500}>
                                            <MetaMessage label={t('ERR_NO_META_SELECTED')} />
                                        </DelayedRenderer>
                                        :
                                        metaDetails.metaItem === null ?
                                            <MetaMessage label={t('ERR_NO_ADDONS_FOR_META')} />
                                            :
                                            metaDetails.metaItem.content.type === 'Err' ?
                                                <MetaMessage label={t('ERR_NO_META_FOUND')} />
                                                :
                                                metaDetails.metaItem.content.type === 'Loading' ?
                                                    <MetaPreview.Placeholder className="min-h-[22rem] flex-none self-stretch" />
                                                    :
                                                    <MetaPreview
                                                        className="max-w-[52rem] flex-none self-stretch duration-300 animate-in fade-in"
                                                        name={metaDetails.metaItem.content.content.name}
                                                        logo={metaDetails.metaItem.content.content.logo}
                                                        runtime={metaDetails.metaItem.content.content.runtime}
                                                        releaseInfo={metaDetails.metaItem.content.content.releaseInfo}
                                                        released={metaDetails.metaItem.content.content.released}
                                                        description={
                                                            video !== null && typeof video.overview === 'string' && video.overview.length > 0 ?
                                                                video.overview
                                                                :
                                                                metaDetails.metaItem.content.content.description
                                                        }
                                                        links={metaDetails.metaItem.content.content.links}
                                                        inLibrary={metaDetails.metaItem.content.content.inLibrary}
                                                        toggleInLibrary={metaDetails.metaItem.content.content.inLibrary ? removeFromLibrary : addToLibrary}
                                                        watched={metaDetails.metaItem.content.content.watched}
                                                        toggleWatched={toggleWatched}
                                                        ratingInfo={metaDetails.ratingInfo}
                                                    />
                                }
                            </div>
                            {
                                // The cap keeps it a card, not a cinema.
                                metaPath !== null && metaDetails.metaItem !== null && metaDetails.metaItem.content.type === 'Ready' ?
                                    <div className="flex w-full min-w-0 max-w-[52rem] items-center self-center max-[75rem]:hidden">
                                        <HeroMedia
                                            className="aspect-video w-full max-w-[52rem] flex-none overflow-hidden rounded-xl duration-300 animate-in fade-in"
                                            ytIds={trailerYtIds}
                                            background={metaDetails.metaItem.content.content.background}
                                            poster={metaDetails.metaItem.content.content.poster}
                                            name={metaDetails.metaItem.content.content.name}
                                        />
                                    </div>
                                    :
                                    null
                            }
                        </div>
                        {
                            // EpisodeArrows: previous/next episode out at the
                            // side edges (series only: renders the list
                            // untouched for a movie or channel).
                            streamPath !== null ?
                                <EpisodeArrows
                                    className="mt-8 flex-none self-stretch"
                                    type={streamPath.type}
                                    videos={episodes}
                                    videoId={streamPath.id}
                                >
                                    <StreamsList
                                        className="w-full max-w-[80rem] flex-none self-center"
                                        streams={metaDetails.streams}
                                        video={video}
                                        type={streamPath.type}
                                        metaId={metaPath?.id ?? streamPath.id}
                                        videoId={streamPath.id}
                                        libraryItem={metaDetails.libraryItem}
                                        onEpisodeSearch={handleEpisodeSearch}
                                    />
                                </EpisodeArrows>
                                :
                                null
                        }
                        {
                            // The episode browser renders on the videos route
                            // ONLY: once a stream is selected the streams list
                            // owns the pane (both at once read as two competing
                            // lists - Michael's call, 2026-08-15).
                            streamPath === null && metaPath !== null ?
                                <VideosList
                                    className="mt-8 w-full max-w-[80rem] flex-none self-center"
                                    metaItem={metaDetails.metaItem}
                                    libraryItem={metaDetails.libraryItem}
                                    season={season}
                                    selectedVideoId={metaDetails.libraryItem?.state?.video_id}
                                    seasonOnSelect={seasonOnSelect}
                                    toggleNotifications={toggleNotifications}
                                />
                                :
                                null
                        }
                    </div>
                    {
                        // BOTTOM pane: "Similar", full-width under both columns.
                        // Keyless, from the installed addon catalogs; renders
                        // nothing at all when it has nothing.
                        metaPath !== null ?
                            <SimilarRow className="mt-6" metaItem={metaDetails.metaItem} />
                            :
                            null
                    }
                </div>
            </div>
            {
                metaExtension !== null ?
                    <ModalDialog
                        title={metaExtension.name}
                        onCloseRequest={clearMetaExtension}>
                        <iframe
                            className="block h-[70vh] w-[75vw] max-w-full rounded-card border-0"
                            sandbox={'allow-forms allow-scripts allow-same-origin'}
                            src={metaExtension.url}
                        />
                    </ModalDialog>
                    :
                    null
            }
        </div>
    );
};

// The details column: flex 0 0 clamp(20rem,40%,38rem), full-height in the band;
// stacks full-width below the 60rem breakpoint.
const MetaDetailsFallback = () => (
    <div
        className="relative box-border flex h-full w-full flex-col"
        style={{ paddingLeft: 'var(--safe-area-inset-left)', paddingRight: 'var(--safe-area-inset-right)' }}
    >
        <HorizontalNavBar
            className="z-[1] flex-none self-stretch"
            backButton={true}
            fullscreenButton={true}
            navMenu={true}
        />
    </div>
);

export default withCoreSuspender(MetaDetails, MetaDetailsFallback);
