// Copyright (C) 2017-2023 Smart code 203358507

/**
 * Board - the home route. Clean-room rewrite (Phase 3 / Wave B): the shell, hero and
 * row orchestration are authored here in TypeScript on Tailwind utilities over our
 * semantic tokens; it consumes the already-rewritten kit-based MetaRow / MetaItem /
 * ContinueWatchingItem verbatim. Every hook and the windowing contract are reused
 * exactly as before: useBoard's loadBoardRows({start,end}) lazy loading, the hero
 * fed from the first Ready catalog, the boardCatalogsOffset that accounts for the
 * hero + continue-watching rows shifting the visible-child -> catalog index map, the
 * THRESHOLD=5 debounced scroll re-run, and the streaming-server warning gating.
 *
 * The catalog rows themselves (the Loading / Ready / Err tri-state and the per-breakpoint
 * poster trim) are the shared components/CatalogRows, consumed here and by routes/Search.
 * The continue-watching preview row is Board-specific and stays here; it reuses the same
 * ROW_CLASS rhythm and HIDE_POSTER trim from that module.
 */

import React from 'react';
import debounce from 'lodash.debounce';
import useTranslate from 'rillio/common/useTranslate';
import { useStreamingServer, useNotifications, withCoreSuspender, getVisibleChildrenRange, useProfile } from 'rillio/common';
import { ContinueWatchingItem, MainNavBars, MetaRow } from 'rillio/components';
import CatalogRows, { HIDE_POSTER, ROW_CLASS } from 'rillio/components/CatalogRows';
import useBoard from './useBoard';
import useContinueWatchingPreview from './useContinueWatchingPreview';
import StreamingServerWarning from './StreamingServerWarning';
import HeroCarousel from './HeroCarousel';

const HERO_SLIDES = 6;

const THRESHOLD = 5;

// The floating streaming-server warning: absolute inset placement with safe-area
// insets, rebased above the bottom nav rail at the minimum width, and nudged clear of
// the left rail in phone-landscape (was board-warning-container in styles.less).
const BOARD_WARNING =
    'absolute bottom-[calc(var(--safe-area-inset-bottom)+0.5rem)] left-[calc(var(--safe-area-inset-left)+0.5rem)] right-[calc(var(--safe-area-inset-right)+0.5rem)] ' +
    'max-[640px]:bottom-[calc(var(--vertical-nav-bar-size)+0.5rem)] max-[640px]:h-28 ' +
    '[@media(orientation:landscape)and(max-width:1000px)and(max-height:500px)]:left-[calc(var(--safe-area-inset-left)+var(--vertical-nav-bar-size)+0.5rem)]';

const cx = (...parts: (string | false | undefined)[]) => parts.filter(Boolean).join(' ');

