// Copyright (C) 2017-2026 Smart code 203358507

/**
 * EpisodeArrows - previous/next episode navigation for the streams view.
 *
 * Wraps the streams region and owns its centred 80rem column. From `sm`
 * up the two arrows sit INSIDE that column, at its far left and far right,
 * vertically centred on the streams, each in its own lane: the column
 * insets the header, controls and cards by the arrow plus a gap, so the
 * arrows never cover a card and read as page navigation rather than a
 * carousel control of the tiles. The arrows are bare chevrons with no fill
 * or rule beside them. Below `sm` two lanes would squeeze the
 * cards, so the arrows become two small labelled pills ("S1E1", "S1E3")
 * in a row above the streams header instead.
 *
 * The neighbours come from adjacentEpisodes (season-then-episode order,
 * specials only from a special); the click follows the target video's own
 * metaDetailsStreams deep link, exactly as picking it from the episode list
 * does. An end of the list shows no arrow at all; anything but a series
 * (a movie, a channel, even one whose videos carry numbers) or a series
 * without an episode list renders the children untouched.
 *
 * Keyboard: Left/Right step too, but only while nothing on the page holds
 * focus (or one of these arrows does). Everything on this page that already
 * binds the arrow keys (the preset toggle's roving focus, the trailer
 * carousel, the language search, the addon select, the tile row's scroll,
 * the all-streams modal) only does so while focused, so a body-focused key
 * press cannot be meant for any of them.
 */

import React from 'react';
import { useNavigate } from 'react-router';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { Button } from 'rillio/components/ui/button';
import { cn } from 'rillio/components/ui/cn';

const { adjacentEpisodes } = require('./adjacentEpisodes');
const { episodeArrowKey } = require('./episodeArrowKey');
const { default: toPath } = require('rillio-router/toPath');

type Target = {
    id: string;
    season: number;
    episode: number;
    title: string;
    label: string;
    link: string | null;
};

type Props = {
    className?: string;
    // The media type ('series' is the only one with arrows), the title's
    // episode list (meta.videos) and the video on screen.
    type: string;
    videos: any[];
    videoId: string;
    children: React.ReactNode;
};

const navigable = (target: Target | null): target is Target & { link: string } =>
    target !== null && typeof target.link === 'string';

// Sizes are in rem and the app's root font is 14px: h-11 is ~38px, size-13
// ~45px.
const ARROW_CLASS = cn(
    // focus-visible:outline-solid: the kit Button's own focus outline never
    // shows (its base `outline-none` sets --tw-outline-style: none, which
    // its focus-visible:outline / outline-2 then read, and tailwind-merge
    // drops focus-visible:outline as well), so with no fill left the arrow
    // restores the outline style itself. The kit bug is reported separately.
    'text-fg-muted hover:text-fg focus-visible:outline-solid',
    // Phone: a small labelled pill in the row above the streams.
    'h-11 gap-1 bg-surface px-3.5 text-xs font-medium hover:bg-surface-hover',
    // sm and up: a bare chevron in its lane at the column's edge. No fill at
    // rest or on hover (the glyph's muted -> full foreground is the hover);
    // the transparent 3.25rem square stays the hit area (45px+ at every root
    // size) and carries the kit's focus outline.
    'sm:pointer-events-auto sm:absolute sm:top-1/2 sm:size-13 sm:-translate-y-1/2 sm:bg-transparent sm:p-0 sm:hover:bg-transparent',
);

const EpisodeArrows = ({ className, type, videos, videoId, children }: Props) => {
    const navigate = useNavigate();
    const navRef = React.useRef<HTMLElement>(null);
    const { previous, next } = React.useMemo(() => adjacentEpisodes(type, videos, videoId), [type, videos, videoId]);
    const previousTarget = navigable(previous) ? previous : null;
    const nextTarget = navigable(next) ? next : null;
    const hasArrows = previousTarget !== null || nextTarget !== null;

    // replace, like the streams view's own back button: stepping through
    // episodes should not stack one history entry per episode.
    const go = React.useCallback((target: { link: string }) => {
        navigate(toPath(target.link), { replace: true });
    }, [navigate]);

    React.useEffect(() => {
        if (!hasArrows) {
            return;
        }
        const onKeyDown = (event: KeyboardEvent) => {
            const active = document.activeElement;
            const onArrow = active instanceof HTMLElement && navRef.current !== null && navRef.current.contains(active);
            const step = episodeArrowKey(event, {
                focusFree: active === null || active === document.body || onArrow,
                dialogOpen: document.querySelector('[role="dialog"]') !== null,
            });
            if (step === null) return;
            const target = step === 'previous' ? previousTarget : nextTarget;
            if (target === null) return;
            event.preventDefault();
            go(target);
        };
        document.addEventListener('keydown', onKeyDown);
        return () => document.removeEventListener('keydown', onKeyDown);
    }, [hasArrows, previousTarget, nextTarget, go]);

    // The centred 80rem streams column. From sm up, with arrows, it insets its
    // content by a 5rem lane each side (px-20) = the arrow (size-13, 3.25rem)
    // + a 1.75rem gap to the header and cards; the arrows sit at left-0 /
    // right-0 inside the column's padding box, so nothing ever sits under
    // them and nothing sticks out of a box (the global reset's
    // overflow:hidden has nothing to clip). Change the arrow size and the
    // lane together. Both lanes stay reserved at the first/last episode so
    // the content never shifts sideways between episodes.
    return (
        <div className={cn('relative flex w-full max-w-[80rem] flex-col self-center', hasArrows && 'sm:px-20', className)}>
            {
                hasArrows ?
                    <nav
                        ref={navRef}
                        aria-label="Episodes"
                        className="mx-2 flex items-center sm:pointer-events-none sm:absolute sm:inset-0 sm:mx-0"
                    >
                        {
                            previousTarget !== null ?
                                <Button
                                    variant="ghost"
                                    aria-label="Previous episode"
                                    title={previousTarget.label}
                                    onClick={() => go(previousTarget)}
                                    className={cn(ARROW_CLASS, 'sm:left-0')}
                                >
                                    <ChevronLeft className="size-4 sm:size-7" />
                                    <span className="sm:hidden">S{previousTarget.season}E{previousTarget.episode}</span>
                                </Button>
                                :
                                null
                        }
                        {
                            nextTarget !== null ?
                                <Button
                                    variant="ghost"
                                    aria-label="Next episode"
                                    title={nextTarget.label}
                                    onClick={() => go(nextTarget)}
                                    className={cn(ARROW_CLASS, 'ml-auto sm:right-0')}
                                >
                                    <span className="sm:hidden">S{nextTarget.season}E{nextTarget.episode}</span>
                                    <ChevronRight className="size-4 sm:size-7" />
                                </Button>
                                :
                                null
                        }
                    </nav>
                    :
                    null
            }
            {children}
        </div>
    );
};

export default EpisodeArrows;