const Board = () => {
    const t = useTranslate();
    const streamingServer = useStreamingServer();
    const continueWatchingPreview = useContinueWatchingPreview();
    const [board, loadBoardRows] = useBoard();
    const notifications = useNotifications();
    const profile = useProfile();
    const scrollContainerRef = React.useRef<HTMLDivElement>(null);
    const showStreamingServerWarning = React.useMemo(() => {
        return streamingServer.settings !== null && streamingServer.settings.type === 'Err' && (
            isNaN(profile.settings.streamingServerWarningDismissed.getTime()) ||
            profile.settings.streamingServerWarningDismissed.getTime() < Date.now());
    }, [profile.settings, streamingServer.settings]);
    // Feed the hero from catalogs that have already loaded, so it costs no
    // extra fetch - and from BOTH content types: the first Ready catalog of
    // each distinct item type (in board order that is Popular Movies + Popular
    // Series, both on screen at boot) contributes, interleaved movie/series/
    // movie/... A movies-only hero was never a decision, just an artifact of
    // taking whichever single catalog loaded first. Only items with a backdrop
    // qualify.
    const heroItems = React.useMemo(() => {
        const pools: any[][] = [];
        const seenTypes = new Set<string>();
        for (const catalog of board.catalogs as any[]) {
            if (catalog.content?.type !== 'Ready' || !Array.isArray(catalog.content.content)) continue;
            const items = catalog.content.content
                .filter((item: any) => typeof item.background === 'string' && item.background.length > 0);
            const type = items[0]?.type;
            if (typeof type !== 'string' || seenTypes.has(type)) continue;
            seenTypes.add(type);
            pools.push(items);
            if (pools.length === 2) break;
        }
        const out: any[] = [];
        const seenIds = new Set<string>();
        for (let i = 0; out.length < HERO_SLIDES; i++) {
            let anyLeft = false;
            for (const pool of pools) {
                const item = pool[i];
                if (!item) continue;
                anyLeft = true;
                if (!seenIds.has(item.id) && out.length < HERO_SLIDES) {
                    seenIds.add(item.id);
                    out.push(item);
                }
            }
            if (!anyLeft) break;
        }
        return out;
    }, [board.catalogs]);
    // The hero and the continue-watching row precede the catalog rows inside the
    // scroll container, so they shift the visible-child -> catalog index mapping.
    const boardCatalogsOffset = (continueWatchingPreview.items.length > 0 ? 1 : 0) + (heroItems.length > 0 ? 1 : 0);
    const onVisibleRangeChange = React.useCallback(() => {
        const range = getVisibleChildrenRange(scrollContainerRef.current);
        if (range === null) {
            return;
        }

        const start = Math.max(0, range.start - boardCatalogsOffset - THRESHOLD);
        const end = range.end - boardCatalogsOffset + THRESHOLD;
        if (end < start) {
            return;
        }

        loadBoardRows({ start, end });
    }, [boardCatalogsOffset]);
    const onScroll = React.useCallback(debounce(onVisibleRangeChange, 250), [onVisibleRangeChange]);
    React.useLayoutEffect(() => {
        onVisibleRangeChange();
    }, [board.catalogs, onVisibleRangeChange]);
    return (
        <div className="flex h-[calc(100%-var(--safe-area-inset-bottom))] w-full flex-col max-[640px]:relative max-[640px]:z-0">
            {/* The nav scrim stays ON whenever the board is full-bleed (Michael's
                call): the nav always sits over art here, scrolled or not. */}
            <MainNavBars
                className="flex-1 self-stretch bg-transparent"
                route={'board'}
                fullBleed={heroItems.length > 0}
                navScrim={heroItems.length > 0}
            >
                <div ref={scrollContainerRef} className="h-full w-full overflow-y-auto px-4" onScroll={onScroll}>
                    {
                        // Full-bleed: the hero cancels the container's side padding
                        // and starts at the window top, under the floating nav.
                        heroItems.length > 0 ?
                            <HeroCarousel className={cx('-mx-4 mb-8 w-[calc(100%+2rem)]', 'animation-fade-in')} items={heroItems} />
                            :
                            null
                    }
                    {
                        continueWatchingPreview.items.length > 0 ?
                            <MetaRow
                                className={cx(ROW_CLASS, HIDE_POSTER)}
                                title={t.string('BOARD_CONTINUE_WATCHING')}
                                catalog={continueWatchingPreview}
                                itemComponent={ContinueWatchingItem}
                                notifications={notifications}
                            />
                            :
                            null
                    }
                    <CatalogRows catalogs={board.catalogs} />
                </div>
            </MainNavBars>
            {
                showStreamingServerWarning ?
                    <StreamingServerWarning className={BOARD_WARNING} />
                    :
                    null
            }
        </div>
    );
};

const BoardFallback = () => (
    <div className="flex h-[calc(100%-var(--safe-area-inset-bottom))] w-full flex-col">
        <MainNavBars className="flex-1 self-stretch bg-transparent" route={'board'} />
    </div>
);

export default withCoreSuspender(Board, BoardFallback);
